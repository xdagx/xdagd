//! xdagj framing: packets are snappy-compressed (raw format) and split into
//! frames of at most `max_frame_body` bytes.
//!
//! ```text
//! frame header (16 bytes, big-endian):
//!   u16 version (0) | u8 compress (1 = snappy) | u8 packet type
//!   i32 packet id   | i32 packet size (compressed) | i32 body size
//! ```

use bytes::{Buf, BufMut, BytesMut};
use std::collections::HashMap;
use tokio_util::codec::{Decoder, Encoder};

pub const HEADER_SIZE: usize = 16;
pub const VERSION: u16 = 0;
pub const COMPRESS_NONE: u8 = 0;
pub const COMPRESS_SNAPPY: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub compress: u8,
    pub packet_type: u8,
    pub packet_id: i32,
    pub packet_size: i32,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid frame: {0}")]
    Invalid(&'static str),
}

pub struct FrameCodec {
    pub max_body: usize,
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if src.len() < HEADER_SIZE {
            return Ok(None);
        }
        let mut h = &src[..HEADER_SIZE];
        let version = h.get_u16();
        let compress = h.get_u8();
        let packet_type = h.get_u8();
        let packet_id = h.get_i32();
        let packet_size = h.get_i32();
        let body_size = h.get_i32();
        if version != VERSION {
            return Err(FrameError::Invalid("frame version"));
        }
        if body_size < 0 || body_size as usize > self.max_body {
            return Err(FrameError::Invalid("frame body size"));
        }
        if src.len() < HEADER_SIZE + body_size as usize {
            src.reserve(HEADER_SIZE + body_size as usize - src.len());
            return Ok(None);
        }
        src.advance(HEADER_SIZE);
        let body = src.split_to(body_size as usize).to_vec();
        Ok(Some(Frame { compress, packet_type, packet_id, packet_size, body }))
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, f: Frame, dst: &mut BytesMut) -> Result<(), FrameError> {
        if f.body.len() > self.max_body {
            return Err(FrameError::Invalid("frame body too large"));
        }
        dst.reserve(HEADER_SIZE + f.body.len());
        dst.put_u16(VERSION);
        dst.put_u8(f.compress);
        dst.put_u8(f.packet_type);
        dst.put_i32(f.packet_id);
        dst.put_i32(f.packet_size);
        dst.put_i32(f.body.len() as i32);
        dst.put_slice(&f.body);
        Ok(())
    }
}

/// Split a message into frames. Unlike xdagj's encoder this never produces an
/// empty trailing frame when the compressed size is a multiple of the frame
/// limit (xdagj sends `len % limit` = 0 bytes as the last chunk, and the
/// receiver then waits forever for the missing data).
pub fn packetize(packet_type: u8, packet_id: i32, body: &[u8], max_body: usize, max_packet: usize) -> Result<Vec<Frame>, FrameError> {
    let compressed = snap::raw::Encoder::new().compress_vec(body).map_err(|_| FrameError::Invalid("snappy compression failed"))?;
    if body.len() > max_packet || compressed.len() > max_packet {
        return Err(FrameError::Invalid("packet too large"));
    }
    let size = compressed.len();
    let mut frames = vec![];
    if size == 0 {
        frames.push(Frame { compress: COMPRESS_SNAPPY, packet_type, packet_id, packet_size: 0, body: vec![] });
        return Ok(frames);
    }
    for chunk in compressed.chunks(max_body) {
        frames.push(Frame { compress: COMPRESS_SNAPPY, packet_type, packet_id, packet_size: size as i32, body: chunk.to_vec() });
    }
    Ok(frames)
}

/// Reassembles chunked packets (bounded number of concurrent partial packets).
pub struct Assembler {
    partial: HashMap<i32, (u8, u8, usize, Vec<u8>)>,
    pub max_packet: usize,
    pub max_partial: usize,
}

impl Assembler {
    pub fn new(max_packet: usize) -> Self {
        Assembler { partial: HashMap::new(), max_packet, max_partial: 16 }
    }

    /// Feed a frame; returns `(packet type, decompressed body)` when complete.
    pub fn push(&mut self, f: Frame) -> Result<Option<(u8, Vec<u8>)>, FrameError> {
        if f.packet_size < 0 || f.packet_size as usize > self.max_packet {
            return Err(FrameError::Invalid("packet size"));
        }
        let complete = if f.body.len() == f.packet_size as usize {
            Some((f.compress, f.packet_type, f.body))
        } else {
            if !self.partial.contains_key(&f.packet_id) && self.partial.len() >= self.max_partial {
                return Err(FrameError::Invalid("too many partial packets"));
            }
            let e = self.partial.entry(f.packet_id).or_insert_with(|| (f.compress, f.packet_type, f.packet_size as usize, Vec::new()));
            if e.2 != f.packet_size as usize {
                return Err(FrameError::Invalid("inconsistent packet size"));
            }
            e.3.extend_from_slice(&f.body);
            if e.3.len() > e.2 {
                return Err(FrameError::Invalid("packet overflow"));
            }
            if e.3.len() == e.2 {
                let (c, t, _, data) = self.partial.remove(&f.packet_id).unwrap();
                Some((c, t, data))
            } else {
                None
            }
        };
        let Some((compress, ptype, data)) = complete else { return Ok(None) };
        let body = match compress {
            COMPRESS_SNAPPY => {
                let n = snap::raw::decompress_len(&data).map_err(|_| FrameError::Invalid("snappy header"))?;
                if n > self.max_packet {
                    return Err(FrameError::Invalid("decompressed packet too large"));
                }
                snap::raw::Decoder::new().decompress_vec(&data).map_err(|_| FrameError::Invalid("snappy data"))?
            }
            COMPRESS_NONE => data,
            _ => return Err(FrameError::Invalid("compression type")),
        };
        Ok(Some((ptype, body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_roundtrip_including_exact_multiple() {
        for len in [0usize, 10, 1000, 4096, 50_000] {
            // incompressible data so the compressed size is ~len
            let body: Vec<u8> = (0..len).map(|i| ((i * 7919) % 251) as u8 ^ (i >> 3) as u8).collect();
            let frames = packetize(0x18, 7, &body, 1024, 1 << 20).unwrap();
            let mut codec = FrameCodec { max_body: 1024 };
            let mut buf = BytesMut::new();
            for f in frames {
                codec.encode(f, &mut buf).unwrap();
            }
            let mut asm = Assembler::new(1 << 20);
            let mut out = None;
            while let Some(f) = codec.decode(&mut buf).unwrap() {
                if let Some(p) = asm.push(f).unwrap() {
                    out = Some(p);
                }
            }
            assert_eq!(out.unwrap(), (0x18, body));
        }
    }

    #[test]
    fn rejects_oversized() {
        let mut codec = FrameCodec { max_body: 16 };
        let mut buf = BytesMut::new();
        buf.put_u16(0);
        buf.put_u8(1);
        buf.put_u8(1);
        buf.put_i32(1);
        buf.put_i32(100);
        buf.put_i32(100);
        assert!(codec.decode(&mut buf).is_err());
        let mut asm = Assembler::new(64);
        assert!(asm.push(Frame { compress: 1, packet_type: 1, packet_id: 1, packet_size: 1000, body: vec![0; 8] }).is_err());
    }
}
