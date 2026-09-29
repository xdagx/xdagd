//! Wire messages. Codes 0x00–0x1A are xdagj's; 0x20+ are extensions sent only
//! to peers that advertise the `NOVA_V1` capability (xdagj ignores unknown
//! capabilities and unknown message codes, so mixed networks keep working).

use xdag_types::wire::{Dec, DecodeError, Enc};
use xdag_types::{HashLow, U256};

pub mod code {
    pub const DISCONNECT: u8 = 0x00;
    pub const HANDSHAKE_INIT: u8 = 0x01;
    pub const HANDSHAKE_HELLO: u8 = 0x02;
    pub const HANDSHAKE_WORLD: u8 = 0x03;
    pub const PING: u8 = 0x04;
    pub const PONG: u8 = 0x05;
    pub const BLOCKS_REQUEST: u8 = 0x10;
    pub const BLOCKS_REPLY: u8 = 0x11;
    pub const SUMS_REQUEST: u8 = 0x12;
    pub const SUMS_REPLY: u8 = 0x13;
    pub const BLOCK_REQUEST: u8 = 0x16;
    pub const NEW_BLOCK: u8 = 0x18;
    pub const SYNC_BLOCK: u8 = 0x19;
    pub const SYNCBLOCK_REQUEST: u8 = 0x1A;
    // extensions
    pub const GET_PEERS: u8 = 0x20;
    pub const PEERS: u8 = 0x21;
    pub const NEW_BLOCK_EXT: u8 = 0x22;
    pub const GET_PAYLOAD: u8 = 0x23;
    pub const PAYLOAD: u8 = 0x24;
    pub const NEW_TXS: u8 = 0x25;
}

pub const CAP_FULL_NODE: &str = "FULL_NODE";
pub const CAP_LIGHT_NODE: &str = "LIGHT_NODE";
pub const CAP_NOVA: &str = "NOVA_V1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Reason {
    BadNetwork = 0,
    BadNetworkVersion = 1,
    TooManyPeers = 2,
    InvalidHandshake = 3,
    DuplicatedPeerId = 4,
    MessageQueueFull = 5,
    ValidatorIpLimited = 6,
    HandshakeExists = 7,
    BadPeer = 8,
}

impl Reason {
    pub fn from_u8(v: u8) -> Option<Reason> {
        use Reason::*;
        Some(match v {
            0 => BadNetwork,
            1 => BadNetworkVersion,
            2 => TooManyPeers,
            3 => InvalidHandshake,
            4 => DuplicatedPeerId,
            5 => MessageQueueFull,
            6 => ValidatorIpLimited,
            7 => HandshakeExists,
            8 => BadPeer,
            _ => return None,
        })
    }
}

/// Chain statistics piggy-backed on every xdagj consensus message. Purely
/// informational here: xdagj trusted these numbers to decide when syncing was
/// finished, which let any peer keep a node in "syncing" forever.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub max_difficulty: U256,
    pub total_blocks: i64,
    pub total_main: i64,
    pub total_hosts: i32,
    pub main_time: i64,
}

/// Common body of the xdagj "XdagMessage" family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XdagBody {
    pub start: i64,
    pub end: i64,
    pub random: i64,
    /// 32 bytes in xdagj's (byte-reversed) representation.
    pub hash: [u8; 32],
    pub stats: Stats,
}

impl XdagBody {
    pub fn new(start: i64, end: i64, random: i64, stats: Stats) -> Self {
        XdagBody { start, end, random, hash: [0u8; 32], stats }
    }
    pub fn with_hashlow(h: &HashLow, stats: Stats) -> Self {
        XdagBody { start: 0, end: 0, random: 0, hash: hashlow_to_wire(h), stats }
    }
    pub fn hashlow(&self) -> HashLow {
        wire_to_hashlow(&self.hash)
    }
    fn encode(&self, e: &mut Enc) {
        e.long(self.start);
        e.long(self.end);
        e.long(self.random);
        e.bytes(&self.hash);
        let d = self.stats.max_difficulty.to_be_bytes::<32>();
        e.bytes(&d[16..]);
        e.long(self.stats.total_blocks);
        e.long(self.stats.total_main);
        e.int(self.stats.total_hosts);
        e.long(self.stats.main_time);
    }
    fn decode(d: &mut Dec) -> Result<Self, DecodeError> {
        let start = d.long()?;
        let end = d.long()?;
        let random = d.long()?;
        let h = d.bytes()?;
        if h.len() != 32 {
            return Err(DecodeError);
        }
        let md = d.bytes()?;
        if md.len() > 32 {
            return Err(DecodeError);
        }
        let mut buf = [0u8; 32];
        buf[32 - md.len()..].copy_from_slice(&md);
        let stats = Stats {
            max_difficulty: U256::from_be_bytes(buf),
            total_blocks: d.long()?,
            total_main: d.long()?,
            total_hosts: d.int()?,
            main_time: d.long()?,
        };
        Ok(XdagBody { start, end, random, hash: h.try_into().unwrap(), stats })
    }
}

