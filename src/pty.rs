use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{dup2, execve, execvp, fork, setsid, ForkResult, Pid};
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
    /// `extra_env` is a list of `(key, value)` pairs to add or override in the
    /// child environment. When non-empty the child is started with `execve`
    /// using the current process environment merged with the overrides.
    pub fn spawn(
        cmd: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        extra_env: &[(&str, &str)],
    ) -> io::Result<Self> {
        // Open a pty pair
        let OpenptyResult { master, slave } = openpty(None, None).map_err(io::Error::other)?;

        // Set initial window size
        set_winsize(master.as_raw_fd(), cols, rows)?;

        // Set master fd to non-blocking so the daemon can drain all available
        // data in a loop without blocking on the last read.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

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
                dup2(slave.as_raw_fd(), libc::STDIN_FILENO).ok();
                dup2(slave.as_raw_fd(), libc::STDOUT_FILENO).ok();
                dup2(slave.as_raw_fd(), libc::STDERR_FILENO).ok();

                if slave.as_raw_fd() > 2 {
                    drop(slave);
                }

                // Exec the command
                let c_cmd = CString::new(cmd).unwrap();
                let c_args: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();

                if extra_env.is_empty() {
                    execvp(&c_cmd, &c_args).ok();
                } else {
                    // Build full environment: current env merged with overrides.
                    let mut env_map: std::collections::HashMap<String, String> =
                        std::env::vars().collect();
                    for (k, v) in extra_env {
                        env_map.insert(k.to_string(), v.to_string());
                    }
                    let c_env: Vec<CString> = env_map
                        .into_iter()
                        .filter_map(|(k, v)| CString::new(format!("{}={}", k, v)).ok())
                        .collect();
                    execve(&c_cmd, &c_args, &c_env).ok();
                }

                // If exec fails
                std::process::exit(127);
            }
        }
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
