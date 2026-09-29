//! xdagj `SimpleEncoder` / `SimpleDecoder` wire format: big-endian integers,
//! byte strings prefixed with a VLQ length (7-bit groups, most significant
//! first, continuation bit on all but the last group, at most 4 groups).

#[derive(Default)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn int(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn long(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn short(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn byte(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn boolean(&mut self, v: bool) {
        self.0.push(v as u8);
    }
    pub fn size(&mut self, mut n: usize) {
        assert!(n <= 0x0FFF_FFFF, "size too large for VLQ");
        let mut buf = [0u8; 4];
        let mut i = 4;
        loop {
            i -= 1;
            buf[i] = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                break;
            }
        }
        for (j, b) in buf.iter().enumerate().skip(i) {
            self.0.push(if j != 3 { b | 0x80 } else { *b });
        }
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.size(b.len());
        self.0.extend_from_slice(b);
    }
    pub fn string(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    pub fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
}

pub struct Dec<'a> {
    b: &'a [u8],
    pos: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("truncated or malformed data")]
pub struct DecodeError;

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Dec { b, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.pos + n > self.b.len() {
            return Err(DecodeError);
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn int(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn long(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn short(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub fn boolean(&mut self) -> Result<bool, DecodeError> {
        Ok(self.byte()? != 0)
    }
    pub fn size(&mut self) -> Result<usize, DecodeError> {
        let mut size = 0usize;
        for _ in 0..4 {
            let b = self.byte()?;
            size = (size << 7) | (b & 0x7f) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok(size)
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>, DecodeError> {
        let n = self.size()?;
        Ok(self.take(n)?.to_vec())
    }
    pub fn string(&mut self) -> Result<String, DecodeError> {
        String::from_utf8(self.bytes()?).map_err(|_| DecodeError)
    }
    pub fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlq_matches_xdagj() {
        for n in [0usize, 1, 127, 128, 300, 16383, 16384, 0x0FFF_FFFF] {
            let mut e = Enc::default();
            e.size(n);
            assert_eq!(Dec::new(&e.0).size().unwrap(), n);
        }
        let mut e = Enc::default();
        e.size(300); // 0b10_0101100 → 0x82 0x2c
        assert_eq!(e.0, vec![0x82, 0x2c]);
    }
}
