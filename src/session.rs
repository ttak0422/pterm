use crate::pty::Pty;
use std::io;
use std::os::fd::AsRawFd;

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
    pub fn read_pty(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.pty.master.as_raw_fd();
        let n = nix::unistd::read(fd, buf).map_err(io::Error::other)?;
        if n > 0 {
            self.parser.process(&buf[..n]);
        }
        Ok(n)
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
        self.parser.screen().state_formatted()
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
