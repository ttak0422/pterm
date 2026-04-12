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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    InvalidResizePayloadLen(usize),
    InvalidExitPayloadLen(usize),
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
}
