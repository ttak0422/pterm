use crate::session::Session;
use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use pterm_proto::{self as proto};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LISTENER: Token = Token(0);
const PTY_BASE: Token = Token(0x1000_0000);
const CLIENT_BASE: Token = Token(0x2000_0000);

/// Per-connection timeout before we send the initial snapshot.
/// This gives the client time to send a RESIZE with its actual window size
/// so the snapshot is generated at the correct dimensions.
const SNAPSHOT_DEFER_MS: u64 = 500;

/// Micro-batching constants for PTY output coalescing.
/// After a read, wait this long before flushing to coalesce nearby writes.
const BATCH_DELAY_MS: u64 = 1;
/// Maximum time from the first read in a burst before we force a flush.
const BATCH_MAX_MS: u64 = 3;
/// If the pending buffer reaches this size, flush immediately.
const BATCH_FLUSH_SIZE: usize = 4096;

struct Client {
    stream: UnixStream,
    recv_buf: Vec<u8>,
    send_buf: Vec<u8>,
    /// `true` until the initial snapshot has been sent.
    pending_snapshot: bool,
    /// Deadline after which we send the snapshot even without a RESIZE.
    pending_snapshot_deadline: Option<Instant>,
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
    /// When the first byte of the current batch arrived.
    batch_start: Option<Instant>,
    /// Deadline by which the current batch must be flushed.
    batch_deadline: Option<Instant>,
    /// `true` after the EXIT message has been broadcast to clients.
    exit_sent: bool,
}

