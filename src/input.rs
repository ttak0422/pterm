//! Bounded input adapters for managed terminals. These deliberately recognize
//! only the private origin marker, SGR mouse reports, and paste delimiters.

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

const ORIGIN_PREFIX: &[u8] = b"\x1b]51;pterm-input-origin;";
const MOUSE_PREFIX: &[u8] = b"\x1b[<";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const MAX_PENDING: usize = 128;
const MAX_LEFTCOL: i32 = 1_000_000;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputEvent {
    Bytes(Vec<u8>),
    Origin { row_base: i32, leftcol: i32 },
}

/// Call `flush_pending` after 25 ms without completing a candidate. In
/// particular, an ordinary Escape key must not wait for another input byte.
#[derive(Default)]
pub(crate) struct ManagedInput {
    scanner: Scanner,
}

impl ManagedInput {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn decode(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        let mut events = Vec::new();
        for token in self.scanner.push(bytes, Target::Origin) {
            match token {
                Token::Bytes(bytes) => append_bytes(&mut events, bytes),
                Token::Candidate(bytes) => match parse_origin(&bytes) {
                    Some((row_base, leftcol)) => {
                        events.push(InputEvent::Origin { row_base, leftcol });
                    }
                    None => append_bytes(&mut events, bytes),
                },
            }
        }
        events
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.scanner.pending.is_empty()
    }

    pub(crate) fn flush_pending(&mut self) -> Vec<InputEvent> {
        let pending = self.scanner.flush_pending();
        if pending.is_empty() {
            Vec::new()
        } else {
            vec![InputEvent::Bytes(pending)]
        }
    }
}

fn append_bytes(events: &mut Vec<InputEvent>, bytes: Vec<u8>) {
    if bytes.is_empty() {
        return;
    }
    if let Some(InputEvent::Bytes(previous)) = events.last_mut() {
        previous.extend(bytes);
    } else {
        events.push(InputEvent::Bytes(bytes));
    }
}

fn parse_origin(bytes: &[u8]) -> Option<(i32, i32)> {
    let body = bytes.strip_prefix(ORIGIN_PREFIX)?.strip_suffix(b"\x07")?;
    let text = std::str::from_utf8(body).ok()?;
    let (row, col) = text.split_once(';')?;
    let digits = row.strip_prefix('-').unwrap_or(row);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if col.is_empty() || !col.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let row_base = row.parse::<i32>().ok()?;
    let leftcol = col.parse::<i32>().ok()?;
    (leftcol <= MAX_LEFTCOL).then_some((row_base, leftcol))
}

#[derive(Clone, Copy)]
enum Target {
    Origin,
    Mouse,
}

impl Target {
    fn prefix(self) -> &'static [u8] {
        match self {
            Self::Origin => ORIGIN_PREFIX,
            Self::Mouse => MOUSE_PREFIX,
        }
    }

    fn terminator(self, byte: u8) -> bool {
        match self {
            Self::Origin => byte == 7,
            Self::Mouse => byte == b'M' || byte == b'm',
        }
    }

    fn body_byte(self, byte: u8) -> bool {
        byte.is_ascii_digit() || byte == b';' || (matches!(self, Self::Origin) && byte == b'-')
    }
}

enum Token {
    Bytes(Vec<u8>),
    Candidate(Vec<u8>),
}

/// Paste delimiters are observed independently of buffering, so even a
/// delimiter split across a timeout remains protected. Paste bytes themselves
/// are never buffered. All ordinary bytes are passed through in original order.
#[derive(Default)]
struct Scanner {
    pending: Vec<u8>,
    in_paste: bool,
    delimiter_matched: usize,
}

impl Scanner {
    fn observe_delimiter(&mut self, byte: u8) {
        let delimiter = if self.in_paste {
            PASTE_END
        } else {
            PASTE_START
        };
        if byte == delimiter[self.delimiter_matched] {
            self.delimiter_matched += 1;
            if self.delimiter_matched == delimiter.len() {
                self.in_paste = !self.in_paste;
                self.delimiter_matched = 0;
            }
        } else {
            // Escape occurs only at the start of either delimiter.
            self.delimiter_matched = usize::from(byte == b'\x1b');
        }
    }

