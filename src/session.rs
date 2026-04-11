use crate::pty::Pty;
use std::collections::VecDeque;
use std::io;
use std::os::fd::AsRawFd;

#[derive(Default)]
struct SessionCallbacks {
    window_title: Option<String>,
    window_title_stack: Vec<String>,
    pending_da1_queries: usize,
    pending_da2_queries: usize,
    passthrough_sequences: VecDeque<Vec<u8>>,
    passthrough_bytes: usize,
}

impl SessionCallbacks {
    const MAX_PASSTHROUGH_SEQUENCES: usize = 256;
    const MAX_PASSTHROUGH_BYTES: usize = 16 * 1024;
    const PASSTHROUGH_DEC_PRIVATE_MODES: [u16; 4] = [12, 69, 1004, 2026];

    fn default_da_params(params: &[&[u16]]) -> bool {
        params.is_empty()
            || (params.len() == 1
                && (params[0].is_empty() || (params[0].len() == 1 && params[0][0] == 0)))
    }

    fn first_param(params: &[&[u16]]) -> Option<u16> {
        params.first().and_then(|param| param.first()).copied()
    }

    fn is_passthrough_private_mode(
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) -> bool {
        i1 == Some(b'?')
            && i2.is_none()
            && matches!(c, 'h' | 'l')
            && Self::first_param(params)
                .is_some_and(|mode| Self::PASSTHROUGH_DEC_PRIVATE_MODES.contains(&mode))
    }

    fn is_kitty_keyboard_protocol(i1: Option<u8>, i2: Option<u8>, c: char) -> bool {
        i2.is_none() && c == 'u' && matches!(i1, Some(b'>') | Some(b'=') | Some(b'<'))
    }

