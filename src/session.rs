use crate::pty::Pty;
use std::io;
use std::os::fd::AsRawFd;

#[derive(Default)]
struct SessionCallbacks {
    window_title: Option<String>,
    pending_da1_queries: usize,
    pending_da2_queries: usize,
}

impl SessionCallbacks {
    fn default_da_params(params: &[&[u16]]) -> bool {
        params.is_empty()
            || (params.len() == 1
                && (params[0].is_empty() || (params[0].len() == 1 && params[0][0] == 0)))
    }

    fn take_pending_da_queries(&mut self) -> (usize, usize) {
        let counts = (self.pending_da1_queries, self.pending_da2_queries);
        self.pending_da1_queries = 0;
        self.pending_da2_queries = 0;
        counts
    }
}

impl vt100::Callbacks for SessionCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.window_title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if c != 'c' || i2.is_some() || !Self::default_da_params(params) {
            return;
        }

        match i1 {
            None => self.pending_da1_queries += 1,
            Some(b'>') => self.pending_da2_queries += 1,
            _ => {}
        }
    }
}

pub struct Session {
    pub name: String,
    pub pty: Pty,
    parser: vt100::Parser<SessionCallbacks>,
    pub exited: Option<i32>,
}

impl Session {
    /// Create a new session with the given name and command.
    pub fn new(name: String, cmd: &str, args: &[&str], cols: u16, rows: u16) -> io::Result<Self> {
        let pty = Pty::spawn(cmd, args, cols, rows)?;
        Ok(Self {
            name,
            pty,
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                10_000,
                SessionCallbacks::default(),
            ),
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
        self.parser.screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// Generate escape sequences that reproduce the current terminal state.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut snapshot = self.parser.screen().state_formatted();
        let mut prefix = Vec::new();

        if let Some(title) = self.parser.callbacks().window_title.as_ref() {
            prefix.extend_from_slice(b"\x1b]2;");
            prefix.extend_from_slice(title.as_bytes());
            prefix.extend_from_slice(b"\x1b\\");
        }
        if self.parser.screen().alternate_screen() {
            prefix.extend_from_slice(b"\x1b[?1049h");
        }

        if prefix.is_empty() {
            snapshot
        } else {
            prefix.append(&mut snapshot);
            prefix
        }
    }

    pub fn take_pending_da_queries(&mut self) -> (usize, usize) {
        self.parser.callbacks_mut().take_pending_da_queries()
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
