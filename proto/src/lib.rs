//! Wire protocol for pterm daemon <-> client communication.
//!
//! All messages are framed as: [type: u8] [length: u32 LE] [payload: &[u8]]
//! Exception: messages with no payload omit the length field.

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
}

/// Daemon → Client message types
pub mod server {
    /// pty output (raw bytes from pty stdout)
    /// Payload: raw bytes
    pub const OUTPUT: u8 = 0x01;

    /// Child process exited
    /// Payload: [exit_code: i32 LE]
    pub const EXIT: u8 = 0x02;

    /// Scrollback dump (sent on initial attach)
    /// Payload: full accumulated scrollback bytes
    pub const SCROLLBACK: u8 = 0x80;
}

/// Encode a framed message into a Vec<u8>.
pub fn encode(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(1 + 4 + payload.len());
    buf.push(msg_type);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Header size: 1 byte type + 4 bytes length
pub const HEADER_SIZE: usize = 5;

/// Parse a message header. Returns (msg_type, payload_length).
pub fn decode_header(header: &[u8; HEADER_SIZE]) -> (u8, u32) {
    let msg_type = header[0];
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    (msg_type, len)
}

/// Encode a resize payload.
pub fn encode_resize(cols: u16, rows: u16) -> [u8; 4] {
    let mut buf = [0u8; 4];
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
}