impl Server {
    /// Create a new server. `session_dir` is the directory for this session
    /// (e.g. `/tmp/pterm-1000/mysession/`). The socket file will be created
    /// as `session_dir/socket`.
    pub fn new(session_dir: &Path, session: Session) -> io::Result<Self> {
        std::fs::create_dir_all(session_dir)?;

        let socket_path = session_dir.join("socket");

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
            batch_start: None,
            batch_deadline: None,
            exit_sent: false,
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

            // Dynamic poll timeout: use the nearest deadline among the
            // default 100ms, any pending snapshot deadline, and the batch
            // flush deadline.
            let mut poll_timeout = Duration::from_millis(100);
            if let Some(bd) = self.batch_deadline {
                let remaining = bd.saturating_duration_since(Instant::now());
                poll_timeout = poll_timeout.min(remaining);
            }
            // Also consider snapshot deadlines.
            for c in self.clients.values() {
                if let Some(sd) = c.pending_snapshot_deadline {
                    let remaining = sd.saturating_duration_since(Instant::now());
                    poll_timeout = poll_timeout.min(remaining);
                }
            }

            self.poll.poll(&mut events, Some(poll_timeout))?;

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

            // Flush pending PTY output if the batch deadline has expired.
            if let Some(bd) = self.batch_deadline {
                if Instant::now() >= bd {
                    self.flush_pty_output();
                }
            }

            // Send deferred snapshots for clients whose deadline has expired
            // without receiving a RESIZE (backwards compatibility with plain
            // `pterm attach` from a raw terminal).
            let now = Instant::now();
            let pending_ids: Vec<usize> = self
                .clients
                .iter()
                .filter_map(|(&id, c)| {
                    if c.pending_snapshot {
                        if let Some(deadline) = c.pending_snapshot_deadline {
                            if now >= deadline {
                                return Some(id);
                            }
                        }
                    }
                    None
                })
                .collect();
            if !pending_ids.is_empty() {
                // Flush any pending PTY output before sending snapshots so
                // the vt state is up-to-date.
                self.flush_pty_output();
                for id in pending_ids {
                    log::info!("Client {} snapshot deadline expired, sending snapshot", id);
                    self.send_snapshot_to_client(id);
                }
            }

            if !self.exit_sent {
                if let Some(exit_code) = self.session.check_exit() {
                    // Flush pending output before the EXIT message.
                    self.flush_pty_output();
                    log::info!("Child exited with code {}", exit_code);

                    let msg = proto::encode(proto::server::EXIT, &exit_code.to_le_bytes());
                    for client in self.clients.values_mut() {
                        client.send_buf.extend_from_slice(&msg);
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

                    let deadline =
                        Instant::now() + std::time::Duration::from_millis(SNAPSHOT_DEFER_MS);

                    self.clients.insert(
                        id,
                        Client {
                            stream,
                            recv_buf: Vec::new(),
                            send_buf: Vec::new(),
                            pending_snapshot: true,
                            pending_snapshot_deadline: Some(deadline),
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
    fn send_snapshot_to_client(&mut self, client_id: usize) {
        let snapshot = self.session.snapshot();
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.pending_snapshot = false;
            client.pending_snapshot_deadline = None;
            if !snapshot.is_empty() {
                let msg = proto::encode(proto::server::SCROLLBACK, &snapshot);
                client.send_buf.extend_from_slice(&msg);
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

    fn handle_pty_output(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // Drain all available PTY data into the pending batch buffer.
        let mut read_any = false;
        loop {
            match self.session.read_pty(buf) {
                Ok(0) => break,
                Ok(n) => {
                    read_any = true;
                    self.pending_pty_output.extend_from_slice(&buf[..n]);

                    // Enforce a hard size cap during draining.
                    if self.pending_pty_output.len() >= BATCH_FLUSH_SIZE {
                        self.flush_pty_output();
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    if self.pending_pty_output.is_empty() {
                        log::error!("pty read error: {}", e);
                    }
                    break;
                }
            }
        }

        if self.pending_pty_output.is_empty() {
            return Ok(());
        }

        // Update deadline only when this call actually read new bytes.
        if read_any {
            let now = Instant::now();
            let batch_start = *self.batch_start.get_or_insert(now);
            let max_deadline = batch_start + Duration::from_millis(BATCH_MAX_MS);
            let delay_deadline = now + Duration::from_millis(BATCH_DELAY_MS);
            self.batch_deadline = Some(delay_deadline.min(max_deadline));
        }

        Ok(())
    }

    /// Flush accumulated PTY output to all connected clients and reset batch state.
    fn flush_pty_output(&mut self) {
        if self.pending_pty_output.is_empty() {
            return;
        }

        let msg = proto::encode(proto::server::OUTPUT, &self.pending_pty_output);
        self.pending_pty_output.clear();
        self.batch_start = None;
        self.batch_deadline = None;

        let mut disconnected = Vec::new();
        let mut flush_ids = Vec::new();
        for (&id, client) in self.clients.iter_mut() {
            // A newly attached client must receive a coherent snapshot first.
            // Sending live OUTPUT before that can interleave mid-sequence bytes
            // with snapshot replay and corrupt rendering.
            if client.pending_snapshot {
                continue;
            }
            client.send_buf.extend_from_slice(&msg);
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

            while !client.send_buf.is_empty() {
                match client.stream.write(&client.send_buf) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "client stream closed",
                        ));
                    }
                    Ok(n) => {
                        client.send_buf.drain(..n);
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
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
        let mut offset = 0;
        while offset + proto::HEADER_SIZE <= recv_buf.len() {
            let header: [u8; proto::HEADER_SIZE] = recv_buf[offset..offset + proto::HEADER_SIZE]
                .try_into()
                .unwrap();
            let (msg_type, payload_len) = proto::decode_header(&header);

            let payload_len = payload_len as usize;
            if offset + proto::HEADER_SIZE + payload_len > recv_buf.len() {
                break; // incomplete message, wait for more data
            }

            offset += proto::HEADER_SIZE;
            let payload = &recv_buf[offset..offset + payload_len];
            offset += payload_len;

            match msg_type {
                proto::client::INPUT => {
                    self.session.write_pty(payload)?;
                }
                proto::client::RESIZE => {
                    if payload.len() >= 4 {
                        let r: [u8; 4] = payload[..4].try_into().unwrap();
                        let (cols, rows) = proto::decode_resize(&r);
                        self.session.resize(cols, rows)?;

                        // If this client still has a pending snapshot, the
                        // session has now been resized to the correct
                        // dimensions.  Generate and send the snapshot.
                        let needs_snapshot = self
                            .clients
                            .get(&client_id)
                            .map_or(false, |c| c.pending_snapshot);
                        if needs_snapshot {
                            self.send_snapshot_to_client(client_id);
                        }
                    }
                }
                proto::client::DETACH => {}
                proto::client::REDRAW => {
                    log::info!("Redraw requested by client {}", client_id);
                    let mut redraw_data = b"\x1b[2J\x1b[H".to_vec();
                    redraw_data.extend_from_slice(&self.session.snapshot());
                    let msg = proto::encode(proto::server::SCROLLBACK, &redraw_data);
                    for (_, client) in self.clients.iter_mut() {
                        client.send_buf.extend_from_slice(&msg);
                    }
                    flush_all = true;
                }
                _ => log::warn!("Unknown message type: 0x{:02x}", msg_type),
            }
        }

        // Put remaining bytes back into the client's buffer
        if offset > 0 {
            recv_buf.drain(..offset);
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.recv_buf = recv_buf;
        }
        Ok(flush_all)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
