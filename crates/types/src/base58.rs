//! Base58 / Base58Check (Bitcoin alphabet). XDAG's Base58Check has no version
//! byte: `payload || sha256d(payload)[0..4]`.

use crate::hash::sha256d;

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

pub fn encode(input: &[u8]) -> String {
    let zeros = input.iter().take_while(|&&b| b == 0).count();
    // log(256)/log(58) ~ 1.37
    let mut digits: Vec<u8> = Vec::with_capacity(input.len() * 138 / 100 + 1);
    for &byte in &input[zeros..] {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(zeros + digits.len());
    for _ in 0..zeros {
        out.push('1');
    }
    for &d in digits.iter().rev() {
        out.push(ALPHABET[d as usize] as char);
    }
    out
}

pub fn decode(input: &str) -> Option<Vec<u8>> {
    let mut index = [0xffu8; 128];
    for (i, &c) in ALPHABET.iter().enumerate() {
        index[c as usize] = i as u8;
    }
    let bytes = input.as_bytes();
    let zeros = bytes.iter().take_while(|&&c| c == b'1').count();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    for &c in &bytes[zeros..] {
        if c >= 128 || index[c as usize] == 0xff {
            return None;
        }
        let mut carry = index[c as usize] as u32;
        for b in out.iter_mut() {
            carry += (*b as u32) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            out.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut res = vec![0u8; zeros];
    res.extend(out.iter().rev());
    Some(res)
}

pub fn encode_check(payload: &[u8]) -> String {
    let checksum = sha256d(payload);
    let mut data = Vec::with_capacity(payload.len() + 4);
    data.extend_from_slice(payload);
    data.extend_from_slice(&checksum[..4]);
    encode(&data)
}

pub fn decode_check(input: &str) -> Option<Vec<u8>> {
    let data = decode(input)?;
    if data.len() < 4 {
        return None;
    }
    let (payload, checksum) = data.split_at(data.len() - 4);
    if sha256d(payload)[..4] != *checksum {
        return None;
    }
    Some(payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_leading_zeros() {
        let data = [0u8, 0, 1, 2, 3, 255];
        assert_eq!(decode(&encode(&data)).unwrap(), data);
        assert_eq!(encode(&[]), "");
    }

    #[test]
    fn check_rejects_bad_checksum() {
        // PubkeyAddressUtilsTest: ...Dh2C valid, ...Dh2a invalid
        assert!(decode_check("7pWm5FZaNVV61wb4vQapqVixPaLC7Dh2C").is_some());
        assert!(decode_check("7pWm5FZaNVV61wb4vQapqVixPaLC7Dh2a").is_none());
    }
}
