//! Web-owned authenticated secret envelopes.

use anyhow::{Context as _, Result};
use base64::Engine as _;
use chacha20poly1305::aead::Aead as _;
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

pub const MASTER_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const PREFIX: &str = "enc:v1:";

pub fn parse_master_key(value: &str) -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>> {
    let bytes = Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(value.trim())?);
    anyhow::ensure!(
        bytes.len() == MASTER_KEY_LEN,
        "master key must decode to 32 bytes"
    );
    let mut key = Zeroizing::new([0_u8; MASTER_KEY_LEN]);
    key.copy_from_slice(&bytes);
    Ok(key)
}

pub fn encrypt(clear: &[u8], key: &[u8; MASTER_KEY_LEN]) -> Result<String> {
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(|error| anyhow::anyhow!("nonce: {error}"))?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("fixed key length");
    let encrypted = cipher
        .encrypt(XNonce::from_slice(&nonce), clear)
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut value = nonce.to_vec();
    value.extend(encrypted);
    Ok(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(value)
    ))
}

pub fn decrypt(value: &str, key: &[u8; MASTER_KEY_LEN]) -> Result<Zeroizing<Vec<u8>>> {
    let encoded = value
        .strip_prefix(PREFIX)
        .context("encrypted envelope required")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    anyhow::ensure!(bytes.len() > NONCE_LEN, "encrypted envelope is truncated");
    let (nonce, encrypted) = bytes.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("fixed key length");
    Ok(Zeroizing::new(
        cipher
            .decrypt(XNonce::from_slice(nonce), encrypted)
            .map_err(|_| anyhow::anyhow!("decryption failed"))?,
    ))
}

pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(PREFIX)
}
