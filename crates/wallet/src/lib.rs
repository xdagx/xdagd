//! XDAG wallet.
//!
//! Reads and writes xdagj's `wallet.data` (version 4) so existing wallets can
//! be used unchanged:
//!
//! ```text
//! int  version = 4
//! bytes salt (16)                      key = BCrypt.generate(password, salt, 12)  (24 bytes)
//! int  n  { bytes iv(16), bytes AES-192-CBC-PKCS7(key, iv, privkey) } * n
//! bytes iv, bytes AES(key, iv, { string mnemonic, int nextAccountIndex })
//! ```
//! HD accounts follow BIP44 `m/44'/586'/0'/0/i` (586 = XDAG).

use std::path::{Path, PathBuf};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use rand::RngCore;
use secp256k1::{Scalar, SecretKey, SECP256K1};
use sha2::Sha512;
use xdag_types::wire::{Dec, Enc};
use xdag_types::{Address, KeyPair};

type Aes192CbcEnc = cbc::Encryptor<aes::Aes192>;
type Aes192CbcDec = cbc::Decryptor<aes::Aes192>;

pub const WALLET_VERSION: i32 = 4;
pub const BCRYPT_COST: u32 = 12;
pub const XDAG_COIN_TYPE: u32 = 586;

#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("wrong password or corrupt wallet")]
    BadPassword,
    #[error("unsupported wallet version {0}")]
    Version(i32),
    #[error("malformed wallet file")]
    Malformed,
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid mnemonic: {0}")]
    Mnemonic(String),
    #[error("wallet has no HD seed")]
    NoSeed,
    #[error("invalid key")]
    Key,
}

pub type Result<T> = std::result::Result<T, WalletError>;

fn derive_key(password: &str, salt: &[u8; 16]) -> Result<[u8; 24]> {
    // BouncyCastle BCrypt.generate: raw 24-byte output, password bytes as-is
    // (no NUL terminator), 1..=72 bytes.
    let pw = password.as_bytes();
    if pw.is_empty() || pw.len() > 72 {
        return Err(WalletError::BadPassword);
    }
    Ok(bcrypt::bcrypt(BCRYPT_COST, *salt, pw))
}

fn encrypt(key: &[u8; 24], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    Aes192CbcEnc::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(data)
}

fn decrypt(key: &[u8; 24], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if iv.len() != 16 {
        return Err(WalletError::Malformed);
    }
    Aes192CbcDec::new(key.into(), iv.into()).decrypt_padded_vec_mut::<Pkcs7>(data).map_err(|_| WalletError::BadPassword)
}

/// BIP32 extended private key.
#[derive(Clone)]
pub struct ExtendedKey {
    pub key: SecretKey,
    pub chain_code: [u8; 32],
}

impl ExtendedKey {
    pub fn master(seed: &[u8]) -> Result<Self> {
        let mut mac = Hmac::<Sha512>::new_from_slice(b"Bitcoin seed").unwrap();
        mac.update(seed);
        let i = mac.finalize().into_bytes();
        let key = SecretKey::from_slice(&i[..32]).map_err(|_| WalletError::Key)?;
        Ok(ExtendedKey { key, chain_code: i[32..].try_into().unwrap() })
    }

    pub fn child(&self, index: u32) -> Result<Self> {
        let mut mac = Hmac::<Sha512>::new_from_slice(&self.chain_code).unwrap();
        if index & 0x8000_0000 != 0 {
            mac.update(&[0u8]);
            mac.update(&self.key.secret_bytes());
        } else {
            mac.update(&secp256k1::PublicKey::from_secret_key(SECP256K1, &self.key).serialize());
        }
        mac.update(&index.to_be_bytes());
        let i = mac.finalize().into_bytes();
        let tweak = Scalar::from_be_bytes(i[..32].try_into().unwrap()).map_err(|_| WalletError::Key)?;
        let key = self.key.add_tweak(&tweak).map_err(|_| WalletError::Key)?;
        Ok(ExtendedKey { key, chain_code: i[32..].try_into().unwrap() })
    }