/// xdagj puts hashlows on the wire as its in-memory 32-byte form:
/// 8 (ignored) bytes followed by the reversed 24 identifying bytes.
pub fn hashlow_to_wire(h: &HashLow) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..24 {
        out[8 + i] = h.0[23 - i];
    }
    out
}

pub fn wire_to_hashlow(b: &[u8; 32]) -> HashLow {
    let mut h = [0u8; 24];
    for i in 0..24 {
        h[i] = b[31 - i];
    }
    HashLow(h)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub network: u8,
    pub network_version: i16,
    pub peer_id: String,
    pub port: i32,
    pub client_id: String,
    pub capabilities: Vec<String>,
    pub latest_block_number: i64,
    pub secret: Vec<u8>,
    pub timestamp: i64,
    pub generate_block: bool,
    pub node_tag: String,
    pub signature: Vec<u8>,
}

impl Handshake {
    /// Bytes covered by the signature (`encodeBasicInfo`).
    pub fn basic_info(&self) -> Vec<u8> {
        let mut e = Enc::default();
        e.byte(self.network);
        e.short(self.network_version);
        e.string(&self.peer_id);
        e.int(self.port);
        e.string(&self.client_id);
        e.int(self.capabilities.len() as i32);
        for c in &self.capabilities {
            e.string(c);
        }
        e.long(self.latest_block_number);
        e.bytes(&self.secret);
        e.long(self.timestamp);
        e.boolean(self.generate_block);
        e.string(&self.node_tag);
        e.0
    }

    fn encode(&self) -> Vec<u8> {
        let mut e = Enc(self.basic_info());
        e.bytes(&self.signature);
        e.0
    }

    fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut d = Dec::new(b);
        let network = d.byte()?;
        let network_version = d.short()?;
        let peer_id = d.string()?;
        let port = d.int()?;
        let client_id = d.string()?;
        let n = d.int()?;
        if !(0..=64).contains(&n) {
            return Err(DecodeError);
        }
        let mut capabilities = vec![];
        for _ in 0..n {
            capabilities.push(d.string()?);
        }
        Ok(Handshake {
            network,
            network_version,
            peer_id,
            port,
            client_id,
            capabilities,
            latest_block_number: d.long()?,
            secret: d.bytes()?,
            timestamp: d.long()?,
            generate_block: d.boolean()?,
            node_tag: d.string()?,
            signature: d.bytes()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Disconnect(u8),
    Init { secret: Vec<u8>, timestamp: i64 },
    Hello(Handshake),
    World(Handshake),
    Ping(i64),
    Pong(i64),
    BlocksRequest(XdagBody),
    BlocksReply(XdagBody),
    SumsRequest(XdagBody),
    SumsReply(XdagBody, Vec<u8>),
    BlockRequest(XdagBody),
    SyncBlockRequest(XdagBody),
    NewBlock { block: Vec<u8>, ttl: i32 },
    SyncBlock { block: Vec<u8>, ttl: i32, exec_state: u8 },
    // extensions
    GetPeers,
    Peers(Vec<(String, u16)>),
    NewBlockExt { block: Vec<u8>, ttl: i32, payload: Vec<u8> },
    GetPayload(HashLow),
    Payload(HashLow, Vec<u8>),
    NewTxs(Vec<(u8, Vec<u8>)>),
}

impl Message {
    pub fn code(&self) -> u8 {
        use code::*;
        match self {
            Message::Disconnect(_) => DISCONNECT,
            Message::Init { .. } => HANDSHAKE_INIT,
            Message::Hello(_) => HANDSHAKE_HELLO,
            Message::World(_) => HANDSHAKE_WORLD,
            Message::Ping(_) => PING,
            Message::Pong(_) => PONG,
            Message::BlocksRequest(_) => BLOCKS_REQUEST,
            Message::BlocksReply(_) => BLOCKS_REPLY,
            Message::SumsRequest(_) => SUMS_REQUEST,
            Message::SumsReply(..) => SUMS_REPLY,
            Message::BlockRequest(_) => BLOCK_REQUEST,
            Message::SyncBlockRequest(_) => SYNCBLOCK_REQUEST,
            Message::NewBlock { .. } => NEW_BLOCK,
            Message::SyncBlock { .. } => SYNC_BLOCK,
            Message::GetPeers => GET_PEERS,
            Message::Peers(_) => PEERS,
            Message::NewBlockExt { .. } => NEW_BLOCK_EXT,
            Message::GetPayload(_) => GET_PAYLOAD,
            Message::Payload(..) => PAYLOAD,
            Message::NewTxs(_) => NEW_TXS,
        }
    }

    pub fn is_extension(&self) -> bool {
        self.code() >= 0x20
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            Message::Disconnect(r) => e.byte(*r),
            Message::Init { secret, timestamp } => {
                e.bytes(secret);
                e.long(*timestamp);
            }
            Message::Hello(h) | Message::World(h) => return h.encode(),
            Message::Ping(t) | Message::Pong(t) => e.long(*t),
            Message::BlocksRequest(b)
            | Message::BlocksReply(b)
            | Message::SumsRequest(b)
            | Message::BlockRequest(b)
            | Message::SyncBlockRequest(b) => b.encode(&mut e),
            Message::SumsReply(b, sums) => {
                b.encode(&mut e);
                e.bytes(sums);
            }
            Message::NewBlock { block, ttl } => {
                e.bytes(block);
                e.int(*ttl);
            }
            Message::SyncBlock { block, ttl, exec_state } => {
                e.bytes(block);
                e.int(*ttl);
                e.byte(*exec_state);
            }
            Message::GetPeers => {}
            Message::Peers(list) => {
                e.int(list.len() as i32);
                for (h, p) in list {
                    e.string(h);
                    e.int(*p as i32);
                }
            }
            Message::NewBlockExt { block, ttl, payload } => {
                e.bytes(block);
                e.int(*ttl);
                e.bytes(payload);
            }
            Message::GetPayload(h) => e.bytes(&hashlow_to_wire(h)),
            Message::Payload(h, p) => {
                e.bytes(&hashlow_to_wire(h));
                e.bytes(p);
            }
            Message::NewTxs(txs) => {
                e.int(txs.len() as i32);
                for (k, t) in txs {
                    e.byte(*k);
                    e.bytes(t);
                }
            }
        }
        e.0
    }

    /// Decode a packet. `Ok(None)` for unknown codes (ignored, like xdagj).
    pub fn decode(code: u8, body: &[u8]) -> Result<Option<Message>, DecodeError> {
        use code::*;
        let mut d = Dec::new(body);
        let m = match code {
            DISCONNECT => Message::Disconnect(d.byte()?),
            HANDSHAKE_INIT => Message::Init { secret: d.bytes()?, timestamp: d.long()? },
            HANDSHAKE_HELLO => Message::Hello(Handshake::decode(body)?),
            HANDSHAKE_WORLD => Message::World(Handshake::decode(body)?),
            PING => Message::Ping(d.long()?),
            PONG => Message::Pong(d.long()?),
            BLOCKS_REQUEST => Message::BlocksRequest(XdagBody::decode(&mut d)?),
            BLOCKS_REPLY => Message::BlocksReply(XdagBody::decode(&mut d)?),
            SUMS_REQUEST => Message::SumsRequest(XdagBody::decode(&mut d)?),
            SUMS_REPLY => {
                let b = XdagBody::decode(&mut d)?;
                let s = d.bytes()?;
                if s.len() != 256 {
                    return Err(DecodeError);
                }
                Message::SumsReply(b, s)
            }
            BLOCK_REQUEST => Message::BlockRequest(XdagBody::decode(&mut d)?),
            SYNCBLOCK_REQUEST => Message::SyncBlockRequest(XdagBody::decode(&mut d)?),
            NEW_BLOCK => Message::NewBlock { block: d.bytes()?, ttl: d.int()? },
            SYNC_BLOCK => {
                let block = d.bytes()?;
                let ttl = d.int()?;
                // older xdagj versions omit the execution state
                let exec_state = if d.remaining() > 0 { d.byte()? } else { 0 };
                Message::SyncBlock { block, ttl, exec_state }
            }
            GET_PEERS => Message::GetPeers,
            PEERS => {
                let n = d.int()?;
                if !(0..=256).contains(&n) {
                    return Err(DecodeError);
                }
                let mut v = vec![];
                for _ in 0..n {
                    let h = d.string()?;
                    let p = d.int()?;
                    if !(1..=65535).contains(&p) || h.len() > 255 {
                        return Err(DecodeError);
                    }
                    v.push((h, p as u16));
                }
                Message::Peers(v)
            }
            NEW_BLOCK_EXT => Message::NewBlockExt { block: d.bytes()?, ttl: d.int()?, payload: d.bytes()? },
            GET_PAYLOAD => Message::GetPayload(wire_to_hashlow(&d.fixed_bytes32()?)),
            PAYLOAD => {
                let h = wire_to_hashlow(&d.fixed_bytes32()?);
                Message::Payload(h, d.bytes()?)
            }
            NEW_TXS => {
                let n = d.int()?;
                if !(0..=10_000).contains(&n) {
                    return Err(DecodeError);
                }
                let mut v = vec![];
                for _ in 0..n {
                    let k = d.byte()?;
                    v.push((k, d.bytes()?));
                }
                Message::NewTxs(v)
            }
            _ => return Ok(None),
        };
        Ok(Some(m))
    }
}

trait Fixed32 {
    fn fixed_bytes32(&mut self) -> Result<[u8; 32], DecodeError>;
}

impl Fixed32 for Dec<'_> {
    fn fixed_bytes32(&mut self) -> Result<[u8; 32], DecodeError> {
        let b = self.bytes()?;
        b.try_into().map_err(|_| DecodeError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all() {
        let stats = Stats { max_difficulty: U256::from(12345u64), total_blocks: 9, total_main: 7, total_hosts: 2, main_time: 3 };
        let hs = Handshake {
            network: 2,
            network_version: 0,
            peer_id: "abc".into(),
            port: 8001,
            client_id: "xdag-rs".into(),
            capabilities: vec![CAP_FULL_NODE.into(), CAP_NOVA.into()],
            latest_block_number: 5,
            secret: vec![1; 32],
            timestamp: 99,
            generate_block: true,
            node_tag: "tag".into(),
            signature: vec![2; 65],
        };
        let hl = HashLow([7u8; 24]);
        let msgs = vec![
            Message::Disconnect(3),
            Message::Init { secret: vec![9; 32], timestamp: 1 },
            Message::Hello(hs.clone()),
            Message::World(hs),
            Message::Ping(5),
            Message::BlocksRequest(XdagBody::new(1, 2, 3, stats.clone())),
            Message::SumsReply(XdagBody::new(1, 2, 3, stats.clone()), vec![5; 256]),
            Message::BlockRequest(XdagBody::with_hashlow(&hl, stats)),
            Message::NewBlock { block: vec![1; 512], ttl: 5 },
            Message::SyncBlock { block: vec![1; 512], ttl: 1, exec_state: 2 },
            Message::Peers(vec![("1.2.3.4".into(), 8001)]),
            Message::NewBlockExt { block: vec![3; 512], ttl: 2, payload: vec![4; 100] },
            Message::GetPayload(hl),
            Message::Payload(hl, vec![1, 2]),
            Message::NewTxs(vec![(1, vec![1, 2, 3])]),
        ];
        for m in msgs {
            let enc = m.encode();
            let dec = Message::decode(m.code(), &enc).unwrap().unwrap();
            assert_eq!(dec, m);
        }
        assert_eq!(Message::decode(0x7f, &[]).unwrap(), None);
    }

    #[test]
    fn hashlow_wire_matches_xdagj_repr() {
        let hl = HashLow::from_xdagj_hex("0000000000000000a54ddd7c7a7bdbb22366fa2516cf648f32641623580b67e3").unwrap();
        assert_eq!(hex_of(&hashlow_to_wire(&hl)), "0000000000000000a54ddd7c7a7bdbb22366fa2516cf648f32641623580b67e3");
        assert_eq!(wire_to_hashlow(&hashlow_to_wire(&hl)), hl);
    }

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
