//! Wire protocol for pterm daemon <-> client communication.
//!
//! All messages are framed as: [type: u8] [length: u32 LE] [payload: &[u8]]
//!
//! ```text
//!  ┌─ message type
//!  │              ┌─ payload
//! ┌┴─┬───────────┬┴───────
//! │  │  │  │  │  │  │  │   ...
//! └──┴┬──────────┴────────
//!     └─ payload length
//! ```

use std::fmt;

/// Wire protocol version. Bump on incompatible changes.
pub const PROTO_VERSION: u32 = 1;

/// Client → Daemon message types
pub mod client {
    /// Forward keyboard input to pty
    /// Payload: raw bytes to write to pty stdin
    pub const INPUT: u8 = 0x01;

    /// Resize the pty
    /// Payload: [cols: u16 LE] [rows: u16 LE]
    pub const RESIZE: u8 = 0x02;

    /// Graceful detach request (no payload)
    pub const DETACH: u8 = 0x03;

    /// Request terminal redraw (no payload)
    pub const REDRAW: u8 = 0x04;

    /// Request a plain-text snapshot of the visible terminal screen (no payload)
    pub const SNAPSHOT_TEXT: u8 = 0x05;

    /// Request a diagnostic dump of the daemon-side terminal state (no payload)
    pub const DUMP: u8 = 0x06;

    /// Handshake. Payload: [proto_version: u32 LE] [flags: u32 LE]
    pub const HELLO: u8 = 0x07;
}

/// Flags carried in the client HELLO payload.
pub mod hello_flags {
    /// Client wants scrollback history replay on attach.
    pub const REQUEST_HISTORY: u32 = 1 << 0;
}

/// Daemon → Client message types
pub mod server {
    /// pty output (raw bytes from pty stdout)
    /// Payload: raw bytes
    pub const OUTPUT: u8 = 0x01;

    /// Child process exited
    /// Payload: [exit_code: i32 LE]
    pub const EXIT: u8 = 0x02;

    /// Terminal state snapshot (sent on initial attach)
    /// Payload: escape sequences reproducing current terminal state
    pub const STATE_SYNC: u8 = 0x80;

    /// Plain-text snapshot of the visible terminal screen
    /// Payload: UTF-8 text rows separated by LF
    pub const SNAPSHOT_TEXT: u8 = 0x81;

    /// Diagnostic dump of the daemon-side terminal state
    /// Payload: UTF-8 JSON
    pub const DUMP: u8 = 0x82;

    /// Handshake reply. Payload:
    /// [proto_version: u32 LE] [pkg_version: UTF-8 (rest of payload)]
    pub const HELLO_ACK: u8 = 0x83;

    /// Scrollback history replay, sent once before the initial STATE_SYNC to
    /// clients that requested it via `hello_flags::REQUEST_HISTORY`.
    /// Payload: raw escape sequences, written to stdout like OUTPUT.
    pub const HISTORY: u8 = 0x84;
}

/// Encode a framed message into a Vec<u8>.
pub fn encode(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
    buf.push(msg_type);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Header size: 1 byte type + 4 bytes length
pub const HEADER_SIZE: usize = 5;
pub const RESIZE_PAYLOAD_SIZE: usize = 4;
pub const EXIT_PAYLOAD_SIZE: usize = 4;
pub const HELLO_PAYLOAD_SIZE: usize = 8;
/// HELLO_ACK payload starts with the protocol version.
pub const HELLO_ACK_MIN_PAYLOAD_SIZE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    InvalidResizePayloadLen(usize),
    InvalidExitPayloadLen(usize),
    InvalidHelloPayloadLen(usize),
    InvalidHelloAckPayloadLen(usize),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResizePayloadLen(len) => {
                write!(
                    f,
                    "invalid resize payload length: expected {} bytes, got {}",
                    RESIZE_PAYLOAD_SIZE, len
                )
            }
            Self::InvalidExitPayloadLen(len) => {
                write!(
                    f,
                    "invalid exit payload length: expected {} bytes, got {}",
                    EXIT_PAYLOAD_SIZE, len
                )
            }
            Self::InvalidHelloPayloadLen(len) => {
                write!(
                    f,
                    "invalid hello payload length: expected {} bytes, got {}",
                    HELLO_PAYLOAD_SIZE, len
                )
            }
            Self::InvalidHelloAckPayloadLen(len) => {
                write!(
                    f,
                    "invalid hello ack payload length: expected at least {} bytes, got {}",
                    HELLO_ACK_MIN_PAYLOAD_SIZE, len
                )
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Parse a message header. Returns (msg_type, payload_length).
pub fn decode_header(header: &[u8; HEADER_SIZE]) -> (u8, u32) {
    let msg_type = header[0];
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    (msg_type, len)
}

/// Decode all complete frames from `recv_buf`, leaving any trailing partial
/// frame bytes in place for the next read.
pub fn decode_frames(recv_buf: &mut Vec<u8>) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut offset = 0;

    while offset + HEADER_SIZE <= recv_buf.len() {
        let header: [u8; HEADER_SIZE] = recv_buf[offset..offset + HEADER_SIZE]
            .try_into()
            .expect("header slice length should match HEADER_SIZE");
        let (msg_type, payload_len) = decode_header(&header);
        let payload_len = payload_len as usize;

        if offset + HEADER_SIZE + payload_len > recv_buf.len() {
            break;
        }

        offset += HEADER_SIZE;
        let payload = recv_buf[offset..offset + payload_len].to_vec();
        offset += payload_len;

        frames.push(Frame { msg_type, payload });
    }

    if offset > 0 {
        recv_buf.drain(..offset);
    }

    frames
}

/// Encode a resize payload.
pub fn encode_resize(cols: u16, rows: u16) -> [u8; 4] {
    let mut buf = [0u8; RESIZE_PAYLOAD_SIZE];
    buf[0..2].copy_from_slice(&cols.to_le_bytes());
    buf[2..4].copy_from_slice(&rows.to_le_bytes());
    buf
}

/// Decode a resize payload.
pub fn decode_resize(payload: &[u8; 4]) -> (u16, u16) {
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    (cols, rows)
}

pub fn parse_resize(payload: &[u8]) -> Result<(u16, u16), DecodeError> {
    let payload: &[u8; 4] = payload
        .try_into()
        .map_err(|_| DecodeError::InvalidResizePayloadLen(payload.len()))?;
    Ok(decode_resize(payload))
}

/// Encode a hello payload.
pub fn encode_hello(version: u32, flags: u32) -> [u8; HELLO_PAYLOAD_SIZE] {
    let mut buf = [0u8; HELLO_PAYLOAD_SIZE];
    buf[0..4].copy_from_slice(&version.to_le_bytes());
    buf[4..8].copy_from_slice(&flags.to_le_bytes());
    buf
}

/// Parse a hello payload. Returns (proto_version, flags).
pub fn parse_hello(payload: &[u8]) -> Result<(u32, u32), DecodeError> {
    let payload: &[u8; HELLO_PAYLOAD_SIZE] = payload
        .try_into()
        .map_err(|_| DecodeError::InvalidHelloPayloadLen(payload.len()))?;
    let version = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let flags = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    Ok((version, flags))
}

/// Encode a hello ack payload.
pub fn encode_hello_ack(version: u32, pkg_version: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HELLO_ACK_MIN_PAYLOAD_SIZE + pkg_version.len());
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(pkg_version.as_bytes());
    buf
}