    fn push_byte(tokens: &mut Vec<Token>, byte: u8) {
        if let Some(Token::Bytes(previous)) = tokens.last_mut() {
            previous.push(byte);
        } else {
            tokens.push(Token::Bytes(vec![byte]));
        }
    }

    fn push(&mut self, bytes: &[u8], target: Target) -> Vec<Token> {
        let mut tokens = Vec::new();
        for &byte in bytes {
            let was_paste = self.in_paste;
            self.observe_delimiter(byte);
            if was_paste || self.in_paste {
                for pending in self.flush_pending() {
                    Self::push_byte(&mut tokens, pending);
                }
                Self::push_byte(&mut tokens, byte);
                continue;
            }
            self.pending.push(byte);
            loop {
                if self.pending.is_empty() {
                    break;
                }
                let prefix = target.prefix();
                if prefix.starts_with(&self.pending) {
                    break;
                }
                if self.pending.starts_with(prefix) {
                    let last = *self.pending.last().unwrap();
                    if target.terminator(last) {
                        tokens.push(Token::Candidate(self.flush_pending()));
                        break;
                    }
                    if self.pending.len() < MAX_PENDING && target.body_byte(last) {
                        break;
                    }
                }
                // Release only the first byte, then reconsider the suffix. This
                // preserves unknown escapes without hiding a following report.
                Self::push_byte(&mut tokens, self.pending.remove(0));
            }
        }
        tokens
    }

