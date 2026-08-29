//! Fixed-capacity byte buffer.
//!
//! Overlong writes truncate instead of growing or panicking. That is what makes
//! the type usable from a signal handler: it never allocates, and it cannot
//! abort the process while the process is busy explaining why it is aborting.

/// A byte buffer of capacity `N` that silently drops what does not fit.
#[derive(Debug)]
pub struct Buf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Default for Buf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Buf<N> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    pub fn push(&mut self, byte: u8) {
        if self.len < N {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        let room = N - self.len;
        let take = bytes.len().min(room);
        self.bytes[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
    }

    pub fn push_u64(&mut self, value: u64) {
        self.push_u64_pad(value, 1);
    }

    /// Decimal, left-padded with zeroes to at least `width` digits.
    pub fn push_u64_pad(&mut self, value: u64, width: usize) {
        // 20 digits covers u64::MAX.
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut rest = value;

        loop {
            digits[count] = b'0' + (rest % 10) as u8;
            count += 1;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }

        for _ in count..width {
            self.push(b'0');
        }
        while count > 0 {
            count -= 1;
            self.push(digits[count]);
        }
    }

    /// Lowercase hex without a `0x` prefix.
    pub fn push_hex(&mut self, value: u64) {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut nibble = 16;
        while nibble > 1 && (value >> ((nibble - 1) * 4)) == 0 {
            nibble -= 1;
        }
        while nibble > 0 {
            nibble -= 1;
            self.push(HEX[((value >> (nibble * 4)) & 0xf) as usize]);
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The contents as UTF-8, or `""` if a push truncated a multi-byte
    /// character. Callers only ever push ASCII, so this is total in practice.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.as_bytes()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_and_padding() {
        let mut buf = Buf::<32>::new();
        buf.push_u64(0);
        buf.push(b' ');
        buf.push_u64(1234);
        buf.push(b' ');
        buf.push_u64_pad(7, 4);
        buf.push(b' ');
        buf.push_u64_pad(12345, 2);
        assert_eq!(buf.as_str(), "0 1234 0007 12345");
    }

    #[test]
    fn u64_max_is_exact() {
        let mut buf = Buf::<32>::new();
        buf.push_u64(u64::MAX);
        assert_eq!(buf.as_str(), "18446744073709551615");
    }

    #[test]
    fn hex_drops_leading_zeroes() {
        let mut buf = Buf::<32>::new();
        buf.push_hex(0);
        buf.push(b' ');
        buf.push_hex(0xdead_beef);
        buf.push(b' ');
        buf.push_hex(u64::MAX);
        assert_eq!(buf.as_str(), "0 deadbeef ffffffffffffffff");
    }

    #[test]
    fn overflow_truncates_instead_of_panicking() {
        let mut buf = Buf::<4>::new();
        buf.push_bytes(b"abcdefgh");
        assert_eq!(buf.as_str(), "abcd");

        buf.push(b'x');
        buf.push_u64(999);
        assert_eq!(buf.as_str(), "abcd");
    }
}