/// Parse a hello ack payload. Returns (proto_version, pkg_version).
pub fn parse_hello_ack(payload: &[u8]) -> Result<(u32, String), DecodeError> {
    if payload.len() < HELLO_ACK_MIN_PAYLOAD_SIZE {
        return Err(DecodeError::InvalidHelloAckPayloadLen(payload.len()));
    }
    let version = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let pkg_version = String::from_utf8_lossy(&payload[4..]).into_owned();
    Ok((version, pkg_version))
}

pub fn encode_exit(exit_code: i32) -> [u8; 4] {
    exit_code.to_le_bytes()
}

pub fn parse_exit(payload: &[u8]) -> Result<i32, DecodeError> {
    let payload: &[u8; 4] = payload
        .try_into()
        .map_err(|_| DecodeError::InvalidExitPayloadLen(payload.len()))?;
    Ok(i32::from_le_bytes(*payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encode_decode() {
        let payload = b"hello world";
        let encoded = encode(server::OUTPUT, payload);
        assert_eq!(encoded.len(), HEADER_SIZE + payload.len());

        let header: [u8; HEADER_SIZE] = encoded[..HEADER_SIZE].try_into().unwrap();
        let (msg_type, len) = decode_header(&header);
        assert_eq!(msg_type, server::OUTPUT);
        assert_eq!(len as usize, payload.len());
        assert_eq!(&encoded[HEADER_SIZE..], payload);
    }

    #[test]
    fn roundtrip_resize() {
        let buf = encode_resize(120, 40);
        let (cols, rows) = decode_resize(&buf);
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
    }

    #[test]
    fn decode_frames_drains_complete_frames_and_keeps_partial_tail() {
        let frame_a = encode(server::OUTPUT, b"abc");
        let frame_b = encode(server::STATE_SYNC, b"xyz");

        let mut recv_buf = Vec::new();
        recv_buf.extend_from_slice(&frame_a);
        recv_buf.extend_from_slice(&frame_b[..HEADER_SIZE + 1]);

        let frames = decode_frames(&mut recv_buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].msg_type, server::OUTPUT);
        assert_eq!(frames[0].payload, b"abc");
        assert_eq!(recv_buf, frame_b[..HEADER_SIZE + 1]);
    }

    #[test]
    fn parse_resize_rejects_invalid_lengths() {
        let err = parse_resize(&[1, 2, 3]).unwrap_err();
        assert_eq!(err, DecodeError::InvalidResizePayloadLen(3));
    }

    #[test]
    fn parse_exit_roundtrip() {
        let payload = encode_exit(42);
        let exit_code = parse_exit(&payload).unwrap();
        assert_eq!(exit_code, 42);
    }

    #[test]
    fn hello_roundtrip() {
        let payload = encode_hello(PROTO_VERSION, hello_flags::REQUEST_HISTORY);
        let (version, flags) = parse_hello(&payload).unwrap();
        assert_eq!(version, PROTO_VERSION);
        assert_eq!(flags, hello_flags::REQUEST_HISTORY);
    }

    #[test]
    fn parse_hello_rejects_invalid_lengths() {
        let err = parse_hello(&[1, 2, 3]).unwrap_err();
        assert_eq!(err, DecodeError::InvalidHelloPayloadLen(3));
    }

    #[test]
    fn hello_ack_roundtrip() {
        let payload = encode_hello_ack(PROTO_VERSION, "1.2.3");
        let (version, pkg_version) = parse_hello_ack(&payload).unwrap();
        assert_eq!(version, PROTO_VERSION);
        assert_eq!(pkg_version, "1.2.3");
    }

    #[test]
    fn parse_hello_ack_rejects_invalid_lengths() {
        let err = parse_hello_ack(&[1, 2]).unwrap_err();
        assert_eq!(err, DecodeError::InvalidHelloAckPayloadLen(2));
    }
}
