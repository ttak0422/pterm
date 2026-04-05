use crate::pty::Pty;
use std::io;
use std::os::fd::AsRawFd;

#[derive(Default)]
struct SessionCallbacks {
    window_title: Option<String>,
    pending_da1_queries: usize,
    pending_da2_queries: usize,
    passthrough_sequences: Vec<Vec<u8>>,
    passthrough_bytes: usize,
}

impl SessionCallbacks {
    const MAX_PASSTHROUGH_SEQUENCES: usize = 256;
    const MAX_PASSTHROUGH_BYTES: usize = 16 * 1024;

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

    fn push_passthrough_sequence(&mut self, seq: Vec<u8>) {
        if seq.is_empty() {
            return;
        }

        self.passthrough_bytes += seq.len();
        self.passthrough_sequences.push(seq);

        while self.passthrough_sequences.len() > Self::MAX_PASSTHROUGH_SEQUENCES
            || self.passthrough_bytes > Self::MAX_PASSTHROUGH_BYTES
        {
            let removed = self.passthrough_sequences.remove(0);
            self.passthrough_bytes -= removed.len();
        }
    }

    fn passthrough_sequences_formatted(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.passthrough_bytes);
        for seq in &self.passthrough_sequences {
            bytes.extend_from_slice(seq);
        }
        bytes
    }

    fn format_unhandled_escape(i1: Option<u8>, i2: Option<u8>, b: u8) -> Vec<u8> {
        let mut seq = vec![0x1b];
        if let Some(i1) = i1 {
            seq.push(i1);
        }
        if let Some(i2) = i2 {
            seq.push(i2);
        }
        seq.push(b);
        seq
    }

    fn format_unhandled_csi(i1: Option<u8>, i2: Option<u8>, params: &[&[u16]], c: char) -> Vec<u8> {
        let mut seq = vec![0x1b, b'['];
        if let Some(i1) = i1 {
            seq.push(i1);
        }
        for (idx, param) in params.iter().enumerate() {
            if idx > 0 {
                seq.push(b';');
            }
            for (sub_idx, value) in param.iter().enumerate() {
                if sub_idx > 0 {
                    seq.push(b':');
                }
                seq.extend_from_slice(value.to_string().as_bytes());
            }
        }
        if let Some(i2) = i2 {
            seq.push(i2);
        }
        seq.push(c as u8);
        seq
    }

    fn format_unhandled_osc(params: &[&[u8]]) -> Vec<u8> {
        let mut seq = vec![0x1b, b']'];
        for (idx, param) in params.iter().enumerate() {
            if idx > 0 {
                seq.push(b';');
            }
            seq.extend_from_slice(param);
        }
        seq.extend_from_slice(b"\x1b\\");
        seq
    }
}

fn build_snapshot(screen: &vt100::Screen, callbacks: &SessionCallbacks) -> Vec<u8> {
    let mut snapshot = screen.state_formatted();
    let mut prefix = Vec::new();

    if let Some(title) = callbacks.window_title.as_ref() {
        prefix.extend_from_slice(b"\x1b]2;");
        prefix.extend_from_slice(title.as_bytes());
        prefix.extend_from_slice(b"\x1b\\");
    }
    if screen.alternate_screen() {
        prefix.extend_from_slice(b"\x1b[?1049h");
    }
    prefix.extend_from_slice(&callbacks.passthrough_sequences_formatted());

    if prefix.is_empty() {
        snapshot
    } else {
        prefix.append(&mut snapshot);
        prefix
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
            _ => {
                self.push_passthrough_sequence(Self::format_unhandled_csi(i1, i2, params, c));
            }
        }
    }

    fn unhandled_escape(&mut self, _: &mut vt100::Screen, i1: Option<u8>, i2: Option<u8>, b: u8) {
        self.push_passthrough_sequence(Self::format_unhandled_escape(i1, i2, b));
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        self.push_passthrough_sequence(Self::format_unhandled_osc(params));
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
        build_snapshot(self.parser.screen(), self.parser.callbacks())
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

#[cfg(test)]
mod tests {
    use super::{build_snapshot, SessionCallbacks};

    #[test]
    fn snapshot_preserves_unhandled_osc_sequences() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot = String::from_utf8_lossy(&snapshot);

        assert!(snapshot.contains("\x1b]8;;https://example.com\x1b\\"));
        assert!(snapshot.contains("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn handled_sgr_is_not_added_to_passthrough_sequences() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[31mRED\x1b[m");

        assert!(parser.callbacks().passthrough_sequences.is_empty());
    }
}
