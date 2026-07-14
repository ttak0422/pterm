use crate::session::Session;
use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use pterm_proto::{self as proto};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LISTENER: Token = Token(0);
const PTY_BASE: Token = Token(0x1000_0000);
const CLIENT_BASE: Token = Token(0x2000_0000);
const DA1_RESPONSE: &[u8] = b"\x1b[?62;22c"; // Primary Device Attributes (DA1)
const DA2_RESPONSE: &[u8] = b"\x1b[>1;10;0c"; // Secondary Device Attributes (DA2)
const DA_QUERY_WARN_THRESHOLD: usize = 2;
const LARGE_SEND_BUF_WARN_BYTES: usize = 64 * 1024;
/// How often the daemon re-reads the child shell's working directory and
/// refreshes the `cwd` file so session lists can follow `cd`.
const CWD_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

struct Client {
    stream: UnixStream,
    recv_buf: Vec<u8>,
    send_buf: SendQueue,
    large_send_buf_warned: bool,
    /// `true` until the initial snapshot has been sent.
    pending_snapshot: bool,
    /// One-shot diagnostic clients should not receive normal terminal output.
    diagnostic: bool,
    /// Protocol version from the client HELLO. 0 = no HELLO received
    /// (pre-handshake client), which keeps the legacy behavior.
    proto: u32,
    /// Client requested scrollback history replay (HELLO flag). History is
    /// sent once, alongside the initial snapshot.
    wants_history: bool,
    /// HELLO received but HELLO_ACK not queued yet. The ACK is queued ahead
    /// of the first STATE_SYNC so the bridge can use STATE_SYNC arrival as
    /// the "no ACK means old daemon" anchor.
    hello_ack_pending: bool,
}

/// Outbound frame queue for a client.
///
/// The front frame may already be partially written to the socket. Discarding
/// its unwritten remainder would leave a truncated frame on the wire and
/// permanently desync the client's decoder, so queue replacement
/// (`clear_unsent`) always preserves it.
#[derive(Default)]
struct SendQueue {
    frames: VecDeque<Vec<u8>>,
    /// Bytes of the front frame already written to the socket.
    front_written: usize,
}