    fn is_passthrough_sgr_subparams(
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) -> bool {
        if i1.is_some() || i2.is_some() || c != 'm' {
            return false;
        }

        let Some(first) = params.first() else {
            return false;
        };

        matches!(first.first().copied(), Some(4) if first.len() > 1)
            || matches!(first.first().copied(), Some(58))
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
        self.passthrough_sequences.push_back(seq);

        while self.passthrough_sequences.len() > Self::MAX_PASSTHROUGH_SEQUENCES
            || self.passthrough_bytes > Self::MAX_PASSTHROUGH_BYTES
        {
            let removed = self
                .passthrough_sequences
                .pop_front()
                .expect("passthrough sequence queue should not be empty");
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
        // Private/prefix bytes (0x3C-0x3F: <, =, >, ?) are collected before
        // numerical parameters in the CSI entry state, so they must be emitted
        // before params.
        if let Some(i) = i1 {
            if i >= 0x3C {
                seq.push(i);
            }
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
        // True intermediate bytes (0x20-0x2F, e.g. SP in DECSCUSR ESC[Ps SP q)
        // are collected after numerical parameters, so they must be emitted
        // after params.
        if let Some(i) = i1 {
            if i < 0x3C {
                seq.push(i);
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

    fn format_clipboard_copy(screen: &[u8], data: &[u8]) -> Vec<u8> {
        let mut seq = Vec::with_capacity(8 + screen.len() + data.len());
        seq.extend_from_slice(b"\x1b]52;");
        seq.extend_from_slice(screen);
        seq.push(b';');
        seq.extend_from_slice(data);
        seq.extend_from_slice(b"\x1b\\");
        seq
    }
}

fn build_snapshot(screen: &vt100::Screen, callbacks: &SessionCallbacks) -> Vec<u8> {
    let passthrough = callbacks.passthrough_sequences_formatted();
    let mut snapshot = screen.state_formatted();
    let mut prefix = Vec::new();

    // Rebuild the window title stack so subsequent ESC[23;0t restores work
    // correctly after snapshot replay.
    for stacked_title in &callbacks.window_title_stack {
        prefix.extend_from_slice(b"\x1b]2;");
        prefix.extend_from_slice(stacked_title.as_bytes());
        prefix.extend_from_slice(b"\x1b\\");
        prefix.extend_from_slice(b"\x1b[22;0t");
    }

    if let Some(title) = callbacks.window_title.as_ref() {
        prefix.extend_from_slice(b"\x1b]2;");
        prefix.extend_from_slice(title.as_bytes());
        prefix.extend_from_slice(b"\x1b\\");
    }
    if screen.alternate_screen() {
        prefix.extend_from_slice(b"\x1b[?1049h");
    }

    if !prefix.is_empty() {
        prefix.append(&mut snapshot);
        snapshot = prefix;
    }

    // Passthrough sequences (e.g. DECSCUSR cursor shape) must come after
    // state_formatted() so they are not overwritten by cursor-positioning
    // sequences that state_formatted() emits at the end.
    if !passthrough.is_empty() {
        snapshot.extend_from_slice(&passthrough);
    }
    snapshot
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
        if i1 == Some(b' ') && i2.is_none() && c == 'q' {
            self.push_passthrough_sequence(Self::format_unhandled_csi(i1, i2, params, c));
            return;
        }

        if Self::is_passthrough_private_mode(i1, i2, params, c)
            || Self::is_kitty_keyboard_protocol(i1, i2, c)
            || Self::is_passthrough_sgr_subparams(i1, i2, params, c)
        {
            self.push_passthrough_sequence(Self::format_unhandled_csi(i1, i2, params, c));
            return;
        }

        if i1.is_none() && i2.is_none() && c == 't' {
            match Self::first_param(params) {
                Some(22) => {
                    let title = self.window_title.clone().unwrap_or_default();
                    self.window_title_stack.push(title);
                }
                Some(23) => {
                    if let Some(title) = self.window_title_stack.pop() {
                        self.window_title = if title.is_empty() { None } else { Some(title) };
                    }
                }
                _ => self.push_passthrough_sequence(Self::format_unhandled_csi(i1, i2, params, c)),
            }
            return;
        }

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

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        self.push_passthrough_sequence(Self::format_clipboard_copy(ty, data));
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

    #[test]
    fn snapshot_preserves_decscusr_cursor_shape() {
        // Vim emits DECSCUSR when switching modes (e.g. ESC[6 q for bar cursor
        // in insert mode, ESC[2 q for block in normal mode).
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[6 q");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot = String::from_utf8_lossy(&snapshot);

        assert!(
            snapshot.contains("\x1b[6 q"),
            "DECSCUSR (ESC[6 q) should be preserved in snapshot"
        );
    }

    #[test]
    fn snapshot_preserves_cursor_blink_mode() {
        // AT&T 610: ESC[?12l disables cursor blink, ESC[?12h enables it.
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[?12l");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        assert!(
            snapshot_str.contains("\x1b[?12l"),
            "cursor blink disable (ESC[?12l) should be preserved in snapshot"
        );
    }

    #[test]
    fn snapshot_preserves_declrmm() {
        // DECLRMM: ESC[?69h enables left-right margin mode.
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[?69h");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        assert!(
            snapshot_str.contains("\x1b[?69h"),
            "DECLRMM (ESC[?69h) should be preserved in snapshot"
        );
    }

    #[test]
    fn snapshot_preserves_focus_tracking_and_synchronized_output() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[?1004h\x1b[?2026h");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        assert!(snapshot_str.contains("\x1b[?1004h"));
        assert!(snapshot_str.contains("\x1b[?2026h"));
    }

    #[test]
    fn snapshot_preserves_kitty_keyboard_protocol_sequences() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        let mut screen = parser.screen().clone();
        let gt_params = [1u16];
        let eq_params = [5u16];

        {
            let callbacks = parser.callbacks_mut();
            vt100::Callbacks::unhandled_csi(
                callbacks,
                &mut screen,
                Some(b'>'),
                None,
                &[&gt_params],
                'u',
            );
            vt100::Callbacks::unhandled_csi(
                callbacks,
                &mut screen,
                Some(b'='),
                None,
                &[&eq_params],
                'u',
            );
            vt100::Callbacks::unhandled_csi(callbacks, &mut screen, Some(b'<'), None, &[], 'u');
        }

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        assert!(snapshot_str.contains("\x1b[>1u"));
        assert!(snapshot_str.contains("\x1b[=5u"));
        assert!(snapshot_str.contains("\x1b[<u"));
        assert_eq!(parser.callbacks().pending_da2_queries, 0);
    }

    #[test]
    fn snapshot_preserves_unhandled_sgr_subparams() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[4:3m\x1b[58:2:1:2:3m");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        assert!(snapshot_str.contains("\x1b[4:3m"));
        assert!(snapshot_str.contains("\x1b[58:2:1:2:3m"));
    }

    #[test]
    fn snapshot_preserves_osc_52_clipboard_copy() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b]52;c;SGVsbG8=\x1b\\");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        assert!(snapshot_str.contains("\x1b]52;c;SGVsbG8=\x1b\\"));
    }

    #[test]
    fn snapshot_preserves_non_title_csi_t_sequences() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[14t\x1b[16t\x1b[18t");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        assert!(snapshot_str.contains("\x1b[14t"));
        assert!(snapshot_str.contains("\x1b[16t"));
        assert!(snapshot_str.contains("\x1b[18t"));
    }

    #[test]
    fn snapshot_preserves_window_title_stack() {
        // Vim saves the original title on entry with ESC[22;0t, then sets its
        // own title. On exit it restores with ESC[23;0t.
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());

        // Set initial title, save it, then set a new title (Vim's title)
        parser.process(b"\x1b]2;original\x1b\\");
        parser.process(b"\x1b[22;0t");
        parser.process(b"\x1b]2;vim - file.txt\x1b\\");

        assert_eq!(
            parser.callbacks().window_title_stack,
            vec!["original"],
            "title stack should contain the saved title"
        );
        assert_eq!(
            parser.callbacks().window_title.as_deref(),
            Some("vim - file.txt"),
            "current title should be updated"
        );

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());
        let snapshot_str = String::from_utf8_lossy(&snapshot);

        // Snapshot must contain both: the stacked title with a save command,
        // and the current title.
        assert!(
            snapshot_str.contains("original"),
            "stacked title should appear in snapshot"
        );
        assert!(
            snapshot_str.contains("\x1b[22;0t"),
            "title save command should appear in snapshot"
        );
        assert!(
            snapshot_str.contains("vim - file.txt"),
            "current title should appear in snapshot"
        );
    }

    #[test]
    fn window_title_restore_pops_stack() {
        // ESC[23;0t restores the previously saved title.
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());

        parser.process(b"\x1b]2;original\x1b\\");
        parser.process(b"\x1b[22;0t");
        parser.process(b"\x1b]2;vim\x1b\\");
        parser.process(b"\x1b[23;0t");

        assert!(
            parser.callbacks().window_title_stack.is_empty(),
            "stack should be empty after restore"
        );
        assert_eq!(
            parser.callbacks().window_title.as_deref(),
            Some("original"),
            "title should be restored to original"
        );
    }

    #[test]
    fn snapshot_decscusr_comes_after_state_formatted() {
        // DECSCUSR must appear after state_formatted() output so it is not
        // overwritten by the cursor-positioning sequences that state_formatted()
        // emits at the end.
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 1000, SessionCallbacks::default());
        parser.process(b"\x1b[6 q");

        let snapshot = build_snapshot(parser.screen(), parser.callbacks());

        // state_formatted() ends with a cursor-position sequence (ESC[...H or
        // similar).  Find DECSCUSR and the last ESC occurrence before it to
        // confirm ordering.
        let decscusr_pos = snapshot
            .windows(5)
            .position(|w| w == b"\x1b[6 q")
            .expect("DECSCUSR not found");

        // ESC[H (home) or ESC[r;cH appears in state_formatted output.
        // Any ESC before decscusr_pos indicates state_formatted came first.
        let has_esc_before = snapshot[..decscusr_pos].contains(&0x1b);
        assert!(
            has_esc_before,
            "state_formatted() sequences should appear before DECSCUSR in snapshot"
        );
    }
}
