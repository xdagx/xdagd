//! Minimal explicit binary codec for stored records.
//!
//! Integers are little-endian unless a key needs big-endian for ordering
//! (callers use `to_be_bytes` directly for keys). Variable-length data is
//! prefixed with a u32 length.

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::with_capacity(64) }
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.buf.push(v as u8);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u128(&mut self, v: u128) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn fixed(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.buf.extend_from_slice(b);
        self
    }
    pub fn opt_fixed(&mut self, b: Option<&[u8]>) -> &mut Self {
        match b {
            Some(b) => {
                self.u8(1);
                self.fixed(b)
            }
            None => self.u8(0),
        }
    }
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

pub struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("truncated or malformed record at offset {0}")]
pub struct DecodeError(pub usize);

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.pos + n > self.b.len() {
            return Err(DecodeError(self.pos));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn u128(&mut self) -> Result<u128, DecodeError> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>, DecodeError> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    pub fn opt_fixed<const N: usize>(&mut self) -> Result<Option<[u8; N]>, DecodeError> {
        if self.u8()? == 0 {
            Ok(None)
        } else {
            Ok(Some(self.fixed::<N>()?))
        }
    }
    pub fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut w = Writer::new();
        w.u8(1).u64(42).bytes(b"abc").opt_fixed(Some(&[9u8; 4])).opt_fixed(None).u128(7);
        let buf = w.finish();
        let mut r = Reader::new(&buf);
        assert_eq!(r.u8().unwrap(), 1);
        assert_eq!(r.u64().unwrap(), 42);
        assert_eq!(r.bytes().unwrap(), b"abc");
        assert_eq!(r.opt_fixed::<4>().unwrap(), Some([9u8; 4]));
        assert_eq!(r.opt_fixed::<4>().unwrap(), None);
        assert_eq!(r.u128().unwrap(), 7);
        assert!(r.is_empty());
        assert!(r.u8().is_err());
    }
}