impl SendQueue {
    fn push(&mut self, frame: &[u8]) {
        self.frames.push_back(frame.to_vec());
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Total bytes still waiting to go out on the wire.
    fn pending_bytes(&self) -> usize {
        self.frames.iter().map(Vec::len).sum::<usize>() - self.front_written
    }

    /// Drop every queued frame that has not started going out on the wire.
    /// The partially written front frame is kept so framing stays intact.
    fn clear_unsent(&mut self) {
        if self.front_written > 0 {
            self.frames.truncate(1);
        } else {
            self.frames.clear();
        }
    }

    /// Write as much as the stream accepts, stopping on `WouldBlock`.
    fn write_to(&mut self, stream: &mut UnixStream) -> io::Result<()> {
        while let Some(front) = self.frames.front() {
            match stream.write(&front[self.front_written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "client stream closed",
                    ));
                }
                Ok(n) => {
                    self.front_written += n;
                    if self.front_written == front.len() {
                        self.frames.pop_front();
                        self.front_written = 0;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

pub struct Server {
    socket_path: PathBuf,
    session: Session,
    poll: Poll,
    listener: UnixListener,
    clients: HashMap<usize, Client>,
    next_client_id: usize,
    /// Accumulated PTY output waiting to be flushed.
    pending_pty_output: Vec<u8>,
    /// `true` after the EXIT message has been broadcast to clients.
    exit_sent: bool,
    /// Path of the `cwd` file recording the session's working directory.
    cwd_path: PathBuf,
    /// Last cwd written to `cwd_path`, to skip redundant writes.
    last_cwd: Option<String>,
    /// When the cwd was last refreshed from the child process.
    last_cwd_refresh: Instant,
}

impl Server {
    /// Create a new server. `session_dir` is the directory for this session
    /// (e.g. `/tmp/pterm-1000/mysession/`). The socket file will be created
    /// as `session_dir/socket`.
    pub fn new(session_dir: &Path, session: Session) -> io::Result<Self> {
        std::fs::create_dir_all(session_dir)?;

        let socket_path = session_dir.join("socket");
        let cwd_path = session_dir.join(crate::paths::CWD_FILENAME);
        let last_cwd = std::fs::read_to_string(&cwd_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }

        let mut listener = UnixListener::bind(&socket_path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o700))?;
        }

        let poll = Poll::new()?;
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;

        let pty_fd = session.master_fd();
        let mut source_fd = mio::unix::SourceFd(&pty_fd);
        poll.registry()
            .register(&mut source_fd, PTY_BASE, Interest::READABLE)?;

        Ok(Self {
            socket_path,
            session,
            poll,
            listener,
            clients: HashMap::new(),
            next_client_id: 0,
            pending_pty_output: Vec::new(),
            exit_sent: false,
            cwd_path,
            last_cwd,
            last_cwd_refresh: Instant::now(),
        })
    }

    pub fn run(&mut self) -> io::Result<()> {
        let mut events = Events::with_capacity(64);
        let mut pty_buf = vec![0u8; 65536];
        let mut client_buf = vec![0u8; 65536];

        log::info!(
            "Server running for session '{}' at {:?}",
            self.session.name,
            self.socket_path
        );

        loop {
            // If the socket path disappears (or is replaced with a non-socket),
            // treat the session as deleted and shut down.
            match std::fs::symlink_metadata(&self.socket_path) {
                Ok(meta) if meta.file_type().is_socket() => {}
                _ => {
                    log::warn!(
                        "Socket path '{}' is missing; shutting down session '{}'",
                        self.socket_path.display(),
                        self.session.name
                    );
                    break;
                }
            }

            self.poll
                .poll(&mut events, Some(Duration::from_millis(100)))?;

            self.refresh_cwd();

            for event in events.iter() {
                match event.token() {
                    LISTENER => {
                        if let Err(e) = self.accept_client() {
                            log::warn!("Failed to accept client: {}", e);
                        }
                    }
                    token if token.0 >= CLIENT_BASE.0 => {
                        let id = token.0 - CLIENT_BASE.0;
                        if event.is_readable() {
                            if let Err(e) = self.handle_client_data(id, &mut client_buf) {
                                log::warn!("Client {} read error: {}", id, e);
                                self.clients.remove(&id);
                            }
                        }
                        if event.is_writable() {
                            if let Err(e) = self.flush_client_send_buf(id) {
                                log::warn!("Client {} write error: {}", id, e);
                                self.clients.remove(&id);
                            }
                        }
                    }
                    PTY_BASE => self.handle_pty_output(&mut pty_buf)?,
                    _ => {}
                }
            }

            // No timer-based snapshot deferral. Snapshots are sent either:
            // 1. When the client sends RESIZE (handled in process_client_recv_buf)
            // 2. When PTY OUTPUT arrives for a client still awaiting snapshot
            //    (handled in flush_pty_output)

            if !self.exit_sent {
                if let Some(exit_code) = self.session.check_exit() {
                    // Flush pending output before the EXIT message.
                    self.flush_pty_output();
                    log::info!("Child exited with code {}", exit_code);

                    let msg = proto::encode(proto::server::EXIT, &exit_code.to_le_bytes());
                    for client in self.clients.values_mut() {
                        client.send_buf.push(&msg);
                    }
                    self.flush_all_clients();
                    self.exit_sent = true;

                    if self.clients.is_empty() {
                        break;
                    }
                }
            }

            if self.session.exited.is_some() && self.clients.is_empty() {
                break;
            }
        }

        let _ = std::fs::remove_file(&self.socket_path);
        log::info!("Server shut down for session '{}'", self.session.name);
        Ok(())
    }

    /// Re-read the child shell's working directory and update the `cwd` file
    /// when it changes, so session lists reflect `cd` inside the session.
    /// Throttled to `CWD_REFRESH_INTERVAL`.
    fn refresh_cwd(&mut self) {
        if self.last_cwd_refresh.elapsed() < CWD_REFRESH_INTERVAL {
            return;
        }
        self.last_cwd_refresh = Instant::now();

        let pid = self.session.pty.child_pid.as_raw();
        let cwd = match crate::paths::process_cwd(pid) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => return,
        };
        if self.last_cwd.as_deref() == Some(cwd.as_str()) {
            return;
        }
        if std::fs::write(&self.cwd_path, cwd.as_bytes()).is_ok() {
            self.last_cwd = Some(cwd);
        }
    }

    fn accept_client(&mut self) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let id = self.next_client_id;
                    self.next_client_id += 1;

                    let token = Token(CLIENT_BASE.0 + id);
                    self.poll
                        .registry()
                        .register(&mut stream, token, Interest::READABLE)?;

                    log::info!("Client {} connected to '{}'", id, self.session.name);

                    self.clients.insert(
                        id,
                        Client {
                            stream,
                            recv_buf: Vec::new(),
                            send_buf: SendQueue::default(),
                            large_send_buf_warned: false,
                            pending_snapshot: true,
                            diagnostic: false,
                            proto: 0,
                            wants_history: false,
                            hello_ack_pending: false,
                        },
                    );
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Send the current terminal snapshot to a specific client and clear its
    /// pending-snapshot flag.
    ///
    /// When `replace_send_buf` is `true`, any queued outbound bytes for that
    /// client are dropped first. This is used on RESIZE so an older-size
    /// snapshot or stale OUTPUT frame cannot remain queued ahead of the fresh
    /// snapshot for the client's new dimensions.
    fn send_snapshot_to_client(&mut self, client_id: usize, replace_send_buf: bool) {
        // Diagnostic query clients (dump/snapshot-text/full-text) await
        // exactly one response frame; a snapshot would only add noise, and
        // with replace_send_buf a RESIZE from another client would drop
        // their still-unsent response outright.
        if self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.diagnostic)
        {
            return;
        }

        let buffered_pty_bytes = self.pending_pty_output.len();
        let other_pending_snapshots = self
            .clients
            .iter()
            .filter(|&(id, client)| *id != client_id && client.pending_snapshot)
            .count();
        let pending_send_clients = self
            .clients
            .values()
            .filter(|client| !client.send_buf.is_empty())
            .count();

        if buffered_pty_bytes > 0 || other_pending_snapshots > 0 || pending_send_clients > 1 {
            log::debug!(
                "Client {} snapshot sent while {} PTY byte(s) are buffered, {} other client(s) still await snapshot, and {} client(s) have queued output",
                client_id,
                buffered_pty_bytes,
                other_pending_snapshots,
                pending_send_clients
            );
        }

        // History replay happens once, on the initial snapshot only. REDRAW
        // and later RESIZE re-snapshots must not repeat it: auto_redraw runs
        // on every BufEnter, and repeating history would duplicate it in the
        // client's scrollback without bound.
        let send_history = self.clients.get(&client_id).is_some_and(|client| {
            client.pending_snapshot && client.wants_history && !client.diagnostic
        });
        let history = if send_history {
            self.session.history_formatted(history_replay_limit())
        } else {
            Vec::new()
        };

        let snapshot = self.session.snapshot();
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.pending_snapshot = false;
            if replace_send_buf {
                client.send_buf.clear_unsent();
            }
            // (Re-)queue the HELLO_ACK ahead of the STATE_SYNC. On
            // replace_send_buf a previously queued but unsent ACK may just
            // have been dropped, so re-send for any handshaked client;
            // duplicate ACKs are idempotent on the bridge side.
            if client.hello_ack_pending || (replace_send_buf && client.proto != 0) {
                client.send_buf.push(&hello_ack_message());
                client.hello_ack_pending = false;
            }
            if !history.is_empty() {
                let msg = proto::encode(proto::server::HISTORY, &history);
                client.send_buf.push(&msg);
            }
            if !snapshot.is_empty() {
                let msg = proto::encode(proto::server::STATE_SYNC, &snapshot);
                client.send_buf.push(&msg);
            }
        }
        if let Err(e) = self.flush_client_send_buf(client_id) {
            log::warn!(
                "Client {} flush error during snapshot send: {}",
                client_id,
                e
            );
        }
    }

