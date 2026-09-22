//! Bridge process: connects stdin/stdout to a pterm daemon session via Unix socket.
//!
//! Launched by Neovim as `jobstart({"pterm", "attach", session}, {term=true})`.
//! Neovim owns the PTY that the bridge's stdin/stdout are connected to, so
//! libvterm processes escape sequences natively in C -- no Lua intermediary.

use crate::constants::{DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS};
use crate::server::SendQueue;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::libc;
use nix::sys::termios;
use pterm_proto as proto;
use std::io::{self, IsTerminal, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_PENDING_INPUT_BYTES: usize = 64 * 1024;
const MAX_PENDING_OUTPUT_BYTES: usize = 1024 * 1024;

// Some interactive programs enable xterm/kitty keyboard enhancement modes.
// Reset them on detach so the next shell prompt does not inherit CSI-u style
// encodings such as Ctrl-D => `CSI 100;5u`.
//
// Cleanup sequences:
// - `CSI ? 1000/1002/1003 l`: disable mouse tracking modes used by full-screen TUIs.
// - `CSI ? 1004 l`: disable focus in/out reporting.
// - `CSI ? 1006 l`: disable SGR mouse encoding.
// - `CSI ? 2004 l`: disable bracketed paste mode.
// - `CSI ? 2026 l`: disable synchronized output mode.
// - `CSI ? 1049 l`: leave the alternate screen buffer.
// - `CSI ? 69 l`: disable left/right margin mode (DECLRMM).
// - `CSI 0 q`: restore the default cursor shape.
// - `CSI ? 25 h`: ensure the text cursor is visible again.
// - `CSI > 4 n`: reset xterm's modifyOtherKeys state.
// - `CSI < u`: disable kitty's progressive keyboard enhancement flags.
// - `CSI = 0 u`: reset kitty keyboard protocol to the base mode.
const DETACH_CLEANUP_SEQUENCES: &[u8] = b"\
\x1b[?1000l\
\x1b[?1002l\
\x1b[?1003l\
\x1b[?1004l\
\x1b[?1006l\
\x1b[?2004l\
\x1b[?2026l\
\x1b[?1049l\
\x1b[?69l\
\x1b[0 q\
\x1b[?25h\
\x1b[>4n\
\x1b[<u\
\x1b[=0u";

// Snapshot replay must start from a known kitty keyboard state; otherwise an
// already-attached terminal can retain its own current flags / push-pop stack
// and later `CSI < u` from the PTY will restore the wrong state.
const STATE_SYNC_KEYBOARD_CLEANUP_SEQUENCES: &[u8] = b"\x1b[<u\x1b[=0u";

static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

/// RAII guard that restores terminal settings on drop.
struct RawModeGuard<'fd> {
    fd: BorrowedFd<'fd>,
    original: termios::Termios,
}

impl<'fd> RawModeGuard<'fd> {
    fn enter(fd: BorrowedFd<'fd>) -> io::Result<Self> {
        let original = termios::tcgetattr(fd).map_err(io::Error::other)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, termios::SetArg::TCSANOW, &raw).map_err(io::Error::other)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard<'_> {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.fd, termios::SetArg::TCSANOW, &self.original);
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
    let (read, write) = nix::unistd::pipe().map_err(io::Error::from)?;
    // Set non-blocking on both ends
    for fd in [&read, &write] {
        let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
        fcntl(
            fd,
            FcntlArg::F_SETFL(OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(io::Error::from)?;
    }
    Ok((read, write))
}

/// Global write end of the self-pipe for signal handler.
static mut WAKE_WRITE_FD: RawFd = -1;

extern "C" fn sigwinch_handler(_sig: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
    unsafe {
        let _ = libc::write(WAKE_WRITE_FD, b"W".as_ptr() as *const libc::c_void, 1);
    }
}

/// Restore the shared open-file flags, including when stdin/stdout are a PTY.
struct NonblockingGuard<'fd> {
    fd: BorrowedFd<'fd>,
    original: OFlag,
}

impl<'fd> NonblockingGuard<'fd> {
    fn enter(fd: BorrowedFd<'fd>) -> io::Result<Self> {
        let original = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL)?);
        fcntl(fd, FcntlArg::F_SETFL(original | OFlag::O_NONBLOCK))?;
        Ok(Self { fd, original })
    }
}