    fn flush_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn passthrough(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut output = self.flush_pending();
        for &byte in bytes {
            self.observe_delimiter(byte);
        }
        output.extend_from_slice(bytes);
        output
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    x: u16,
    y: u16,
}

#[derive(Clone, Copy)]
struct ActiveButton {
    id: u32,
    last: Point,
}

#[derive(Clone, Copy)]
struct Report {
    button: u32,
    x: u32,
    y: u32,
    release: bool,
}

fn parse_report(bytes: &[u8]) -> Option<Report> {
    let body = bytes.strip_prefix(MOUSE_PREFIX)?;
    let (&terminator, body) = body.split_last()?;
    if terminator != b'M' && terminator != b'm' {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    let mut fields = text.split(';');
    let mut number = || {
        let field = fields.next()?;
        if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        field.parse::<u32>().ok()
    };
    let button = number()?;
    let x = number()?;
    let y = number()?;
    if fields.next().is_some() || button > 255 {
        return None;
    }
    Some(Report {
        button,
        x,
        y,
        release: terminator == b'm',
    })
}

/// Per-client mouse state. Origins and pressed buttons must never be shared
/// between clients. Like `ManagedInput`, partial candidates need a 25 ms flush.
#[derive(Default)]
pub(crate) struct MouseInput {
    scanner: Scanner,
    origin: Option<(i32, i32)>,
    // The accepted button identities are bounded to seven non-wheel buttons.
    active: Vec<ActiveButton>,
}

impl MouseInput {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_origin(&mut self, row_base: i32, leftcol: i32) {
        self.origin = (0..=MAX_LEFTCOL)
            .contains(&leftcol)
            .then_some((row_base, leftcol));
    }

    pub(crate) fn invalidate_origin(&mut self) {
        self.origin = None;
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.scanner.pending.is_empty()
    }

    pub(crate) fn flush_pending(&mut self) -> Vec<u8> {
        self.scanner.flush_pending()
    }

    pub(crate) fn transform(
        &mut self,
        bytes: &[u8],
        canonical_cols: u16,
        canonical_rows: u16,
        physical_rows: u16,
        mode: MouseProtocolMode,
        encoding: MouseProtocolEncoding,
    ) -> Vec<u8> {
        if mode == MouseProtocolMode::None {
            self.active.clear();
            return self.scanner.passthrough(bytes);
        }
        let mut output = Vec::new();
        for token in self.scanner.push(bytes, Target::Mouse) {
            match token {
                Token::Bytes(bytes) => output.extend(bytes),
                Token::Candidate(bytes) => match parse_report(&bytes) {
                    Some(report) => output.extend(self.translate(
                        report,
                        canonical_cols,
                        canonical_rows,
                        physical_rows,
                        encoding,
                    )),
                    None => output.extend(bytes),
                },
            }
        }
        output
    }

    fn translate(
        &mut self,
        report: Report,
        cols: u16,
        rows: u16,
        physical_rows: u16,
        encoding: MouseProtocolEncoding,
    ) -> Vec<u8> {
        let id = report.button & 0xc3;
        let wheel = report.button & 64 != 0;
        let motion = report.button & 32 != 0;
        let active_index = self.active.iter().position(|button| button.id == id);
        // Button 3 is the legacy "no button" identity, not an extra button.
        let release_index = active_index.or_else(|| {
            (report.release && id == 3)
                .then(|| self.active.len().checked_sub(1))
                .flatten()
        });
        if report.release && release_index.is_none() {
            return Vec::new();
        }
        // A drag whose initial press was dropped in history/padding must not
        // suddenly become an application drag when it enters the live screen.
        if motion && !wheel && id != 3 && active_index.is_none() {
            return Vec::new();
        }
        let previous = release_index.map(|index| self.active[index].last);
        let point = match self.origin {
            Some((row_base, leftcol)) if cols != 0 && rows != 0 => {
                let x = i64::from(report.x) + i64::from(leftcol);
                let y =
                    i64::from(report.y) + i64::from(row_base) + i64::from(rows.min(physical_rows));
                let inside = report.x != 0
                    && report.y != 0
                    && (1..=i64::from(cols)).contains(&x)
                    && (1..=i64::from(rows)).contains(&y);
                if inside {
                    Point {
                        x: x as u16,
                        y: y as u16,
                    }
                } else if report.release || (motion && active_index.is_some()) {
                    Point {
                        x: x.clamp(1, i64::from(cols)) as u16,
                        y: y.clamp(1, i64::from(rows)) as u16,
                    }
                } else {
                    return Vec::new();
                }
            }
            _ if report.release => {
                let previous = previous.unwrap();
                Point {
                    x: previous.x.min(cols.max(1)),
                    y: previous.y.min(rows.max(1)),
                }
            }
            _ => return Vec::new(),
        };
        let (point, encoded) = match encode(report, point, encoding) {
            Some(encoded) => (point, encoded),
            None if report.release => {
                // An application may switch encoding or shrink its dimensions
                // during a drag. Preserve its release at a representable point.
                let old = previous.unwrap();
                let limit = encoding_limit(encoding);
                let fallback = Point {
                    x: old.x.min(cols.max(1)).min(limit),
                    y: old.y.min(rows.max(1)).min(limit),
                };
                match encode(report, fallback, encoding) {
                    Some(encoded) => (fallback, encoded),
                    None => return Vec::new(),
                }
            }
            None => return Vec::new(),
        };
        if report.release {
            if id == 3 {
                self.active.clear();
            } else {
                self.active.remove(release_index.unwrap());
            }
        } else if !wheel && id != 3 {
            if let Some(index) = active_index {
                self.active[index].last = point;
            } else if !motion {
                self.active.push(ActiveButton { id, last: point });
            }
        }
        encoded
    }
}

fn encoding_limit(encoding: MouseProtocolEncoding) -> u16 {
    match encoding {
        MouseProtocolEncoding::Default => 223,
        // Xterm's 1005 protocol uses at most two UTF-8 bytes per coordinate.
        MouseProtocolEncoding::Utf8 => 2015,
        MouseProtocolEncoding::Sgr => u16::MAX,
    }
}

fn encode(report: Report, point: Point, encoding: MouseProtocolEncoding) -> Option<Vec<u8>> {
    if point.x == 0
        || point.y == 0
        || point.x > encoding_limit(encoding)
        || point.y > encoding_limit(encoding)
    {
        return None;
    }
    if encoding == MouseProtocolEncoding::Sgr {
        return Some(
            format!(
                "\x1b[<{};{};{}{}",
                report.button,
                point.x,
                point.y,
                if report.release { 'm' } else { 'M' },
            )
            .into_bytes(),
        );
    }
    // Legacy releases use button 3; preserve Shift/Meta/Control modifiers.
    let button = if report.release {
        (report.button & 28) | 3
    } else {
        report.button
    };
    let codes = [
        button + 32,
        u32::from(point.x) + 32,
        u32::from(point.y) + 32,
    ];
    let mut output = b"\x1b[M".to_vec();
    for code in codes {
        match encoding {
            MouseProtocolEncoding::Default => output.push(u8::try_from(code).ok()?),
            MouseProtocolEncoding::Utf8 => {
                let mut buf = [0; 4];
                output.extend_from_slice(char::from_u32(code)?.encode_utf8(&mut buf).as_bytes());
            }
            MouseProtocolEncoding::Sgr => unreachable!(),
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(row: &str, col: &str) -> Vec<u8> {
        format!("\x1b]51;pterm-input-origin;{row};{col}\x07").into_bytes()
    }

    fn collect_events(parts: impl IntoIterator<Item = Vec<InputEvent>>) -> Vec<InputEvent> {
        let mut all = Vec::new();
        for part in parts {
            for event in part {
                match event {
                    InputEvent::Bytes(bytes) => append_bytes(&mut all, bytes),
                    event => all.push(event),
                }
            }
        }
        all
    }

    fn identity_mouse() -> MouseInput {
        let mut mouse = MouseInput::new();
        mouse.set_origin(-24, 0);
        mouse
    }

    fn sgr(mouse: &mut MouseInput, bytes: &[u8]) -> Vec<u8> {
        mouse.transform(
            bytes,
            80,
            24,
            24,
            MouseProtocolMode::AnyMotion,
            MouseProtocolEncoding::Sgr,
        )
    }

    #[test]
    fn managed_markers_are_ordered_and_stripped_across_every_split() {
        let mut input = b"a\x00\xff\x1b[A".to_vec();
        input.extend(marker("-24", "7"));
        input.extend(b"z\x1b[<0;1;2M");
        input.extend(marker("2147483647", "1000000"));
        input.extend(b"last");
        let expected = vec![
            InputEvent::Bytes(b"a\x00\xff\x1b[A".to_vec()),
            InputEvent::Origin {
                row_base: -24,
                leftcol: 7,
            },
            InputEvent::Bytes(b"z\x1b[<0;1;2M".to_vec()),
            InputEvent::Origin {
                row_base: i32::MAX,
                leftcol: MAX_LEFTCOL,
            },
            InputEvent::Bytes(b"last".to_vec()),
        ];
        for split in 0..=input.len() {
            let mut decoder = ManagedInput::new();
            let parts = vec![
                decoder.decode(&input[..split]),
                decoder.decode(&input[split..]),
                decoder.flush_pending(),
            ];
            assert_eq!(collect_events(parts), expected, "split {split}");
        }
        let mut decoder = ManagedInput::new();
        let parts: Vec<_> = input.iter().map(|byte| decoder.decode(&[*byte])).collect();
        assert_eq!(collect_events(parts), expected);
        assert_eq!(
            decoder.decode(&marker("-2147483648", "0")),
            vec![InputEvent::Origin {
                row_base: i32::MIN,
                leftcol: 0
            },]
        );
    }

    #[test]
    fn malformed_markers_and_unknown_keyboard_bytes_survive() {
        let cases = [
            marker("", "0"),
            marker("-", "0"),
            marker("+1", "0"),
            marker("1", "-1"),
            marker("1", "+1"),
            marker("1", "1000001"),
            marker("2147483648", "0"),
            marker("-2147483649", "0"),
            marker("1", "2147483648"),
            marker("1;2", "3"),
            marker(" 1", "0"),
            marker("1", ""),
            marker("1", "2\n"),
            b"\x1b]52;pterm-input-origin;1;2\x07".to_vec(),
            b"\x1b]51;pterm-input-origin;1;2\x1b\\".to_vec(),
            b"\x1b[1;5A\x1bOP\x00\xff\r\n".to_vec(),
        ];
        for input in cases {
            for split in 0..=input.len() {
                let mut decoder = ManagedInput::new();
                let parts = vec![
                    decoder.decode(&input[..split]),
                    decoder.decode(&input[split..]),
                    decoder.flush_pending(),
                ];
                assert_eq!(
                    collect_events(parts),
                    vec![InputEvent::Bytes(input.clone())],
                    "{input:?}, split {split}"
                );
            }
        }
    }

    #[test]
    fn managed_pending_is_bounded_and_can_be_flushed_on_timeout() {
        let mut decoder = ManagedInput::new();
        assert!(decoder.decode(b"\x1b").is_empty());
        assert!(decoder.has_pending());
        assert_eq!(decoder.flush_pending(), vec![InputEvent::Bytes(vec![27])]);
        assert!(!decoder.has_pending());
        assert!(decoder.flush_pending().is_empty());
        let mut input = ORIGIN_PREFIX.to_vec();
        input.extend(vec![b'0'; 2000]);
        input.extend(b";1\x07");
        let mut parts = Vec::new();
        for byte in &input {
            parts.push(decoder.decode(&[*byte]));
            assert!(decoder.scanner.pending.len() <= MAX_PENDING);
        }
        parts.push(decoder.flush_pending());
        assert_eq!(collect_events(parts), vec![InputEvent::Bytes(input)]);
    }

    #[test]
    fn managed_paste_protects_markers_and_mouse_at_every_split() {
        let mut paste = PASTE_START.to_vec();
        paste.extend(marker("-24", "7"));
        paste.extend(b"\x1b[<0;1;2M\x1b[20x\x1b\x1b[201~");
        let mut input = paste.clone();
        input.extend(marker("-6", "2"));
        for split in 0..=input.len() {
            let mut decoder = ManagedInput::new();
            let parts = vec![
                decoder.decode(&input[..split]),
                decoder.decode(&input[split..]),
                decoder.flush_pending(),
            ];
            assert_eq!(
                collect_events(parts),
                vec![
                    InputEvent::Bytes(paste.clone()),
                    InputEvent::Origin {
                        row_base: -6,
                        leftcol: 2
                    },
                ],
                "split {split}"
            );
        }
    }

    #[test]
    fn paste_delimiters_survive_keyboard_timeouts() {
        let mut decoder = ManagedInput::new();
        let mut observed = Vec::new();
        for byte in PASTE_START {
            observed.push(decoder.decode(&[*byte]));
            observed.push(decoder.flush_pending());
        }
        let pasted_marker = marker("-24", "3");
        observed.push(decoder.decode(&pasted_marker));
        for byte in PASTE_END {
            observed.push(decoder.decode(&[*byte]));
            observed.push(decoder.flush_pending());
        }
        let mut expected = PASTE_START.to_vec();
        expected.extend(pasted_marker);
        expected.extend(PASTE_END);
        assert_eq!(collect_events(observed), vec![InputEvent::Bytes(expected)]);
        assert_eq!(
            decoder.decode(&marker("-24", "3")),
            vec![InputEvent::Origin {
                row_base: -24,
                leftcol: 3
            },]
        );
    }

    #[test]
    fn sgr_press_drag_release_and_modifiers_survive_every_split() {
        let input = b"key\x1b[<20;3;4M\x1b[<52;4;5M\x1b[<20;6;7m\x1b[Aend";
        let expected = b"key\x1b[<20;5;4M\x1b[<52;6;5M\x1b[<20;8;7m\x1b[Aend";
        for split in 0..=input.len() {
            let mut mouse = identity_mouse();
            mouse.set_origin(-24, 2);
            let mut output = sgr(&mut mouse, &input[..split]);
            output.extend(sgr(&mut mouse, &input[split..]));
            output.extend(mouse.flush_pending());
            assert_eq!(output, expected, "split {split}");
            assert!(mouse.active.is_empty());
        }
        let mut mouse = identity_mouse();
        mouse.set_origin(-24, 2);
        let output: Vec<_> = input
            .iter()
            .flat_map(|byte| sgr(&mut mouse, &[*byte]))
            .collect();
        assert_eq!(output, expected);
    }

    #[test]
    fn legacy_default_encoding_preserves_modifiers_and_release() {
        let mut mouse = identity_mouse();
        let output = mouse.transform(
            b"\x1b[<28;4;5M\x1b[<60;5;6M\x1b[<28;6;7m\x1b[<92;2;3M",
            80,
            24,
            24,
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Default,
        );
        assert_eq!(output, b"\x1b[M<$%\x1b[M\\%&\x1b[M?&'\x1b[M|\"#");
        assert!(mouse.active.is_empty());
    }

    #[test]
    fn utf8_encoding_preserves_modifiers_and_release() {
        let mut mouse = MouseInput::new();
        mouse.set_origin(-400, 5);
        let output = mouse.transform(
            b"\x1b[<28;95;110M\x1b[<60;96;111M\x1b[<28;97;112m",
            400,
            400,
            400,
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Utf8,
        );
        assert_eq!(
            output,
            b"\x1b[M<\xc2\x84\xc2\x8e\x1b[M\\\xc2\x85\xc2\x8f\x1b[M?\xc2\x86\xc2\x90"
        );
        assert!(mouse.active.is_empty());
    }

    #[test]
    fn viewport_origins_cover_active_passive_and_independent_crops() {
        // canonical rows, physical rows, row base, native row, canonical row
        let cases = [
            (6, 12, -6, 1, 1),  // Short active window follows the live tail.
            (6, 12, -12, 7, 1), // Tall passive window includes top padding.
            (6, 12, -3, 1, 4),  // Short passive window shows lower live rows.
            (12, 6, -6, 1, 1),  // Independent shorter renderer crops top-left.
            (6, 12, -8, 3, 1),  // Independently scrolled window with two history rows.
        ];
        for (rows, physical, origin, native_y, expected_y) in cases {
            let mut mouse = MouseInput::new();
            mouse.set_origin(origin, 4);
            let input = format!("\x1b[<0;2;{native_y}M");
            assert_eq!(
                mouse.transform(
                    input.as_bytes(),
                    20,
                    rows,
                    physical,
                    MouseProtocolMode::PressRelease,
                    MouseProtocolEncoding::Sgr
                ),
                format!("\x1b[<0;6;{expected_y}M").into_bytes()
            );
        }
    }

    #[test]
    fn history_padding_and_horizontal_overflow_drop_new_mouse_actions() {
        let mut mouse = MouseInput::new();
        mouse.set_origin(-12, 0);
        let events =
            b"\x1b[<0;1;1M\x1b[<64;1;6M\x1b[<35;1;6M\x1b[<0;1;13M\x1b[<0;21;7M\x1b[<0;0;7M";
        assert!(mouse
            .transform(
                events,
                20,
                6,
                12,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr
            )
            .is_empty());
        assert!(mouse.active.is_empty());
        // Drag/release from a dropped padding press remain suppressed inside.
        assert!(mouse
            .transform(
                b"\x1b[<32;1;8M\x1b[<0;1;8m",
                20,
                6,
                12,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr
            )
            .is_empty());
        mouse.set_origin(i32::MIN, MAX_LEFTCOL);
        assert!(mouse
            .transform(
                b"\x1b[<0;4294967295;4294967295M",
                20,
                6,
                12,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr
            )
            .is_empty());
    }

    #[test]
    fn active_drag_and_release_are_clamped_when_pointer_leaves_live_rectangle() {
        let mut mouse = identity_mouse();
        assert_eq!(sgr(&mut mouse, b"\x1b[<20;4;5M"), b"\x1b[<20;4;5M");
        mouse.set_origin(-30, 0);
        assert_eq!(sgr(&mut mouse, b"\x1b[<52;2;1M"), b"\x1b[<52;2;1M");
        assert_eq!(sgr(&mut mouse, b"\x1b[<20;999;999m"), b"\x1b[<20;80;24m");
        assert!(mouse.active.is_empty());
        assert!(sgr(&mut mouse, b"\x1b[<20;3;10m").is_empty());
    }

    #[test]
    fn no_origin_drops_presses_and_motion_but_releases_at_last_valid_point() {
        let mut mouse = MouseInput::new();
        assert!(sgr(&mut mouse, b"\x1b[<0;4;5M\x1b[<35;6;7M").is_empty());
        mouse.set_origin(-24, 0);
        assert_eq!(
            sgr(&mut mouse, b"\x1b[<0;4;5M\x1b[<32;6;7M"),
            b"\x1b[<0;4;5M\x1b[<32;6;7M"
        );
        mouse.invalidate_origin();
        assert!(sgr(&mut mouse, b"\x1b[<32;9;10M\x1b[<1;3;4M").is_empty());
        assert_eq!(sgr(&mut mouse, b"\x1b[<16;999;999m"), b"\x1b[<16;6;7m");
        assert!(mouse.active.is_empty());
    }

    #[test]
    fn invalid_origin_preserves_release_after_resize() {
        let mut mouse = identity_mouse();
        sgr(&mut mouse, b"\x1b[<0;40;20M");
        mouse.set_origin(-24, -1);
        assert_eq!(
            mouse.transform(
                b"\x1b[<0;40;20m",
                10,
                5,
                5,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr
            ),
            b"\x1b[<0;10;5m"
        );
    }

    #[test]
    fn multiple_buttons_are_tracked_independently_and_wheels_do_not_latch() {
        let mut mouse = identity_mouse();
        sgr(
            &mut mouse,
            b"\x1b[<0;1;2M\x1b[<1;3;4M\x1b[<128;5;6M\x1b[<64;7;8M",
        );
        assert_eq!(mouse.active.len(), 3);
        mouse.invalidate_origin();
        assert_eq!(
            sgr(&mut mouse, b"\x1b[<1;99;99m\x1b[<128;99;99m\x1b[<0;99;99m"),
            b"\x1b[<1;3;4m\x1b[<128;5;6m\x1b[<0;1;2m"
        );
        assert!(mouse.active.is_empty());
    }

    #[test]
    fn legacy_coordinate_limits_drop_unrepresentable_reports_but_release_safely() {
        for (encoding, limit) in [
            (MouseProtocolEncoding::Default, 223),
            (MouseProtocolEncoding::Utf8, 2015),
        ] {
            let mut mouse = MouseInput::new();
            mouse.set_origin(-3000, 0);
            let encode_input = |button, x, end| format!("\x1b[<{button};{x};1{end}");
            let run = |mouse: &mut MouseInput, button, x, end| {
                mouse.transform(
                    encode_input(button, x, end).as_bytes(),
                    3000,
                    3000,
                    3000,
                    MouseProtocolMode::ButtonMotion,
                    encoding,
                )
            };
            assert!(run(&mut mouse, 0, limit + 1, 'M').is_empty());
            assert!(mouse.active.is_empty());
            let expected_press = encode(
                Report {
                    button: 0,
                    x: 0,
                    y: 0,
                    release: false,
                },
                Point { x: limit, y: 1 },
                encoding,
            )
            .unwrap();
            assert_eq!(run(&mut mouse, 0, limit, 'M'), expected_press);
            assert!(run(&mut mouse, 32, limit + 1, 'M').is_empty());
            let expected_release = encode(
                Report {
                    button: 20,
                    x: 0,
                    y: 0,
                    release: true,
                },
                Point { x: limit, y: 1 },
                encoding,
            )
            .unwrap();
            assert_eq!(run(&mut mouse, 20, limit + 1, 'm'), expected_release);
            assert!(mouse.active.is_empty());
        }
    }

    #[test]
    fn encoding_changes_do_not_lose_an_active_release() {
        let mut mouse = MouseInput::new();
        mouse.set_origin(-400, 0);
        assert_eq!(
            mouse.transform(
                b"\x1b[<0;300;300M",
                400,
                400,
                400,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr
            ),
            b"\x1b[<0;300;300M"
        );
        mouse.invalidate_origin();
        assert_eq!(
            mouse.transform(
                b"\x1b[<0;300;300m",
                400,
                400,
                400,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default
            ),
            b"\x1b[M#\xff\xff"
        );
        assert!(mouse.active.is_empty());
    }

    #[test]
    fn malformed_mouse_unknown_escapes_and_keyboard_bytes_survive_every_split() {
        let cases: &[&[u8]] = &[
            b"\x1b[<;1;2M",
            b"\x1b[<0;;2M",
            b"\x1b[<0;1;m",
            b"\x1b[<0;1;2;3M",
            b"\x1b[<256;1;2M",
            b"\x1b[<0;4294967296;2M",
            b"\x1b[<0;-1;2M",
            b"\x1b[<0;1;2x",
            b"\x1b[<0;1;2",
            b"\x1b[A\x1bOP\x00\xff\r\n",
            b"\x1b]51;pterm-input-origin;-24;7\x07",
            b"\x1b[M !!",
        ];
        for &input in cases {
            for split in 0..=input.len() {
                let mut mouse = identity_mouse();
                let mut output = sgr(&mut mouse, &input[..split]);
                output.extend(sgr(&mut mouse, &input[split..]));
                output.extend(mouse.flush_pending());
                assert_eq!(output, input, "{input:?}, split {split}");
            }
        }
    }

    #[test]
    fn mouse_pending_is_bounded_and_lone_escape_can_be_flushed() {
        let mut mouse = identity_mouse();
        assert!(sgr(&mut mouse, b"\x1b").is_empty());
        assert!(mouse.has_pending());
        assert_eq!(mouse.flush_pending(), b"\x1b");
        assert!(!mouse.has_pending());
        let mut input = MOUSE_PREFIX.to_vec();
        input.extend(vec![b'0'; 2000]);
        input.extend(b";1;2M");
        let mut output = Vec::new();
        for byte in &input {
            output.extend(sgr(&mut mouse, &[*byte]));
            assert!(mouse.scanner.pending.len() <= MAX_PENDING);
        }
        output.extend(mouse.flush_pending());
        assert_eq!(output, input);
    }

    #[test]
    fn mouse_paste_protects_reports_and_markers_at_every_split() {
        let mut paste = PASTE_START.to_vec();
        paste.extend(b"\x1b[<0;1;2M\x1b[<32;2;3M\x1b[<0;2;3m");
        paste.extend(marker("-24", "7"));
        paste.extend(PASTE_END);
        let mut input = paste.clone();
        input.extend(b"\x1b[<0;3;4M");
        let mut expected = paste;
        expected.extend(b"\x1b[<0;5;4M");
        for split in 0..=input.len() {
            let mut mouse = identity_mouse();
            mouse.set_origin(-24, 2);
            let mut output = sgr(&mut mouse, &input[..split]);
            output.extend(sgr(&mut mouse, &input[split..]));
            output.extend(mouse.flush_pending());
            assert_eq!(output, expected, "split {split}");
            assert_eq!(mouse.active.len(), 1);
        }
    }

    #[test]
    fn mode_none_is_immediate_raw_passthrough_and_keeps_paste_protected() {
        let mut mouse = identity_mouse();
        let input = b"\x1b[<0;1;2M\x1b[200~\x1b]51;pterm-input-origin;-24;7\x07\x1b";
        for byte in input {
            assert_eq!(
                mouse.transform(
                    &[*byte],
                    80,
                    24,
                    24,
                    MouseProtocolMode::None,
                    MouseProtocolEncoding::Default
                ),
                vec![*byte]
            );
            assert!(!mouse.has_pending());
        }
        assert_eq!(
            sgr(&mut mouse, b"\x1b[<0;1;2M\x1b[201~"),
            b"\x1b[<0;1;2M\x1b[201~"
        );
        assert!(mouse.active.is_empty());
        assert!(sgr(&mut mouse, b"\x1b[").is_empty());
        assert_eq!(
            mouse.transform(
                b"<0;1;2M",
                80,
                24,
                24,
                MouseProtocolMode::None,
                MouseProtocolEncoding::Default
            ),
            b"\x1b[<0;1;2M"
        );
    }
}
