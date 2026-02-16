/// A simple ring buffer that stores raw bytes from pty output.
/// When capacity is exceeded, oldest data is overwritten.
pub struct ScrollbackBuffer {
    buf: Vec<u8>,
    capacity: usize,
    /// Write position (next byte goes here)
    write_pos: usize,
    /// Total bytes ever written (used to detect wrap)
    total_written: u64,
}

impl ScrollbackBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity],
            capacity,
            write_pos: 0,
            total_written: 0,
        }
    }

    /// Append data to the buffer.
    pub fn append(&mut self, data: &[u8]) {
        for chunk in data.chunks(self.capacity) {
            let chunk = if chunk.len() > self.capacity {
                &chunk[chunk.len() - self.capacity..]
            } else {
                chunk
            };

            let space_to_end = self.capacity - self.write_pos;
            if chunk.len() <= space_to_end {
                self.buf[self.write_pos..self.write_pos + chunk.len()].copy_from_slice(chunk);
            } else {
                self.buf[self.write_pos..].copy_from_slice(&chunk[..space_to_end]);
                self.buf[..chunk.len() - space_to_end].copy_from_slice(&chunk[space_to_end..]);
            }
            self.write_pos = (self.write_pos + chunk.len()) % self.capacity;
            self.total_written += chunk.len() as u64;
        }
    }

    /// Get all stored data in order (oldest first).
    /// Returns a Vec<u8> containing the valid scrollback content.
    pub fn get_contents(&self) -> Vec<u8> {
        if self.total_written == 0 {
            return Vec::new();
        }

        if self.total_written <= self.capacity as u64 {
            // Haven't wrapped yet
            let len = self.total_written as usize;
            // Data starts at 0, ends at write_pos
            return self.buf[..len].to_vec();
        }

        // Wrapped: data from write_pos..end, then 0..write_pos
        let mut result = Vec::with_capacity(self.capacity);
        result.extend_from_slice(&self.buf[self.write_pos..]);
        result.extend_from_slice(&self.buf[..self.write_pos]);
        result
    }

    /// Current amount of valid data stored.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        if self.total_written <= self.capacity as u64 {
            self.total_written as usize
        } else {
            self.capacity
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.total_written == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_append_and_read() {
        let mut buf = ScrollbackBuffer::new(16);
        buf.append(b"hello");
        assert_eq!(buf.get_contents(), b"hello");
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn wrap_around() {
        let mut buf = ScrollbackBuffer::new(8);
        buf.append(b"12345678"); // fills exactly
        assert_eq!(buf.get_contents(), b"12345678");

        buf.append(b"ab"); // overwrites "12"
        assert_eq!(buf.get_contents(), b"345678ab");
    }

    #[test]
    fn large_write() {
        let mut buf = ScrollbackBuffer::new(4);
        buf.append(b"abcdefgh"); // double the capacity
                                 // Should keep the last 4 bytes
        assert_eq!(buf.get_contents(), b"efgh");
    }

    #[test]
    fn empty() {
        let buf = ScrollbackBuffer::new(16);
        assert!(buf.is_empty());
        assert_eq!(buf.get_contents(), b"");
    }
}