impl Drop for NonblockingGuard<'_> {
    fn drop(&mut self) {
        let _ = fcntl(self.fd, FcntlArg::F_SETFL(self.original));
    }
}

fn poll_fd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        // poll reports HUP even without requested events; disable paused fds.
        fd: if events == 0 { -1 } else { fd },
        events,
        revents: 0,
    }
}

/// Run the bridge, connecting stdin/stdout to the daemon session at `socket_path`.
/// Returns the child process exit code (from the daemon's EXIT message).
///
/// `initial_cols` / `initial_rows` override the terminal size sent in the
/// initial RESIZE message. When `None`, the size is read from `TIOCGWINSZ`
/// (stdout) with a final fallback to the default terminal size.
pub fn run(
    socket_path: &Path,
    initial_cols: Option<u16>,
    initial_rows: Option<u16>,
) -> io::Result<i32> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stdin_fd = stdin.as_raw_fd();
    let stdout_fd = stdout.as_raw_fd();

    // Enter raw mode on stdin (if it's a terminal)
    let _raw_guard = if stdin.is_terminal() {
        Some(RawModeGuard::enter(stdin.as_fd())?)
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
        sa.sa_sigaction = sigwinch_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }

    let mut socket = UnixStream::connect(socket_path)?;
    socket.set_nonblocking(true)?;
    let _stdin_flags = NonblockingGuard::enter(stdin.as_fd())?;
    let _stdout_flags = NonblockingGuard::enter(stdout.as_fd())?;

    // Send initial RESIZE to sync terminal size.
    // CLI-supplied values take priority, then TIOCGWINSZ, then the default
    // terminal size.
    let (cols, rows) = {
        let winsize = get_winsize(stdout_fd).ok();
        let c = initial_cols
            .filter(|&cols| cols > 0)
            .or(winsize.map(|(c, _)| c))
            .filter(|&cols| cols > 0)
            .unwrap_or(DEFAULT_TERMINAL_COLS);
        let r = initial_rows
            .filter(|&rows| rows > 0)
            .or(winsize.map(|(_, r)| r))
            .filter(|&rows| rows > 0)
            .unwrap_or(DEFAULT_TERMINAL_ROWS);
        (c, r)
    };
    let mut send_buf = SendQueue::default();
    {
        // HELLO must precede RESIZE in a single write so the daemon records
        // the handshake before queueing the initial snapshot.
        let hello_payload =
            proto::encode_hello(proto::PROTO_VERSION, proto::hello_flags::REQUEST_HISTORY);
        let mut msg = proto::encode(proto::client::HELLO, &hello_payload);
        let resize_payload = proto::encode_resize(cols, rows);
        msg.extend_from_slice(&proto::encode(proto::client::RESIZE, &resize_payload));
        send_buf.push(&msg);
    }

    let mut stdin_buf = [0u8; 8192];
    let mut sock_buf = [0u8; 65536];
    let mut recv_buf = Vec::new();
    let mut output = SendQueue::default();
    let mut exit_code = 0;
    // A daemon predating HELLO_ACK is detected at the first STATE_SYNC.
    let mut daemon_proto = None;
    let mut proto_checked = false;
    let mut running = true;
    let mut socket_open = true;
    let mut cleanup_queued = false;
    loop {
        if !running && !cleanup_queued {
            output.push(DETACH_CLEANUP_SEQUENCES);
            cleanup_queued = true;
        }
        if !running && output.is_empty() && (!socket_open || send_buf.is_empty()) {
            break;
        }

        // Native poll also supports regular files, unlike epoll registration.
        // Pause only socket reads at the output watermark; input, socket writes,
        // and SIGWINCH remain responsive while the display is stopped.
        let read_socket = running && output.pending_bytes() < MAX_PENDING_OUTPUT_BYTES;
        let mut fds = [
            poll_fd(
                stdin_fd,
                if running && send_buf.pending_bytes() < MAX_PENDING_INPUT_BYTES {
                    libc::POLLIN
                } else {
                    0
                },
            ),
            poll_fd(
                socket.as_raw_fd(),
                if socket_open {
                    (if read_socket { libc::POLLIN } else { 0 })
                        | (if !send_buf.is_empty() {
                            libc::POLLOUT
                        } else {
                            0
                        })
                } else {
                    0
                },
            ),
            poll_fd(wake_read.as_raw_fd(), libc::POLLIN),
            poll_fd(stdout_fd, if output.is_empty() { 0 } else { libc::POLLOUT }),
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        if fds[0].revents != 0 {
            match nix::unistd::read(&stdin, &mut stdin_buf) {
                Ok(0) => {
                    running = false;
                    send_buf.push(&proto::encode(proto::client::DETACH, &[]));
                }
                Ok(n) => send_buf.push(&proto::encode(proto::client::INPUT, &stdin_buf[..n])),
                Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => {}
                Err(error) => return Err(error.into()),
            }
        }

        if read_socket && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match socket.read(&mut sock_buf) {
                Ok(0) => {
                    running = false;
                    socket_open = false;
                }
                Ok(n) => {
                    recv_buf.extend_from_slice(&sock_buf[..n]);
                    let mut output_batch: Vec<u8> = Vec::new();
                    let mut state_sync_cleanup_queued = false;
                    for frame in proto::decode_frames(&mut recv_buf, proto::MAX_SERVER_PAYLOAD)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                    {
                        match frame.msg_type {
                            proto::server::OUTPUT => {
                                output_batch.extend_from_slice(&frame.payload);
                            }
                            proto::server::HELLO_ACK => {
                                match proto::parse_hello_ack(&frame.payload) {
                                    Ok((version, pkg_version)) => {
                                        log::info!(
                                            "Daemon hello ack: proto v{}, pterm {}",
                                            version,
                                            pkg_version
                                        );
                                        daemon_proto = Some(version);
                                    }
                                    Err(e) => {
                                        log::warn!("Invalid hello ack payload: {}", e);
                                    }
                                }
                            }
                            proto::server::HISTORY => {
                                // Scrollback replay: written to the terminal
                                // like OUTPUT so it accumulates in the client
                                // terminal's own scrollback. The daemon sends
                                // it just before the initial STATE_SYNC.
                                output_batch.extend_from_slice(&frame.payload);
                            }
                            proto::server::STATE_SYNC => {
                                if !proto_checked {
                                    proto_checked = true;
                                    let version = daemon_proto.unwrap_or(0);
                                    if version != proto::PROTO_VERSION {
                                        log::warn!(
                                            "Daemon protocol v{} differs from client v{}",
                                            version,
                                            proto::PROTO_VERSION
                                        );
                                        let notice = format!(
                                            "[pterm: daemon protocol v{} / client v{} — restart the session to upgrade]\r\n",
                                            version,
                                            proto::PROTO_VERSION
                                        );
                                        output_batch.extend_from_slice(notice.as_bytes());
                                    }
                                }
                                if !state_sync_cleanup_queued {
                                    output_batch
                                        .extend_from_slice(STATE_SYNC_KEYBOARD_CLEANUP_SEQUENCES);
                                    state_sync_cleanup_queued = true;
                                }
                                output_batch.extend_from_slice(&frame.payload);
                            }
                            proto::server::EXIT => {
                                if let Ok(code) = proto::parse_exit(&frame.payload) {
                                    exit_code = code;
                                }
                                running = false;
                                socket_open = false;
                                break;
                            }
                            _ => {}
                        }
                    }
                    if !output_batch.is_empty() {
                        output.push(&output_batch);
                    }
                }
                Err(ref error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    running = false;
                    socket_open = false;
                }
            }
        }

        if socket_open
            && fds[1].revents & (libc::POLLOUT | libc::POLLHUP | libc::POLLERR) != 0
            && send_buf.write_to(&mut socket).is_err()
        {
            running = false;
            socket_open = false;
        }

        if fds[2].revents != 0 {
            let mut drain = [0u8; 64];
            while matches!(nix::unistd::read(&wake_read, &mut drain), Ok(n) if n > 0) {}
            if running && SIGWINCH_RECEIVED.swap(false, Ordering::SeqCst) {
                if let Ok((cols, rows)) = get_winsize(stdout_fd) {
                    if cols > 0 && rows > 0 {
                        let payload = proto::encode_resize(cols, rows);
                        send_buf.push(&proto::encode(proto::client::RESIZE, &payload));
                    }
                }
            }
        }

        if fds[3].revents != 0 {
            output
                .write_with(|bytes| nix::unistd::write(&stdout, bytes).map_err(io::Error::from))?;
        }
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::{
        make_pipe, NonblockingGuard, RawModeGuard, DETACH_CLEANUP_SEQUENCES,
        STATE_SYNC_KEYBOARD_CLEANUP_SEQUENCES,
    };
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::sys::termios;
    use std::os::fd::AsFd;

    #[test]
    fn wake_pipe_is_nonblocking_and_transfers_bytes() {
        let (read, write) = make_pipe().unwrap();
        for fd in [&read, &write] {
            let flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL).unwrap());
            assert!(flags.contains(OFlag::O_NONBLOCK));
        }
        let mut buf = [0; 4];
        assert_eq!(
            nix::unistd::read(&read, &mut buf),
            Err(nix::errno::Errno::EAGAIN)
        );
        assert_eq!(nix::unistd::write(&write, b"wake").unwrap(), 4);
        assert_eq!(nix::unistd::read(&read, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"wake");
    }

    #[test]
    fn nonblocking_guard_restores_shared_descriptor_flags() {
        let (read, _) = nix::unistd::pipe().unwrap();
        let original = fcntl(&read, FcntlArg::F_GETFL).unwrap();
        {
            let _first = NonblockingGuard::enter(read.as_fd()).unwrap();
            {
                let _second = NonblockingGuard::enter(read.as_fd()).unwrap();
            }
            let flags = OFlag::from_bits_retain(fcntl(&read, FcntlArg::F_GETFL).unwrap());
            assert!(flags.contains(OFlag::O_NONBLOCK));
        }
        assert_eq!(fcntl(&read, FcntlArg::F_GETFL).unwrap(), original);
    }

    #[test]
    fn raw_mode_guard_restores_terminal_settings() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let original = termios::tcgetattr(&pty.slave).unwrap();
        {
            let _guard = RawModeGuard::enter(pty.slave.as_fd()).unwrap();
            let raw = termios::tcgetattr(&pty.slave).unwrap();
            assert!(!raw
                .local_flags
                .intersects(termios::LocalFlags::ICANON | termios::LocalFlags::ECHO));
        }
        let restored = termios::tcgetattr(&pty.slave).unwrap();
        assert_eq!(restored.input_flags, original.input_flags);
        assert_eq!(restored.output_flags, original.output_flags);
        assert_eq!(restored.control_flags, original.control_flags);
        // macOS may set PENDIN when canonical mode is restored.
        assert_eq!(
            restored.local_flags & !termios::LocalFlags::PENDIN,
            original.local_flags & !termios::LocalFlags::PENDIN
        );
        assert_eq!(restored.control_chars, original.control_chars);
    }

    #[test]
    fn detach_cleanup_resets_keyboard_protocols() {
        let cleanup = std::str::from_utf8(DETACH_CLEANUP_SEQUENCES).unwrap();
        assert!(cleanup.contains("\x1b[?1004l"));
        assert!(cleanup.contains("\x1b[?2026l"));
        assert!(cleanup.contains("\x1b[>4n"));
        assert!(cleanup.contains("\x1b[<u"));
        assert!(cleanup.contains("\x1b[=0u"));
    }

    #[test]
    fn state_sync_cleanup_resets_kitty_keyboard_state() {
        let cleanup = std::str::from_utf8(STATE_SYNC_KEYBOARD_CLEANUP_SEQUENCES).unwrap();
        assert_eq!(cleanup, "\x1b[<u\x1b[=0u");
    }
}
