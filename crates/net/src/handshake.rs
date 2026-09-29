//! xdagj handshake: INIT(secret) → HELLO → WORLD, each HELLO/WORLD signed
//! with the node key over `sha256(basic info)`; the public key is recovered
//! from the signature and must hash to the claimed peer id.

use xdag_types::crypto::pubkey_address;
use xdag_types::hash::sha256;
use xdag_types::{Address, KeyPair, PublicKey, RecSignature};

use crate::message::Handshake;

pub const SECRET_LEN: usize = 32;

#[derive(Clone)]
pub struct Identity {
    pub key: KeyPair,
    pub peer_id: String,
}

impl Identity {
    pub fn new(key: KeyPair) -> Self {
        let peer_id = key.address().to_base58();
        Identity { key, peer_id }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build(
    id: &Identity,
    network: u8,
    network_version: i16,
    port: u16,
    client_id: &str,
    capabilities: Vec<String>,
    latest_block_number: i64,
    secret: &[u8],
    timestamp_ms: i64,
    generate_block: bool,
    node_tag: &str,
) -> Handshake {
    let mut h = Handshake {
        network,
        network_version,
        peer_id: id.peer_id.clone(),
        port: port as i32,
        client_id: client_id.to_string(),
        capabilities,
        latest_block_number,
        secret: secret.to_vec(),
        timestamp: timestamp_ms,
        generate_block,
        node_tag: node_tag.to_string(),
        signature: vec![],
    };
    let digest = sha256(&h.basic_info());
    h.signature = id.key.sign_recoverable(&digest).0.to_vec();
    h
}

#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    BadNetwork,
    BadVersion,
    Invalid(&'static str),
}

/// xdagj `HandshakeMessage.validate` plus the secret check.
pub fn validate(
    h: &Handshake,
    network: u8,
    network_version: i16,
    expected_secret: &[u8],
    now_ms: i64,
    expiry_ms: i64,
) -> Result<(PublicKey, Address), HandshakeError> {
    use HandshakeError::*;
    if h.network != network {
        return Err(BadNetwork);
    }
    if h.network_version != network_version {
        return Err(BadVersion);
    }
    if h.peer_id.len() > 64 || h.peer_id.is_empty() {
        return Err(Invalid("peer id"));
    }
    if !(1..=65535).contains(&h.port) {
        return Err(Invalid("port"));
    }
    if h.client_id.len() >= 128 || h.node_tag.len() > 128 {
        return Err(Invalid("client id"));
    }
    if h.latest_block_number < 0 {
        return Err(Invalid("latest block number"));
    }
    if h.secret.len() != SECRET_LEN || h.secret != expected_secret {
        return Err(Invalid("secret"));
    }
    if (now_ms - h.timestamp).abs() > expiry_ms {
        return Err(Invalid("timestamp"));
    }
    let sig: [u8; 65] = h.signature.as_slice().try_into().map_err(|_| Invalid("signature length"))?;
    let digest = sha256(&h.basic_info());
    let pk = RecSignature(sig).recover(&digest).map_err(|_| Invalid("signature"))?;
    let addr = pubkey_address(&pk);
    if addr.to_base58() != h.peer_id {
        return Err(Invalid("peer id does not match key"));
    }
    Ok((pk, addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_validate() {
        let id = Identity::new(KeyPair::random());
        let secret = [7u8; 32];
        let h = build(&id, 2, 0, 28001, "xdag-rs/1", vec!["FULL_NODE".into()], 10, &secret, 1000, false, "t");
        assert!(validate(&h, 2, 0, &secret, 1000 + 1000, 300_000).is_ok());
        assert_eq!(validate(&h, 1, 0, &secret, 1000, 300_000), Err(HandshakeError::BadNetwork));
        assert!(validate(&h, 2, 0, &[8u8; 32], 1000, 300_000).is_err());
        assert!(validate(&h, 2, 0, &secret, 1000 + 400_000, 300_000).is_err());
        let mut forged = h.clone();
        forged.peer_id = Identity::new(KeyPair::random()).peer_id;
        assert!(validate(&forged, 2, 0, &secret, 1000, 300_000).is_err());
    }
}