    pub fn derive(&self, path: &[u32]) -> Result<Self> {
        let mut k = self.clone();
        for &i in path {
            k = k.child(i)?;
        }
        Ok(k)
    }

    pub fn keypair(&self) -> KeyPair {
        KeyPair::from_secret(self.key)
    }
}

const H: u32 = 0x8000_0000;

/// BIP44 XDAG key `m/44'/586'/account'/0/index`.
pub fn derive_xdag_key(seed: &[u8], account: u32, index: u32) -> Result<KeyPair> {
    Ok(ExtendedKey::master(seed)?.derive(&[44 | H, XDAG_COIN_TYPE | H, account | H, 0, index])?.keypair())
}

pub fn mnemonic_to_seed(mnemonic: &str, passphrase: &str) -> Result<[u8; 64]> {
    let m = bip39::Mnemonic::parse_normalized(&mnemonic.split_whitespace().collect::<Vec<_>>().join(" "))
        .map_err(|e| WalletError::Mnemonic(e.to_string()))?;
    Ok(m.to_seed(passphrase))
}

pub fn generate_mnemonic(words: usize) -> Result<String> {
    let entropy_len = words / 3 * 4;
    let mut entropy = vec![0u8; entropy_len];
    rand::thread_rng().fill_bytes(&mut entropy);
    let m = bip39::Mnemonic::from_entropy(&entropy).map_err(|e| WalletError::Mnemonic(e.to_string()))?;
    Ok(m.to_string())
}

/// An unlocked wallet.
pub struct Wallet {
    pub path: PathBuf,
    password: String,
    pub accounts: Vec<KeyPair>,
    pub mnemonic: String,
    pub next_index: i32,
}

impl Wallet {
    pub fn exists(path: &Path) -> bool {
        std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
    }

    /// New HD wallet with a fresh 12-word mnemonic and one account.
    pub fn create(path: &Path, password: &str) -> Result<Wallet> {
        let mnemonic = generate_mnemonic(12)?;
        Self::from_mnemonic(path, password, &mnemonic)
    }

    pub fn from_mnemonic(path: &Path, password: &str, mnemonic: &str) -> Result<Wallet> {
        mnemonic_to_seed(mnemonic, "")?;
        if password.is_empty() || password.len() > 72 {
            return Err(WalletError::BadPassword);
        }
        let mut w =
            Wallet { path: path.to_path_buf(), password: password.to_string(), accounts: vec![], mnemonic: mnemonic.to_string(), next_index: 0 };
        w.add_hd_account()?;
        Ok(w)
    }

    pub fn unlock(path: &Path, password: &str) -> Result<Wallet> {
        let data = std::fs::read(path)?;
        Self::decode(path, password, &data)
    }

