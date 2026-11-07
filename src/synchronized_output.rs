//! Best-effort coalescing, not an atomic terminal write guarantee. PTYs and
//! consumers may still split a completed update. Never change the byte stream.
use crate::server::SendQueue;
use std::io;
use std::time::{Duration, Instant};

const MAX_UPDATE_BYTES: usize = 256 * 1024;
const MAX_HOLD: Duration = Duration::from_millis(25);
const BEGIN: &[u8] = b"\x1b[?2026h";
const END: &[u8] = b"\x1b[?2026l";

#[derive(Default)]
enum Scan {
    #[default]
    Ground,
    Escape,
    Csi,
    String,
    StringEscape,
}

/// Per bridge, after history-reset ordering and before stdout backpressure.
/// Only the canonical seven-bit DEC 2026 spelling is coalesced. Other forms
/// pass through unchanged. OSC/DCS/APC/PM/SOS payloads are not interpreted.
#[derive(Default)]
pub(crate) struct SynchronizedOutput {
    ready: SendQueue,
    held: Vec<u8>,
    prefix: Vec<u8>,
    scan: Scan,
    osc: bool,
    active: bool,
    bypass: bool,
    since: Option<Instant>,
}

impl SynchronizedOutput {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.push_at(bytes, Instant::now());
    }

    fn push_at(&mut self, bytes: &[u8], now: Instant) {
        self.expire(now);
        // Accumulate ordinary bytes too, so this does not turn plain output
        // into one allocation/write per byte.
        let mut released = Vec::new();
        for &byte in bytes {
            match self.scan {
                Scan::Escape | Scan::Csi if byte == 0x1b => {
                    self.release_prefix(&mut released);
                    self.prefix.push(byte);
                    self.scan = Scan::Escape;
                }
                Scan::Ground if byte == 0x1b => {
                    self.scan = Scan::Escape;
                    self.prefix.push(byte);
                }
                Scan::Escape => {
                    self.prefix.push(byte);
                    self.osc = byte == b']';
                    self.scan = match byte {
                        b'[' => Scan::Csi,
                        b']' | b'P' | b'_' | b'^' | b'X' => Scan::String,
                        0x1b => Scan::Escape,
                        _ => Scan::Ground,
                    };
                    if !matches!(self.scan, Scan::Csi) {
                        self.release_prefix(&mut released);
                    }
                }
                Scan::Csi if self.prefix.is_empty() => {
                    self.emit(byte, &mut released);
                    if (0x40..=0x7e).contains(&byte) {
                        self.scan = Scan::Ground;
                    }
                }
                Scan::Csi => {
                    self.prefix.push(byte);
                    if !BEGIN.starts_with(&self.prefix) && !END.starts_with(&self.prefix) {
                        // Not a supported delimiter. Continue scanning CSI to
                        // its final byte, without retaining arbitrary parameters.
                        self.release_prefix(&mut released);
                    }
                    if (0x40..=0x7e).contains(&byte) {
                        if self.prefix == BEGIN && !self.active {
                            self.active = true;
                            self.since = Some(now);
                        }
                        let end = self.prefix == END;
                        self.release_prefix(&mut released);
                        if end {
                            released.append(&mut self.held);
                            self.active = false;
                            self.bypass = false;
                        }
                        self.scan = Scan::Ground;
                    }
                }
                Scan::String => {
                    self.emit(byte, &mut released);
                    if byte == 0x1b {
                        self.scan = Scan::StringEscape;
                    } else if byte == 7 && self.osc {
                        self.scan = Scan::Ground;
                    }
                }
                Scan::StringEscape => {
                    self.emit(byte, &mut released);
                    self.scan = match byte {
                        b'\\' => Scan::Ground,
                        7 if self.osc => Scan::Ground,
                        0x1b => Scan::StringEscape,
                        _ => Scan::String,
                    };
                }
                Scan::Ground => self.emit(byte, &mut released),
            }
            if self.held.len() >= MAX_UPDATE_BYTES {
                released.append(&mut self.held);
                self.bypass = true;
            }
            if self.held.is_empty() && self.prefix.is_empty() {
                self.since = None;
            } else {
                self.since.get_or_insert(now);
            }
        }
        if !released.is_empty() {
            self.ready.push(&released);
        }
    }

    fn emit(&mut self, byte: u8, released: &mut Vec<u8>) {
        if self.active && !self.bypass {
            self.held.push(byte);
        } else {
            released.push(byte);
        }
    }

    fn release_prefix(&mut self, released: &mut Vec<u8>) {
        let prefix = std::mem::take(&mut self.prefix);
        for byte in prefix {
            self.emit(byte, released);
        }
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        if self
            .since
            .is_some_and(|since| now.duration_since(since) >= MAX_HOLD)
        {
            self.flush();
        }
    }

    /// Flush before detach cleanup, on EOF, or after a deadline. Keep scanning
    /// but do not restart a timed-out update until its closing delimiter.
    pub(crate) fn flush(&mut self) {
        self.held.append(&mut self.prefix);
        if !self.held.is_empty() {
            self.ready.push(&self.held);
            self.held.clear();
        }
        self.bypass = self.active;
        self.since = None;
    }

    pub(crate) fn poll_timeout(&self, timeout: i32, now: Instant) -> i32 {
        let Some(since) = self.since else {
            return timeout;
        };
        let remaining = MAX_HOLD.saturating_sub(now.duration_since(since));
        let millis = remaining.as_nanos().div_ceil(1_000_000) as i32;
        if timeout < 0 {
            millis
        } else {
            timeout.min(millis)
        }
    }

    #[cfg(test)]
    fn holding(&self) -> bool {
        self.since.is_some()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    pub(crate) fn pending_bytes(&self) -> usize {
        self.ready.pending_bytes() + self.held.len() + self.prefix.len()
    }

    pub(crate) fn write_with(
        &mut self,
        write: impl FnMut(&[u8]) -> io::Result<usize>,
    ) -> io::Result<()> {
        self.ready.write_with(write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(output: &mut SynchronizedOutput) -> Vec<Vec<u8>> {
        let mut writes = Vec::new();
        output
            .write_with(|bytes| {
                writes.push(bytes.to_vec());
                Ok(bytes.len())
            })
            .unwrap();
        writes
    }

    #[test]
    fn every_fragment_size_preserves_one_completed_update() {
        let now = Instant::now();
        let mut update = BEGIN.to_vec();
        update.extend_from_slice(b"\x1b[23;19H\xff\x00\xc3\xa9\x1b[?25hhello");
        update.extend_from_slice(END);
        for size in 1..=update.len() {
            let mut output = SynchronizedOutput::default();
            let mut writes = Vec::new();
            for part in update.chunks(size) {
                output.push_at(part, now);
                writes.extend(drain(&mut output));
            }
            assert_eq!(writes, vec![update.clone()], "chunk size {size}");
        }
    }

    #[test]
    fn ordinary_output_and_unrelated_escape_sequences_do_not_wait() {
        let mut output = SynchronizedOutput::default();
        let bytes = b"prompt\x1b[31mred\x1b[0m\x1b[2;3H\xff\x00";
        output.push(bytes);
        assert_eq!(drain(&mut output).concat(), bytes);
        assert!(!output.holding());
    }

    #[test]
    fn strings_do_not_start_or_end_updates() {
        for opener in [b']', b'P', b'_', b'^', b'X'] {
            let mut bytes = vec![0x1b, opener];
            bytes.extend_from_slice(BEGIN);
            bytes.extend_from_slice(b"data\x1b\\");
            let mut output = SynchronizedOutput::default();
            for chunk in bytes.chunks(1) {
                output.push(chunk);
            }
            assert_eq!(drain(&mut output).concat(), bytes);
            assert!(!output.active);
        }
    }

    #[test]
    fn timeout_is_not_extended_by_data_or_repeated_begin() {
        let now = Instant::now();
        let mut output = SynchronizedOutput::default();
        output.push_at(BEGIN, now);
        output.push_at(BEGIN, now + MAX_HOLD / 2);
        output.push_at(b"still drawing", now + MAX_HOLD / 2);
        assert!(drain(&mut output).is_empty());
        output.expire(now + MAX_HOLD);
        let mut expected = BEGIN.repeat(2);
        expected.extend_from_slice(b"still drawing");
        assert_eq!(drain(&mut output).concat(), expected);
        output.push_at(b"no closing delimiter", now + MAX_HOLD);
        assert_eq!(drain(&mut output).concat(), b"no closing delimiter");
        output.push_at(END, now + MAX_HOLD);
        assert_eq!(drain(&mut output).concat(), END);
        output.push_at(BEGIN, now + MAX_HOLD);
        assert!(drain(&mut output).is_empty());
    }

    #[test]
    fn size_limit_and_timeout_preserve_all_bytes_and_resume() {
        let now = Instant::now();
        let mut bytes = BEGIN.to_vec();
        bytes.extend(vec![b'x'; MAX_UPDATE_BYTES * 2]);
        bytes.extend_from_slice(END);
        let mut output = SynchronizedOutput::default();
        let mut actual = Vec::new();
        for part in bytes.chunks(7919) {
            output.push_at(part, now);
            assert!(output.held.len() < MAX_UPDATE_BYTES);
            actual.extend(drain(&mut output).concat());
        }
        assert_eq!(actual, bytes);
        output.push_at(BEGIN, now);
        assert!(drain(&mut output).is_empty());
        output.flush();
        assert_eq!(drain(&mut output).concat(), BEGIN);
    }

    #[test]
    fn incomplete_prefix_is_bounded_and_transparent() {
        for split in 1..BEGIN.len() {
            let now = Instant::now();
            let mut output = SynchronizedOutput::default();
            output.push_at(&BEGIN[..split], now);
            output.expire(now + MAX_HOLD);
            let mut actual = drain(&mut output).concat();
            output.push_at(&BEGIN[split..], now + MAX_HOLD);
            output.flush();
            actual.extend(drain(&mut output).concat());
            assert_eq!(actual, BEGIN);
        }
    }

    #[test]
    fn arbitrary_bytes_remain_identical_under_flushes_and_short_writes() {
        let mut seed = 17u32;
        let mut bytes = Vec::new();
        for _ in 0..40000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            bytes.push((seed >> 24) as u8);
        }
        bytes.extend_from_slice(BEGIN);
        bytes.extend_from_slice(b"unfinished\x1b[");
        let mut output = SynchronizedOutput::default();
        for part in bytes.chunks(31) {
            output.push(part);
            output.flush();
        }
        let mut actual = Vec::new();
        while !output.is_empty() {
            let mut blocked = false;
            output
                .write_with(|part| {
                    if blocked {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    blocked = true;
                    let len = part.len().min(3);
                    actual.extend_from_slice(&part[..len]);
                    Ok(len)
                })
                .unwrap();
        }
        assert_eq!(actual, bytes);
    }
    #[test]
    fn cancelled_escapes_and_string_bel_are_scanned_correctly() {
        for prefix in [b"\x1b".as_slice(), b"\x1b[123"] {
            let mut output = SynchronizedOutput::default();
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(BEGIN);
            output.push(&bytes);
            assert_eq!(drain(&mut output).concat(), prefix);
            assert!(output.active);
            output.push(END);
            assert_eq!(drain(&mut output).concat(), [BEGIN, END].concat());
        }
        for opener in [b'P', b'_', b'^', b'X'] {
            let mut output = SynchronizedOutput::default();
            let mut bytes = vec![0x1b, opener, 7];
            bytes.extend_from_slice(BEGIN);
            bytes.extend_from_slice(b"\x1b\\");
            output.push(&bytes);
            assert_eq!(drain(&mut output).concat(), bytes);
            assert!(!output.active);
        }
    }

    #[test]
    fn poll_uses_remaining_deadline() {
        let now = Instant::now();
        let mut output = SynchronizedOutput::default();
        output.push_at(BEGIN, now);
        assert_eq!(output.poll_timeout(-1, now + Duration::from_millis(24)), 1);
        assert_eq!(output.poll_timeout(0, now), 0);
    }
}