    fn send_snapshot_to_all_clients(&mut self, replace_send_buf: bool) {
        let client_ids: Vec<usize> = self.clients.keys().copied().collect();
        for client_id in client_ids {
            self.send_snapshot_to_client(client_id, replace_send_buf);
        }
    }

    fn handle_pty_output(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.drain_pty_output(buf)?;

        if !self.pending_pty_output.is_empty() {
            self.flush_pty_output();
        }

        Ok(())
    }

    fn drain_pty_output(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // Drain all available PTY data (non-blocking). No timer-based batching
        // -- the drain loop itself coalesces all bytes available right now.
        loop {
            match self.session.read_pty(buf, &mut self.pending_pty_output) {
                Ok(0) => break,
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    if self.pending_pty_output.is_empty() {
                        log::error!("pty read error: {}", e);
                    }
                    break;
                }
            }
        }

        let (pending_da1, pending_da2) = self.session.take_pending_da_queries();
        let total_pending_da = pending_da1 + pending_da2;
        if !self.clients.is_empty()
            && (total_pending_da > DA_QUERY_WARN_THRESHOLD
                || (self.clients.len() > 1 && total_pending_da > 0))
        {
            log::warn!(
                "Observed pending device-attribute queries while {} client(s) are attached (DA1={}, DA2={})",
                self.clients.len(),
                pending_da1,
                pending_da2
            );
        }
        if total_pending_da > 0 {
            match self.session.echo_enabled() {
                Ok(false) => {
                    for _ in 0..pending_da1 {
                        if let Err(e) = self.session.write_pty(DA1_RESPONSE) {
                            log::warn!("Failed to write DA1 response to PTY: {}", e);
                            break;
                        }
                    }
                    for _ in 0..pending_da2 {
                        if let Err(e) = self.session.write_pty(DA2_RESPONSE) {
                            log::warn!("Failed to write DA2 response to PTY: {}", e);
                            break;
                        }
                    }
                }
                Ok(true) => {
                    log::debug!(
                        "Dropping {} DA query response(s) while PTY ECHO is enabled",
                        total_pending_da
                    );
                }
                Err(e) => {
                    log::warn!(
                        "Dropping {} DA query response(s) after failing to read PTY ECHO state: {}",
                        total_pending_da,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Flush accumulated PTY output to all connected clients.
    /// Clients still awaiting a snapshot receive the snapshot first (triggered
    /// by the arrival of OUTPUT rather than a timer).
    fn flush_pty_output(&mut self) {
        if self.pending_pty_output.is_empty() {
            return;
        }

        // Clients awaiting snapshot: the arrival of OUTPUT means the VT state
        // is populated, so send their snapshot now (no timer needed).
        // These clients must NOT also receive the raw OUTPUT bytes below,
        // because the snapshot already reflects the effect of those bytes
        // (read_pty feeds data to the VT parser before this method runs).
        // Sending both would cause Neovim's libvterm to process the same
        // content twice, resulting in duplicated rendering.
        let snapshot_ids: Vec<usize> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                if c.pending_snapshot && !c.diagnostic {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        for id in &snapshot_ids {
            log::info!("Client {} snapshot triggered by PTY output arrival", *id);
            self.send_snapshot_to_client(*id, true);
        }

        let msg = proto::encode(proto::server::OUTPUT, &self.pending_pty_output);
        self.pending_pty_output.clear();

        let mut disconnected = Vec::new();
        let mut flush_ids = Vec::new();
        for (&id, client) in self.clients.iter_mut() {
            if client.diagnostic {
                continue;
            }
            // Skip clients that just received a snapshot — they already have
            // the up-to-date screen state and must not get the raw bytes again.
            if snapshot_ids.contains(&id) {
                continue;
            }
            client.send_buf.push(&msg);
            flush_ids.push(id);
        }
        for id in flush_ids {
            if self.flush_client_send_buf(id).is_err() {
                disconnected.push(id);
            }
        }
        for id in disconnected {
            log::info!("Client {} disconnected", id);
            self.clients.remove(&id);
        }
    }

    fn set_client_interest(&mut self, client_id: usize, writable: bool) -> io::Result<()> {
        let client = match self.clients.get_mut(&client_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        let token = Token(CLIENT_BASE.0 + client_id);
        let interest = if writable {
            Interest::READABLE.add(Interest::WRITABLE)
        } else {
            Interest::READABLE
        };
        self.poll
            .registry()
            .reregister(&mut client.stream, token, interest)?;
        Ok(())
    }

    fn flush_client_send_buf(&mut self, client_id: usize) -> io::Result<()> {
        let writable = {
            let client = match self.clients.get_mut(&client_id) {
                Some(c) => c,
                None => return Ok(()),
            };

            client.send_buf.write_to(&mut client.stream)?;

            let pending_bytes = client.send_buf.pending_bytes();
            if pending_bytes >= LARGE_SEND_BUF_WARN_BYTES {
                if !client.large_send_buf_warned {
                    log::warn!(
                        "Client {} send buffer backlog reached {} bytes",
                        client_id,
                        pending_bytes
                    );
                    client.large_send_buf_warned = true;
                }
            } else {
                client.large_send_buf_warned = false;
            }

            !client.send_buf.is_empty()
        };

        self.set_client_interest(client_id, writable)
    }

    fn flush_all_clients(&mut self) {
        let ids: Vec<usize> = self.clients.keys().copied().collect();
        let mut disconnected = Vec::new();
        for id in ids {
            if self.flush_client_send_buf(id).is_err() {
                disconnected.push(id);
            }
        }
        for id in disconnected {
            log::info!("Client {} disconnected during flush", id);
            self.clients.remove(&id);
        }
    }

    fn handle_client_data(&mut self, client_id: usize, buf: &mut [u8]) -> io::Result<()> {
        let remove = {
            let client = match self.clients.get_mut(&client_id) {
                Some(c) => c,
                None => return Ok(()),
            };
            match client.stream.read(buf) {
                Ok(0) => true,
                Ok(n) => {
                    client.recv_buf.extend_from_slice(&buf[..n]);
                    false
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(_) => true,
            }
        };

        if remove {
            log::info!("Client {} disconnected", client_id);
            self.clients.remove(&client_id);
        } else if let Some(client) = self.clients.get_mut(&client_id) {
            if !client.recv_buf.is_empty() {
                // Flush pending PTY output so the vt state is current before
                // processing client messages (e.g. REDRAW, RESIZE snapshots).
                self.flush_pty_output();
                let needs_flush = self.process_client_recv_buf(client_id)?;
                if needs_flush {
                    self.flush_all_clients();
                }
            }
        }
        Ok(())
    }

    fn process_client_recv_buf(&mut self, client_id: usize) -> io::Result<bool> {
        // Take the buffer out to avoid borrowing self.clients while using self.session
        let mut recv_buf = match self.clients.get_mut(&client_id) {
            Some(c) => std::mem::take(&mut c.recv_buf),
            None => return Ok(false),
        };

        let mut flush_all = false;
        for frame in proto::decode_frames(&mut recv_buf) {
            match frame.msg_type {
                proto::client::HELLO => match proto::parse_hello(&frame.payload) {
                    Ok((version, flags)) => {
                        log::info!(
                            "Client {} hello: proto v{}, flags 0x{:x}",
                            client_id,
                            version,
                            flags
                        );
                        if version != proto::PROTO_VERSION {
                            log::warn!(
                                "Client {} protocol v{} differs from daemon v{}",
                                client_id,
                                version,
                                proto::PROTO_VERSION
                            );
                        }
                        if let Some(client) = self.clients.get_mut(&client_id) {
                            client.proto = version;
                            client.wants_history = flags & proto::hello_flags::REQUEST_HISTORY != 0;
                            client.hello_ack_pending = true;
                        }
                    }
                    Err(e) => {
                        log::warn!("Client {} sent invalid hello payload: {}", client_id, e);
                    }
                },
                proto::client::INPUT => {
                    self.session.write_pty(&frame.payload)?;
                }
                proto::client::RESIZE => {
                    let (cols, rows) = match proto::parse_resize(&frame.payload) {
                        Ok(size) => size,
                        Err(e) => {
                            log::warn!("Client {} sent invalid resize payload: {}", client_id, e);
                            continue;
                        }
                    };
                    self.session.resize(cols, rows)?;

                    // The latest RESIZE is authoritative for every attached
                    // client. Replacing all outbound queues prevents stale-size
                    // frames from surviving ahead of the fresh snapshot.
                    self.send_snapshot_to_all_clients(true);
                }
                proto::client::DETACH => {}
                proto::client::REDRAW => {
                    log::info!("Redraw requested by client {}", client_id);
                    let mut redraw_data = b"\x1b[2J\x1b[H".to_vec();
                    redraw_data.extend_from_slice(&self.session.snapshot());
                    let msg = proto::encode(proto::server::STATE_SYNC, &redraw_data);
                    for (_, client) in self.clients.iter_mut() {
                        // The bridge anchors "old daemon" detection on the
                        // first STATE_SYNC, so a pending ACK must precede it.
                        if client.hello_ack_pending {
                            client.send_buf.push(&hello_ack_message());
                            client.hello_ack_pending = false;
                        }
                        client.send_buf.push(&msg);
                    }
                    flush_all = true;
                }
                proto::client::DUMP => {
                    log::info!("Dump requested by client {}", client_id);
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.diagnostic = true;
                        client.pending_snapshot = false;
                        client.send_buf.clear_unsent();
                    }

                    let mut pty_buf = vec![0u8; 65536];
                    self.drain_pty_output(&mut pty_buf)?;
                    if !self.pending_pty_output.is_empty() {
                        self.flush_pty_output();
                    }

                    let payload = serde_json::to_vec_pretty(&self.session.dump())
                        .map_err(io::Error::other)?;
                    let msg = proto::encode(proto::server::DUMP, &payload);
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.send_buf.clear_unsent();
                        client.send_buf.push(&msg);
                    }
                    flush_all = true;
                }
                proto::client::SNAPSHOT_TEXT => {
                    log::info!("Snapshot text requested by client {}", client_id);
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.diagnostic = true;
                        client.pending_snapshot = false;
                        client.send_buf.clear_unsent();
                    }

                    let mut pty_buf = vec![0u8; 65536];
                    self.drain_pty_output(&mut pty_buf)?;
                    if !self.pending_pty_output.is_empty() {
                        self.flush_pty_output();
                    }

                    let payload = self.session.snapshot_text();
                    let msg = proto::encode(proto::server::SNAPSHOT_TEXT, payload.as_bytes());
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.send_buf.clear_unsent();
                        client.send_buf.push(&msg);
                    }
                    flush_all = true;
                }
                proto::client::FULL_TEXT => {
                    log::info!("Full text requested by client {}", client_id);
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.diagnostic = true;
                        client.pending_snapshot = false;
                        client.send_buf.clear_unsent();
                    }

                    let mut pty_buf = vec![0u8; 65536];
                    self.drain_pty_output(&mut pty_buf)?;
                    if !self.pending_pty_output.is_empty() {
                        self.flush_pty_output();
                    }

                    let payload = self.session.full_text();
                    let msg = proto::encode(proto::server::FULL_TEXT, payload.as_bytes());
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.send_buf.clear_unsent();
                        client.send_buf.push(&msg);
                    }
                    flush_all = true;
                }
                _ => log::warn!("Unknown message type: 0x{:02x}", frame.msg_type),
            }
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.recv_buf = recv_buf;
        }
        Ok(flush_all)
    }
}

fn hello_ack_message() -> Vec<u8> {
    proto::encode(
        proto::server::HELLO_ACK,
        &proto::encode_hello_ack(proto::PROTO_VERSION, env!("CARGO_PKG_VERSION")),
    )
}

/// Maximum number of scrollback lines replayed on attach. Defaults to
/// unlimited (bounded in practice by the parser's scrollback capacity).
fn history_replay_limit() -> usize {
    std::env::var("PTERM_HISTORY_REPLAY_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX)
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    /// Regression test: a RESIZE arriving while a client's outbound frame is
    /// only partially written to the socket must not corrupt the frame stream.
    ///
    /// The daemon replaces each client's send queue on RESIZE so stale-size
    /// frames don't precede the fresh snapshot. If that replacement discards
    /// the unwritten remainder of a frame whose header is already on the wire,
    /// the client-side decoder desyncs permanently and the terminal freezes
    /// (observed as "scrolling stops working after splitting the window").
    #[test]
    fn resize_during_output_backlog_keeps_frame_stream_valid() {
        let dir = std::env::temp_dir().join(format!("pterm-server-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Emit ~2MiB quickly, then idle. 'A' (0x41) is not a valid frame type,
        // so any decoder misalignment is detected below.
        let session = Session::new(
            "corruption-test".to_string(),
            "/bin/sh",
            &[
                "sh",
                "-c",
                "dd if=/dev/zero bs=1024 count=2048 2>/dev/null | tr '\\0' 'A'; sleep 30",
            ],
        )
        .expect("failed to spawn test session");

        let mut server = Server::new(&dir, session).expect("failed to create server");
        let socket_path = dir.join("socket");
        let server_thread = std::thread::spawn(move || {
            let _ = server.run();
        });

        let mut client = std::os::unix::net::UnixStream::connect(&socket_path)
            .expect("failed to connect to daemon socket");
        let resize =
            |cols, rows| proto::encode(proto::client::RESIZE, &proto::encode_resize(cols, rows));
        client.write_all(&resize(80, 24)).unwrap();

        // Do not read: the socket send buffer fills and the daemon is left
        // with a partially written OUTPUT frame at the front of its queue.
        std::thread::sleep(Duration::from_millis(300));
        client.write_all(&resize(100, 30)).unwrap();

        // Drain and validate the whole stream.
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut stream_bytes = Vec::new();
        let mut chunk = [0u8; 65536];
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut consecutive_timeouts = 0;
        while Instant::now() < deadline && consecutive_timeouts < 2 {
            match client.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    stream_bytes.extend_from_slice(&chunk[..n]);
                    consecutive_timeouts = 0;
                }
                Err(_) => consecutive_timeouts += 1,
            }
        }

        // Walk the byte stream frame by frame. Every complete header must
        // carry a known server frame type; anything else means the stream
        // desynced.
        let mut offset = 0;
        let mut state_syncs = 0;
        while offset + proto::HEADER_SIZE <= stream_bytes.len() {
            let header: [u8; proto::HEADER_SIZE] = stream_bytes
                [offset..offset + proto::HEADER_SIZE]
                .try_into()
                .unwrap();
            let (msg_type, payload_len) = proto::decode_header(&header);
            assert!(
                matches!(
                    msg_type,
                    proto::server::OUTPUT | proto::server::EXIT | proto::server::STATE_SYNC
                ),
                "frame stream corrupted: unknown frame type 0x{:02x} at offset {} of {} bytes",
                msg_type,
                offset,
                stream_bytes.len()
            );
            if msg_type == proto::server::STATE_SYNC {
                state_syncs += 1;
            }
            let end = offset + proto::HEADER_SIZE + payload_len as usize;
            if end > stream_bytes.len() {
                break; // trailing partial frame is fine
            }
            offset = end;
        }

        // The second RESIZE must yield a decodable snapshot. If the resize
        // snapshot was swallowed by a truncated frame, only the initial
        // snapshot is observed.
        assert!(
            state_syncs >= 2,
            "expected snapshots for both resizes, decoded {} STATE_SYNC frame(s)",
            state_syncs
        );

        // Removing the socket makes the server loop shut down.
        let _ = std::fs::remove_file(&socket_path);
        let _ = server_thread.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression test: a RESIZE from an attached client must not clear a
    /// diagnostic client's queued-but-unsent query response.
    ///
    /// send_snapshot_to_all_clients(true) replaces every client's send queue;
    /// without the diagnostic filter it also dropped a FULL_TEXT/DUMP/
    /// SNAPSHOT_TEXT response still queued behind an output backlog, so the
    /// querying CLI timed out and the session silently vanished from
    /// `:Telescope pterm grep` results.
    #[test]
    fn resize_does_not_drop_queued_diagnostic_response() {
        let dir =
            std::env::temp_dir().join(format!("pterm-diag-resize-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Emit ~2MiB quickly so the query client accumulates an unread
        // OUTPUT backlog between connecting and its request being processed.
        let session = Session::new(
            "diag-resize-test".to_string(),
            "/bin/sh",
            &[
                "sh",
                "-c",
                "dd if=/dev/zero bs=1024 count=2048 2>/dev/null | tr '\\0' 'A'; sleep 30",
            ],
        )
        .expect("failed to spawn test session");

        let mut server = Server::new(&dir, session).expect("failed to create server");
        let socket_path = dir.join("socket");
        let server_thread = std::thread::spawn(move || {
            let _ = server.run();
        });

        let mut attached = std::os::unix::net::UnixStream::connect(&socket_path)
            .expect("failed to connect attached client");
        let resize =
            |cols, rows| proto::encode(proto::client::RESIZE, &proto::encode_resize(cols, rows));
        attached.write_all(&resize(80, 24)).unwrap();

        let mut query = std::os::unix::net::UnixStream::connect(&socket_path)
            .expect("failed to connect query client");
        query
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        // Let OUTPUT frames pile up unread in the query client's socket
        // buffer, request the full text, then resize from the attached
        // client while the response is still queued behind the backlog.
        std::thread::sleep(Duration::from_millis(300));
        query
            .write_all(&proto::encode(proto::client::FULL_TEXT, &[]))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        attached.write_all(&resize(100, 30)).unwrap();

        let mut recv = Vec::new();
        let frames = read_frames_until(&mut query, &mut recv, |f| {
            f.msg_type == proto::server::FULL_TEXT
        });
        assert!(
            frames
                .iter()
                .any(|f| f.msg_type == proto::server::FULL_TEXT),
            "queued FULL_TEXT response was dropped by RESIZE"
        );

        let _ = std::fs::remove_file(&socket_path);
        let _ = server_thread.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read frames from `client` until one matches `stop` (or a timeout
    /// expires), returning everything received.
    fn read_frames_until(
        client: &mut std::os::unix::net::UnixStream,
        recv: &mut Vec<u8>,
        stop: impl Fn(&proto::Frame) -> bool,
    ) -> Vec<proto::Frame> {
        let mut frames = Vec::new();
        let mut chunk = [0u8; 65536];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            for frame in proto::decode_frames(recv) {
                let done = stop(&frame);
                frames.push(frame);
                if done {
                    return frames;
                }
            }
            match client.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => recv.extend_from_slice(&chunk[..n]),
                Err(ref e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }
        frames
    }

    #[test]
    fn history_replay_sent_once_before_initial_snapshot() {
        let dir = std::env::temp_dir().join(format!("pterm-history-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // 200 lines on the default 80x24 screen leave ~176 lines of scrollback.
        let session = Session::new(
            "history-test".to_string(),
            "/bin/sh",
            &["sh", "-c", "seq 1 200; sleep 30"],
        )
        .expect("failed to spawn test session");

        let mut server = Server::new(&dir, session).expect("failed to create server");
        let socket_path = dir.join("socket");
        let server_thread = std::thread::spawn(move || {
            let _ = server.run();
        });

        // Let the daemon drain the child's output into the VT parser before
        // any client attaches.
        std::thread::sleep(Duration::from_millis(500));

        let mut client = std::os::unix::net::UnixStream::connect(&socket_path)
            .expect("failed to connect to daemon socket");
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        let hello = proto::encode_hello(proto::PROTO_VERSION, proto::hello_flags::REQUEST_HISTORY);
        let mut msg = proto::encode(proto::client::HELLO, &hello);
        msg.extend_from_slice(&proto::encode(
            proto::client::RESIZE,
            &proto::encode_resize(80, 24),
        ));
        client.write_all(&msg).unwrap();

        let mut recv = Vec::new();
        let frames = read_frames_until(&mut client, &mut recv, |f| {
            f.msg_type == proto::server::STATE_SYNC
        });

        let type_order: Vec<u8> = frames.iter().map(|f| f.msg_type).collect();
        let ack_pos = type_order
            .iter()
            .position(|&t| t == proto::server::HELLO_ACK)
            .expect("HELLO_ACK should arrive before the first STATE_SYNC");
        let history_pos = type_order
            .iter()
            .position(|&t| t == proto::server::HISTORY)
            .expect("HISTORY should arrive before the first STATE_SYNC");
        let sync_pos = type_order
            .iter()
            .position(|&t| t == proto::server::STATE_SYNC)
            .expect("STATE_SYNC should arrive");
        assert!(ack_pos < sync_pos, "HELLO_ACK must precede STATE_SYNC");
        assert!(history_pos < sync_pos, "HISTORY must precede STATE_SYNC");

        let history = &frames[history_pos].payload;
        let history_str = String::from_utf8_lossy(history);
        assert!(
            history_str.contains("100"),
            "history should contain scrolled-off line 100"
        );

        // REDRAW and a later RESIZE must not repeat the history.
        client
            .write_all(&proto::encode(proto::client::REDRAW, &[]))
            .unwrap();
        let frames = read_frames_until(&mut client, &mut recv, |f| {
            f.msg_type == proto::server::STATE_SYNC
        });
        assert!(
            frames.iter().all(|f| f.msg_type != proto::server::HISTORY),
            "REDRAW must not resend history"
        );

        client
            .write_all(&proto::encode(
                proto::client::RESIZE,
                &proto::encode_resize(100, 30),
            ))
            .unwrap();
        let frames = read_frames_until(&mut client, &mut recv, |f| {
            f.msg_type == proto::server::STATE_SYNC
        });
        assert!(
            frames.iter().all(|f| f.msg_type != proto::server::HISTORY),
            "RESIZE must not resend history"
        );

        // A client that never sends HELLO gets neither ACK nor history.
        let mut legacy = std::os::unix::net::UnixStream::connect(&socket_path)
            .expect("failed to connect legacy client");
        legacy
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        legacy
            .write_all(&proto::encode(
                proto::client::RESIZE,
                &proto::encode_resize(80, 24),
            ))
            .unwrap();
        let mut legacy_recv = Vec::new();
        let frames = read_frames_until(&mut legacy, &mut legacy_recv, |f| {
            f.msg_type == proto::server::STATE_SYNC
        });
        assert!(frames
            .iter()
            .any(|f| f.msg_type == proto::server::STATE_SYNC));
        assert!(
            frames
                .iter()
                .all(|f| f.msg_type != proto::server::HISTORY
                    && f.msg_type != proto::server::HELLO_ACK),
            "legacy client must not receive HISTORY or HELLO_ACK"
        );

        let _ = std::fs::remove_file(&socket_path);
        let _ = server_thread.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
