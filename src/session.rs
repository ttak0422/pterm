use crate::pty::Pty;
use std::io;
use std::io::Write as _;
use std::os::fd::AsRawFd;

fn esc_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == 0x1b).count()
}

fn dbg_daemon(msg: &str) {
    if let Ok(path) = std::env::var("PTERM_DEBUG_DAEMON") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{msg}");
        }
    }
}

pub struct Session {
    pub name: String,
    pub pty: Pty,
    parser: vt100::Parser,
    pub exited: Option<i32>,
}

impl Session {
    /// Create a new session with the given name and command.
    pub fn new(name: String, cmd: &str, args: &[&str], cols: u16, rows: u16) -> io::Result<Self> {
        let pty = Pty::spawn(cmd, args, cols, rows)?;
        Ok(Self {
            name,
            pty,
            parser: vt100::Parser::new(rows, cols, 0),
            exited: None,
        })
    }

    /// Read available data from pty, feed to VT parser, return what was read.
    /// Returns `Err(WouldBlock)` when the non-blocking fd has no more data.
    pub fn read_pty(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.pty.master.as_raw_fd();
        match nix::unistd::read(fd, buf) {
            Ok(n) => {
                if n > 0 {
                    self.parser.process(&buf[..n]);
                    let s = self.parser.screen();
                    let (rows, cols) = s.size();
                    let (cur_row, cur_col) = s.cursor_position();
                    dbg_daemon(&format!(
                        "[read_pty] n={n} esc={} size={cols}x{rows} cursor=({cur_col},{cur_row}) errors={}",
                        esc_count(&buf[..n]),
                        s.errors()
                    ));
                }
                Ok(n)
            }
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }

    /// Write input data to pty (forward user keystrokes).
    pub fn write_pty(&self, data: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            match nix::unistd::write(&self.pty.master, &data[written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "pty write returned 0",
                    ));
                }
                Ok(n) => written += n,
                Err(e) if e == nix::errno::Errno::EINTR => continue,
                Err(e) if e == nix::errno::Errno::EAGAIN => {
                    std::thread::yield_now();
                }
                Err(e) => return Err(io::Error::other(e)),
            }
        }
        Ok(())
    }

    /// Resize the pty and VT parser.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.pty.resize(cols, rows)?;
        self.parser.set_size(rows, cols);
        Ok(())
    }

    /// Generate escape sequences that reproduce the current terminal state.
    pub fn snapshot(&self) -> Vec<u8> {
        let s = self.parser.screen();
        let snap = s.state_formatted();
        let (rows, cols) = s.size();
        let wrapped: Vec<u16> = (0..rows).filter(|&r| s.row_wrapped(r)).take(32).collect();
        dbg_daemon(&format!(
            "[snapshot] len={} esc={} size={cols}x{rows} cursor={:?} wrapped={wrapped:?}",
            snap.len(),
            esc_count(&snap),
            s.cursor_position()
        ));
        snap
    }

    /// Get the master fd for polling.
    pub fn master_fd(&self) -> i32 {
        self.pty.master.as_raw_fd()
    }

    /// Check if the child process has exited.
    pub fn check_exit(&mut self) -> Option<i32> {
        if self.exited.is_some() {
            return self.exited;
        }
        match nix::sys::wait::waitpid(
            self.pty.child_pid,
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        ) {
            Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                self.exited = Some(code);
                Some(code)
            }
            Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                let code = 128 + sig as i32;
                self.exited = Some(code);
                Some(code)
            }
            _ => None,
        }
    }
}
