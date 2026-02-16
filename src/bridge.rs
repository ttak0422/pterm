//! Bridge process: connects stdin/stdout to a pterm daemon session via Unix socket.
//!
//! Launched by Neovim as `jobstart({"pterm", "attach", session}, {term=true})`.
//! Neovim owns the PTY that the bridge's stdin/stdout are connected to, so
//! libvterm processes escape sequences natively in C -- no Lua intermediary.

use mio::net::UnixStream;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use nix::libc;
use nix::sys::termios;
use pterm_proto as proto;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const TOKEN_STDIN: Token = Token(0);
const TOKEN_SOCKET: Token = Token(1);
const TOKEN_WAKE: Token = Token(2);

static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

/// RAII guard that restores terminal settings on drop.
struct RawModeGuard {
    fd: RawFd,
    original: termios::Termios,
}

impl RawModeGuard {
    fn enter(fd: RawFd) -> io::Result<Self> {
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = termios::tcgetattr(borrowed).map_err(io::Error::other)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(borrowed, termios::SetArg::TCSANOW, &raw).map_err(io::Error::other)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = termios::tcsetattr(borrowed, termios::SetArg::TCSANOW, &self.original);
    }
}

/// Get the current terminal size from a file descriptor.
fn get_winsize(fd: RawFd) -> io::Result<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok((ws.ws_col, ws.ws_row))
    }
}

/// Create a pipe and return (read_fd, write_fd) as OwnedFd.
fn make_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    // Set non-blocking on both ends
    for &fd in &fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Global write end of the self-pipe for signal handler.
static mut WAKE_WRITE_FD: RawFd = -1;

extern "C" fn sigwinch_handler(_sig: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
    unsafe {
        let _ = libc::write(WAKE_WRITE_FD, b"W".as_ptr() as *const libc::c_void, 1);
    }
}

/// Write all bytes to a raw fd, retrying on EAGAIN.
fn write_all_raw(fd: RawFd, data: &[u8]) -> io::Result<()> {
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut written = 0;
    while written < data.len() {
        match nix::unistd::write(borrowed, &data[written..]) {
            Ok(n) => written += n,
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                continue
            }
            Err(e) => return Err(io::Error::other(e)),
        }
    }
    Ok(())
}

