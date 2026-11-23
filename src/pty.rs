use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{dup2_stderr, dup2_stdin, dup2_stdout, execvp, fork, setsid, ForkResult, Pid};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

pub struct Pty {
    pub master: OwnedFd,
    pub child_pid: Pid,
}

impl Pty {
    /// Fork a child process connected to a new pty.
    /// `cmd` is the command to execute (e.g., "/bin/bash").
    /// `args` are the arguments (argv[0] should be the command name).
    /// `cols` and `rows` set the initial terminal size.
    ///
    /// # Errors
    /// Returns `InvalidInput` for NUL bytes in the command or its arguments,
    /// before allocating a PTY or forking. PTY setup and fork errors are returned
    /// to the caller; an unsuccessful exec retains child exit status 127.
    pub fn spawn(cmd: &str, args: &[&str], cols: u16, rows: u16) -> io::Result<Self> {
        // Validate and allocate argument storage in the parent, not after fork.
        let c_cmd = CString::new(cmd).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "command contains a NUL byte")
        })?;
        let c_args: Vec<CString> = args
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                CString::new(*arg).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("argument {index} contains a NUL byte"),
                    )
                })
            })
            .collect::<io::Result<_>>()?;

        // Open a pty pair
        let OpenptyResult { master, slave } = openpty(None, None).map_err(io::Error::other)?;

        // Set initial window size
        set_winsize(master.as_raw_fd(), cols, rows)?;

        // Set master fd to non-blocking so the daemon can drain all available
        // data in a loop without blocking on the last read.
        let flags = fcntl(&master, FcntlArg::F_GETFL).map_err(io::Error::from)?;
        fcntl(
            &master,
            FcntlArg::F_SETFL(OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(io::Error::from)?;

        // Fork
        match unsafe { fork() }.map_err(io::Error::other)? {
            ForkResult::Parent { child } => {
                // Parent: close slave side, keep master
                drop(slave);
                Ok(Pty {
                    master,
                    child_pid: child,
                })
            }
            ForkResult::Child => {
                // Child: set up pty as controlling terminal
                drop(master);

                setsid().ok();

                // Set slave as controlling terminal
                unsafe {
                    libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as libc::c_ulong, 0);
                }

                // Redirect stdin/stdout/stderr to slave
                dup2_stdin(&slave).ok();
                dup2_stdout(&slave).ok();
                dup2_stderr(&slave).ok();

                if slave.as_raw_fd() > 2 {
                    drop(slave);
                }

                // Exec the command
                execvp(&c_cmd, &c_args).ok();

                // If exec fails
                std::process::exit(127);
            }
        }
    }

    /// Return the executable of the foreground process-group leader, if verified.
    /// A gone leader or unresolvable pipeline is unknown, never guessed.
    pub fn foreground_executable(&self) -> Option<String> {
        // SAFETY: the fd belongs to this PTY; these calls only query kernel metadata.
        let foreground = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        if foreground <= 0
            || unsafe { libc::getpgid(foreground) } != foreground
            || unsafe { libc::getsid(foreground) } != self.child_pid.as_raw()
        {
            return None;
        }
        let executable = crate::paths::process_executable(foreground)?;
        (unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) } == foreground).then_some(executable)
    }

    /// Resize the pty.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        set_winsize(self.master.as_raw_fd(), cols, rows)
    }
}

fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_command_is_rejected_before_spawn() {
        let Err(error) = Pty::spawn("sh\0other", &["sh"], 80, 24) else {
            panic!("invalid command must not create a child");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "command contains a NUL byte");
    }

    #[test]
    fn invalid_argument_is_rejected_without_echoing_input() {
        for (args, index) in [(vec!["sh\0"], 0), (vec!["sh", "private\0argument"], 1)] {
            let Err(error) = Pty::spawn("sh", &args, 80, 24) else {
                panic!("invalid argument must not create a child");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(
                error.to_string(),
                format!("argument {index} contains a NUL byte")
            );
        }
    }
}
