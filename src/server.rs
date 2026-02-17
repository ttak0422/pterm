use crate::session::Session;
use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use pterm_proto::{self as proto};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

const LISTENER: Token = Token(0);
const PTY_BASE: Token = Token(0x1000_0000);
const CLIENT_BASE: Token = Token(0x2000_0000);

struct Client {
    stream: UnixStream,
    recv_buf: Vec<u8>,
    send_buf: Vec<u8>,
}

/// Remove terminal query escape sequences from scrollback replay.
///
/// Replaying queries like `CSI 6n` can make terminal responses appear as stray
/// input (for example `09;5R`). We only sanitize scrollback replay; live PTY
/// output is forwarded unchanged.
fn sanitize_scrollback_for_replay(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            let mut j = i + 2;
            while j < data.len() {
                let b = data[j];
                // CSI final byte range
                if (0x40..=0x7e).contains(&b) {
                    let final_byte = b;
                    let params = &data[i + 2..j];
                    let is_status_query = final_byte == b'n';
                    let is_device_attr_query = final_byte == b'c'
                        && (params.is_empty() || params[0] == b'>' || params[0] == b'?');
                    if !(is_status_query || is_device_attr_query) {
                        out.extend_from_slice(&data[i..=j]);
                    }
                    i = j + 1;
                    break;
                }
                j += 1;
            }
            if j >= data.len() {
                out.extend_from_slice(&data[i..]);
                break;
            }
            continue;
        }

        out.push(data[i]);
        i += 1;
    }

    out
}

pub struct Server {
    socket_path: PathBuf,
    session: Session,
    poll: Poll,
    listener: UnixListener,
    clients: HashMap<usize, Client>,
    next_client_id: usize,
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
                .poll(&mut events, Some(std::time::Duration::from_millis(100)))?;

            for event in events.iter() {
                match event.token() {
                    LISTENER => self.accept_client()?,
                    token if token.0 >= CLIENT_BASE.0 => {
                        let id = token.0 - CLIENT_BASE.0;
                        if event.is_readable() {
                            self.handle_client_data(id, &mut client_buf)?;
                        }
                        if event.is_writable() {
                            self.flush_client_send_buf(id)?;
                        }
                    }
                    PTY_BASE => self.handle_pty_output(&mut pty_buf)?,
                    _ => {}
                }
            }

            if let Some(exit_code) = self.session.check_exit() {
                log::info!("Child exited with code {}", exit_code);
                let msg = proto::encode(proto::server::EXIT, &exit_code.to_le_bytes());
                for client in self.clients.values_mut() {
                    let _ = client.stream.write_all(&msg);
                }
                if self.clients.is_empty() {
                    break;
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

                    let scrollback =
                        sanitize_scrollback_for_replay(&self.session.scrollback.get_contents());

                    self.clients.insert(
                        id,
                        Client {
                            stream,
                            recv_buf: Vec::new(),
                            send_buf: Vec::new(),
                        },
                    );

                    if !scrollback.is_empty() {
                        let msg = proto::encode(proto::server::SCROLLBACK, &scrollback);
                        if let Some(client) = self.clients.get_mut(&id) {
                            client.send_buf.extend_from_slice(&msg);
                        }
                        self.flush_client_send_buf(id)?;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn handle_pty_output(&mut self, buf: &mut [u8]) -> io::Result<()> {
        match self.session.read_pty(buf) {
            Ok(0) => {}
            Ok(n) => {
                let msg = proto::encode(proto::server::OUTPUT, &buf[..n]);
                let mut disconnected = Vec::new();
                let mut flush_ids = Vec::new();
                for (&id, client) in self.clients.iter_mut() {
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
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => log::error!("pty read error: {}", e),
        }
        Ok(())
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
                self.process_client_recv_buf(client_id)?;
            }
        }
        Ok(())
    }

    fn process_client_recv_buf(&mut self, client_id: usize) -> io::Result<()> {
        // Take the buffer out to avoid borrowing self.clients while using self.session
        let mut recv_buf = match self.clients.get_mut(&client_id) {
            Some(c) => std::mem::take(&mut c.recv_buf),
            None => return Ok(()),
        };

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
                    }
                }
                proto::client::DETACH => {}
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
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