/// Run the bridge, connecting stdin/stdout to the daemon session at `socket_path`.
/// Returns the child process exit code (from the daemon's EXIT message).
pub fn run(socket_path: &Path) -> io::Result<i32> {
    let stdin_fd = libc::STDIN_FILENO;
    let stdout_fd = libc::STDOUT_FILENO;

    // Enter raw mode on stdin (if it's a terminal)
    let _raw_guard = if unsafe { libc::isatty(stdin_fd) } == 1 {
        Some(RawModeGuard::enter(stdin_fd)?)
    } else {
        None
    };

    // Set up self-pipe for SIGWINCH
    let (wake_read, wake_write) = make_pipe()?;
    unsafe {
        WAKE_WRITE_FD = wake_write.as_raw_fd();
    }

    // Install SIGWINCH handler
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigwinch_handler as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }

    // Connect to daemon socket
    let std_stream = std::os::unix::net::UnixStream::connect(socket_path)?;
    std_stream.set_nonblocking(true)?;
    let mut socket = UnixStream::from_std(std_stream);

    // Set stdin to non-blocking
    unsafe {
        let flags = libc::fcntl(stdin_fd, libc::F_GETFL);
        libc::fcntl(stdin_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Set up mio poll
    let mut poll = Poll::new()?;
    let mut stdin_source = SourceFd(&stdin_fd);
    poll.registry()
        .register(&mut stdin_source, TOKEN_STDIN, Interest::READABLE)?;
    poll.registry()
        .register(&mut socket, TOKEN_SOCKET, Interest::READABLE)?;
    let wake_read_fd = wake_read.as_raw_fd();
    let mut wake_source = SourceFd(&wake_read_fd);
    poll.registry()
        .register(&mut wake_source, TOKEN_WAKE, Interest::READABLE)?;

    // Send initial RESIZE to sync terminal size
    if let Ok((cols, rows)) = get_winsize(stdout_fd) {
        let resize_payload = proto::encode_resize(cols, rows);
        let msg = proto::encode(proto::client::RESIZE, &resize_payload);
        socket.write_all(&msg)?;
    }

    let mut events = Events::with_capacity(16);
    let mut stdin_buf = [0u8; 8192];
    let mut sock_buf = [0u8; 65536];
    let mut recv_buf: Vec<u8> = Vec::new();
    let mut exit_code: i32 = 0;
    'main: loop {
        match poll.poll(&mut events, None) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }

        for event in events.iter() {
            match event.token() {
                TOKEN_STDIN => {
                    // Read from stdin, send as INPUT to daemon
                    loop {
                        match nix::unistd::read(stdin_fd, &mut stdin_buf) {
                            Ok(0) => {
                                // stdin EOF: detach and exit
                                let msg = proto::encode(proto::client::DETACH, &[]);
                                let _ = socket.write_all(&msg);
                                break 'main;
                            }
                            Ok(n) => {
                                let msg = proto::encode(proto::client::INPUT, &stdin_buf[..n]);
                                if socket.write_all(&msg).is_err() {
                                    break 'main;
                                }
                            }
                            Err(e)
                                if e == nix::errno::Errno::EAGAIN
                                    || e == nix::errno::Errno::EWOULDBLOCK =>
                            {
                                break;
                            }
                            Err(_) => {
                                break 'main;
                            }
                        }
                    }
                }

                TOKEN_SOCKET => {
                    // Read from socket, parse protocol frames
                    loop {
                        match socket.read(&mut sock_buf) {
                            Ok(0) => {
                                // Socket EOF: daemon closed
                                break 'main;
                            }
                            Ok(n) => {
                                recv_buf.extend_from_slice(&sock_buf[..n]);
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                break;
                            }
                            Err(_) => {
                                break 'main;
                            }
                        }
                    }

                    // Process complete frames
                    let mut offset = 0;
                    while offset + proto::HEADER_SIZE <= recv_buf.len() {
                        let header: [u8; proto::HEADER_SIZE] = recv_buf
                            [offset..offset + proto::HEADER_SIZE]
                            .try_into()
                            .unwrap();
                        let (msg_type, payload_len) = proto::decode_header(&header);
                        let payload_len = payload_len as usize;

                        if offset + proto::HEADER_SIZE + payload_len > recv_buf.len() {
                            break; // incomplete frame
                        }

                        offset += proto::HEADER_SIZE;
                        let payload = &recv_buf[offset..offset + payload_len];
                        offset += payload_len;

                        match msg_type {
                            proto::server::OUTPUT | proto::server::SCROLLBACK => {
                                // Write raw bytes to stdout for libvterm
                                if write_all_raw(stdout_fd, payload).is_err() {
                                    break 'main;
                                }
                            }
                            proto::server::EXIT => {
                                if payload.len() >= 4 {
                                    exit_code =
                                        i32::from_le_bytes(payload[..4].try_into().unwrap());
                                }
                                break 'main;
                            }
                            _ => {}
                        }
                    }

                    if offset > 0 {
                        recv_buf.drain(..offset);
                    }
                }

                TOKEN_WAKE => {
                    // Drain wake pipe
                    let mut drain = [0u8; 64];
                    loop {
                        match nix::unistd::read(wake_read_fd, &mut drain) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }

                    // Handle SIGWINCH
                    if SIGWINCH_RECEIVED.swap(false, Ordering::SeqCst) {
                        if let Ok((cols, rows)) = get_winsize(stdout_fd) {
                            let resize_payload = proto::encode_resize(cols, rows);
                            let msg = proto::encode(proto::client::RESIZE, &resize_payload);
                            let _ = socket.write_all(&msg);
                        }
                    }
                }

                _ => {}
            }
        }
    }

    // Send DETACH before exiting
    let msg = proto::encode(proto::client::DETACH, &[]);
    let _ = socket.write_all(&msg);

    Ok(exit_code)
}