    pub fn decode(path: &Path, password: &str, data: &[u8]) -> Result<Wallet> {
        let mut d = Dec::new(data);
        let version = d.int().map_err(|_| WalletError::Malformed)?;
        if version != WALLET_VERSION {
            return Err(WalletError::Version(version));
        }
        let salt: [u8; 16] = d.bytes().map_err(|_| WalletError::Malformed)?.try_into().map_err(|_| WalletError::Malformed)?;
        let key = derive_key(password, &salt)?;
        let n = d.int().map_err(|_| WalletError::Malformed)?;
        if !(0..=1_000_000).contains(&n) {
            return Err(WalletError::Malformed);
        }
        let mut accounts = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let iv = d.bytes().map_err(|_| WalletError::Malformed)?;
            let enc = d.bytes().map_err(|_| WalletError::Malformed)?;
            let raw = decrypt(&key, &iv, &enc)?;
            // Java BigInteger encoding: may carry a leading zero byte
            let raw = if raw.len() == 33 && raw[0] == 0 { &raw[1..] } else { &raw[..] };
            let kp = KeyPair::from_secret_bytes(raw).map_err(|_| WalletError::BadPassword)?;
            if !accounts.contains(&kp) {
                accounts.push(kp);
            }
        }
        let iv = d.bytes().map_err(|_| WalletError::Malformed)?;
        let enc = d.bytes().map_err(|_| WalletError::Malformed)?;
        let seed = decrypt(&key, &iv, &enc)?;
        let mut sd = Dec::new(&seed);
        let mnemonic = sd.string().map_err(|_| WalletError::Malformed)?;
        let next_index = sd.int().map_err(|_| WalletError::Malformed)?;
        Ok(Wallet { path: path.to_path_buf(), password: password.to_string(), accounts, mnemonic, next_index })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut rng = rand::thread_rng();
        let mut salt = [0u8; 16];
        rng.fill_bytes(&mut salt);
        let key = derive_key(&self.password, &salt)?;
        let mut e = Enc::default();
        e.int(WALLET_VERSION);
        e.bytes(&salt);
        e.int(self.accounts.len() as i32);
        for a in &self.accounts {
            let mut iv = [0u8; 16];
            rng.fill_bytes(&mut iv);
            e.bytes(&iv);
            e.bytes(&encrypt(&key, &iv, &a.secret_bytes()));
        }
        let mut s = Enc::default();
        s.string(&self.mnemonic);
        s.int(self.next_index);
        let mut iv = [0u8; 16];
        rng.fill_bytes(&mut iv);
        e.bytes(&iv);
        e.bytes(&encrypt(&key, &iv, &s.0));
        Ok(e.0)
    }

    /// Write the wallet (owner-only permissions on Unix).
    pub fn flush(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, self.encode()?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn change_password(&mut self, new_password: &str) {
        self.password = new_password.to_string();
    }

    pub fn default_key(&self) -> Option<&KeyPair> {
        self.accounts.first()
    }

    pub fn key_for(&self, a: &Address) -> Option<&KeyPair> {
        self.accounts.iter().find(|k| k.address() == *a || k.evm_address() == *a)
    }

    pub fn add_hd_account(&mut self) -> Result<KeyPair> {
        if self.mnemonic.is_empty() {
            return Err(WalletError::NoSeed);
        }
        let seed = mnemonic_to_seed(&self.mnemonic, "")?;
        let kp = derive_xdag_key(&seed, 0, self.next_index as u32)?;
        self.next_index += 1;
        if !self.accounts.contains(&kp) {
            self.accounts.push(kp.clone());
        }
        Ok(kp)
    }

    pub fn add_random_account(&mut self) -> KeyPair {
        let kp = KeyPair::random();
        self.accounts.push(kp.clone());
        kp
    }

    pub fn import_key(&mut self, secret: &[u8]) -> Result<KeyPair> {
        let kp = KeyPair::from_secret_bytes(secret).map_err(|_| WalletError::Key)?;
        if !self.accounts.contains(&kp) {
            self.accounts.push(kp.clone());
        }
        Ok(kp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bip44_vectors_from_xdagj_crypto() {
        let seed = mnemonic_to_seed("spider elbow fossil truck deal circle divert sleep safe report laundry above", "password").unwrap();
        let k = derive_xdag_key(&seed, 0, 0).unwrap();
        assert_eq!(hex::encode(k.address().0), "6a52a623fc36974cb3c67c3558694584eb39008a");

        let seed = mnemonic_to_seed("know party bunker fly ribbon combine dilemma omit birth impose submit cost", "").unwrap();
        let k = derive_xdag_key(&seed, 0, 0).unwrap();
        assert_eq!(hex::encode(k.secret_bytes()), "3a35b1a709a9fa5ddddbdf4e03f2ef309005e50be04d92e67f75eabae0335ba9");
    }

    #[test]
    fn wallet_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallet.data");
        let mut w =
            Wallet::from_mnemonic(&p, "pw", "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about").unwrap();
        w.add_hd_account().unwrap();
        w.add_random_account();
        w.flush().unwrap();
        let r = Wallet::unlock(&p, "pw").unwrap();
        assert_eq!(r.accounts, w.accounts);
        assert_eq!(r.mnemonic, w.mnemonic);
        assert_eq!(r.next_index, 2);
        assert!(matches!(Wallet::unlock(&p, "wrong"), Err(WalletError::BadPassword)));
    }
}
