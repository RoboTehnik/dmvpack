use crate::manifest::EncryptionSpec;
use anyhow::{anyhow, bail, Result};
use argon2::{Algorithm, Argon2, ParamsBuilder, Version};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use serde::{Deserialize, Serialize};

/// KDF identifier stored in the manifest (1 = argon2id, version 0x13).
pub const KDF_ARGON2ID: u8 = 1;
/// argon2id costs (KiB / passes / lanes), stored per archive in the manifest.
pub const M_COST: u32 = 19 * 1024;
pub const T_COST: u32 = 2;
pub const P_COST: u32 = 1;
const KEY_LEN: usize = 32;

/// Sensitive manifest fields that travel *inside* the encrypted payload.
/// The public manifest keeps dummies for them while an archive is encrypted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrivateSpec {
    pub raw_len: u64,
    pub raw_sha256: String,
    pub compressed_len: u64,
    pub source_name: String,
    pub source_is_dir: bool,
}

fn derive_key(password: &[u8], salt: &[u8], spec: &EncryptionSpec) -> Result<[u8; KEY_LEN]> {
    if spec.kdf != KDF_ARGON2ID {
        bail!(
            "unsupported kdf id {} (this build supports argon2id = {})",
            spec.kdf,
            KDF_ARGON2ID
        );
    }
    let params = ParamsBuilder::new()
        .m_cost(spec.m_cost)
        .t_cost(spec.t_cost)
        .p_cost(spec.p_cost)
        .build()
        .map_err(|e| anyhow!("bad argon2 parameters in manifest: {e}"))?;
    let a = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    a.hash_password_into(password, salt, &mut key)
        .map_err(|e| anyhow!("argon2 key derivation failed: {e}"))?;
    Ok(key)
}

/// Sealed payload layout: `zstd_stream || private_json || u64le(json_len)`.
pub fn wrap(stream: &[u8], private: &[u8]) -> Vec<u8> {
    let mut pt = Vec::with_capacity(stream.len() + private.len() + 8);
    pt.extend_from_slice(stream);
    pt.extend_from_slice(private);
    pt.extend_from_slice(&(private.len() as u64).to_le_bytes());
    pt
}

/// Split a decrypted payload back into the compressed stream and private manifest.
pub fn unwrap_sealed(sealed: &[u8]) -> Result<(&[u8], &[u8])> {
    if sealed.len() < 8 {
        bail!("decrypted payload is too short");
    }
    let l = u64::from_le_bytes(sealed[sealed.len() - 8..].try_into().unwrap()) as usize;
    if sealed.len() < 8 + l {
        bail!("decrypted payload is truncated (corrupt private manifest length)");
    }
    let cut = sealed.len() - 8 - l;
    Ok((&sealed[..cut], &sealed[cut..sealed.len() - 8]))
}

/// Encrypt with a fresh random salt and nonce (argon2id -> chacha20poly1305).
pub fn encrypt(plaintext: &[u8], password: &str) -> Result<(Vec<u8>, EncryptionSpec)> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut salt).map_err(|e| anyhow!("cannot read OS randomness: {e}"))?;
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("cannot read OS randomness: {e}"))?;
    let spec = EncryptionSpec {
        kdf: KDF_ARGON2ID,
        m_cost: M_COST,
        t_cost: T_COST,
        p_cost: P_COST,
        salt: salt.to_vec(),
        nonce: nonce.to_vec(),
    };
    let key = derive_key(password.as_bytes(), &salt, &spec)?;
    let key_arr = Key::try_from(&key[..]).expect("32-byte key");
    let nonce_arr = Nonce::try_from(&nonce[..]).expect("12-byte nonce");
    let cipher = ChaCha20Poly1305::new(&key_arr);
    let ct = cipher
        .encrypt(&nonce_arr, plaintext)
        .map_err(|_| anyhow!("encryption failed"))?;
    Ok((ct, spec))
}

pub fn decrypt(ciphertext: &[u8], spec: &EncryptionSpec, password: &str) -> Result<Vec<u8>> {
    if spec.salt.len() != 16 {
        bail!(
            "corrupt manifest: salt must be 16 bytes, have {}",
            spec.salt.len()
        );
    }
    if spec.nonce.len() != 12 {
        bail!(
            "corrupt manifest: nonce must be 12 bytes, have {}",
            spec.nonce.len()
        );
    }
    let key = derive_key(password.as_bytes(), &spec.salt, spec)?;
    let key_arr = Key::try_from(&key[..]).expect("32-byte key");
    let nonce_arr = Nonce::try_from(&spec.nonce[..]).expect("12-byte nonce (validated above)");
    let cipher = ChaCha20Poly1305::new(&key_arr);
    cipher
        .decrypt(&nonce_arr, ciphertext)
        .map_err(|_| anyhow!("wrong password or corrupted data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let stream: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let priv_spec = PrivateSpec {
            raw_len: 9999,
            raw_sha256: "cafe".repeat(16),
            compressed_len: stream.len() as u64,
            source_name: "secret.bin".into(),
            source_is_dir: false,
        };
        let pj = serde_json::to_vec(&priv_spec).unwrap();
        let (ct, spec) = encrypt(&wrap(&stream, &pj), "hunter2").unwrap();
        let sealed = decrypt(&ct, &spec, "hunter2").unwrap();
        let (zs, pj2) = unwrap_sealed(&sealed).unwrap();
        assert_eq!(zs, &stream[..]);
        let back: PrivateSpec = serde_json::from_slice(pj2).unwrap();
        assert_eq!(back, priv_spec);
    }

    #[test]
    fn wrong_password_fails() {
        let (ct, spec) = encrypt(b"payload", "right").unwrap();
        assert!(decrypt(&ct, &spec, "wrong").is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let (mut ct, spec) = encrypt(b"payload", "pw").unwrap();
        ct[0] ^= 0xff;
        assert!(decrypt(&ct, &spec, "pw").is_err());
        let (mut ct, spec) = encrypt(b"payload", "pw").unwrap();
        let n = ct.len();
        ct[n - 1] ^= 0xff; // tag
        assert!(decrypt(&ct, &spec, "pw").is_err());
    }

    #[test]
    fn unwrap_detects_short_input() {
        assert!(unwrap_sealed(&[]).is_err());
        assert!(unwrap_sealed(&[1, 2, 3]).is_err());
        // declared length larger than the buffer
        let mut bad = b"abcd".to_vec();
        bad.extend_from_slice(&100u64.to_le_bytes());
        assert!(unwrap_sealed(&bad).is_err());
    }

    #[test]
    fn unsupported_kdf_rejected() {
        let (ct, mut spec) = encrypt(b"x", "pw").unwrap();
        spec.kdf = 99;
        assert!(decrypt(&ct, &spec, "pw").is_err());
    }
}
