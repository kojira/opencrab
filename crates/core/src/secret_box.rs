//! at-rest 秘密（Nostr 本鍵・生成鍵）の暗号化保管層（#620）。
//!
//! nostr に**結合しない**汎用の封筒。用途は「エージェントが設定を確認しようとして読んでも、
//! 平文の鍵が目に入らない」こと。偶然の混入だけを守り、意図的な抜き取りは想定しない。
//!
//! 形式は 1 行文字列 `enc:v1:` + base64(nonce(24) ‖ ciphertext+tag)。中身は
//! XChaCha20-Poly1305（24 バイトのランダム nonce を毎回サンプルする）。
//!
//! **マスターキーはこの層では読まない**（純関数・テスト可能）。呼び出し側が
//! `std::env` などから読んで注入する。ここは env にもファイルにも触れない。

use anyhow::{bail, Context, Result};
use base64::Engine;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

/// 封筒接頭辞（バージョン付き）。平文（`nsec1…` / hex）と暗号文を確実に区別し、移行の
/// 冪等判定（既に暗号化済みならスキップ）にも使う。将来方式を変えるときは `enc:v2:` を
/// 足し、旧封筒を読めるようにする。
pub const ENVELOPE_PREFIX: &str = "enc:v1:";

/// XChaCha20-Poly1305 の nonce 長（24 バイト）。ランダム nonce を衝突を気にせず使える幅。
const NONCE_LEN: usize = 24;

/// マスターキー長（32 バイト）。
pub const MASTER_KEY_LEN: usize = 32;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// この文字列が暗号化封筒か（移行の冪等判定・平文検出に使う）。
pub fn is_encrypted(s: &str) -> bool {
    s.starts_with(ENVELOPE_PREFIX)
}

/// 平文を封筒化する。`enc:v1:` + base64(nonce ‖ ct+tag)。
///
/// マスターキーは外から注入する（この関数は env もファイルも読まない）。nonce は毎回
/// ランダムなので、同じ平文でも封筒は毎回変わる。
pub fn encrypt(plaintext: &[u8], master_key: &[u8; MASTER_KEY_LEN]) -> Result<String> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(master_key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| anyhow::anyhow!("failed to sample random nonce: {e}"))?;
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(format!("{ENVELOPE_PREFIX}{}", B64.encode(&blob)))
}

/// 封筒を復号する。復号結果は [`Zeroizing`]（drop 時にゼロ埋め）。
///
/// 接頭辞不一致・base64 不正・長さ不足・認証失敗（鍵違い / 改竄）はすべてエラー。
pub fn decrypt(envelope: &str, master_key: &[u8; MASTER_KEY_LEN]) -> Result<Zeroizing<Vec<u8>>> {
    let b64 = envelope
        .strip_prefix(ENVELOPE_PREFIX)
        .context("not an encrypted envelope (missing enc:v1: prefix)")?;
    let blob = B64
        .decode(b64.trim())
        .context("failed to base64-decode envelope")?;
    if blob.len() < NONCE_LEN {
        bail!("envelope too short to contain a nonce");
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(master_key));
    let nonce = XNonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("decryption failed (wrong master key or corrupted data)"))?;
    Ok(Zeroizing::new(plaintext))
}

/// base64 の 32 バイトマスターキーをパースする。base64 不正・長さ不正はエラー
/// （呼び出し側は fail-closed で扱う）。復号後の生バイトは [`Zeroizing`] で保持する。
pub fn parse_master_key(b64: &str) -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>> {
    let raw = Zeroizing::new(
        B64.decode(b64.trim())
            .context("master key is not valid base64")?,
    );
    if raw.len() != MASTER_KEY_LEN {
        bail!(
            "master key must decode to {MASTER_KEY_LEN} bytes (got {})",
            raw.len()
        );
    }
    let mut key = Zeroizing::new([0u8; MASTER_KEY_LEN]);
    key.copy_from_slice(&raw);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; MASTER_KEY_LEN] {
        let mut k = [0u8; MASTER_KEY_LEN];
        getrandom::getrandom(&mut k).unwrap();
        k
    }

    #[test]
    fn roundtrip_returns_original_plaintext() {
        let key = test_key();
        let secret = b"nsec1supersecretkeymaterial";
        let env = encrypt(secret, &key).unwrap();
        assert!(is_encrypted(&env), "封筒接頭辞が付いていない: {env}");
        assert!(!env.contains("nsec1"), "平文が封筒に漏れている: {env}");
        let out = decrypt(&env, &key).unwrap();
        assert_eq!(&out[..], secret);
    }

    #[test]
    fn nonce_is_random_so_same_plaintext_differs() {
        let key = test_key();
        let a = encrypt(b"same", &key).unwrap();
        let b = encrypt(b"same", &key).unwrap();
        assert_ne!(a, b, "同じ平文で封筒が同一（nonce が固定されている）");
        // どちらも同じ平文へ戻る。
        assert_eq!(&decrypt(&a, &key).unwrap()[..], b"same");
        assert_eq!(&decrypt(&b, &key).unwrap()[..], b"same");
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let key = test_key();
        let mut wrong = key;
        wrong[0] ^= 0xff;
        let env = encrypt(b"secret", &key).unwrap();
        assert!(decrypt(&env, &wrong).is_err(), "鍵違いで復号できてしまった");
    }

    #[test]
    fn tampered_ciphertext_fails_auth() {
        let key = test_key();
        let env = encrypt(b"secret", &key).unwrap();
        // base64 部分の末尾を 1 文字書き換える（tag/ct を壊す）。
        let mut chars: Vec<char> = env.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert!(decrypt(&tampered, &key).is_err(), "改竄が検出されなかった");
    }

    #[test]
    fn decrypt_rejects_non_envelope_and_short_input() {
        let key = test_key();
        assert!(decrypt("nsec1plaintext", &key).is_err(), "接頭辞無しを受理した");
        assert!(
            decrypt("enc:v1:AAAA", &key).is_err(),
            "nonce に満たない短い封筒を受理した"
        );
    }

    #[test]
    fn is_encrypted_distinguishes_plaintext_from_envelope() {
        assert!(!is_encrypted("nsec1abcdef"));
        assert!(!is_encrypted(""));
        assert!(is_encrypted("enc:v1:whatever"));
    }

    #[test]
    fn parse_master_key_accepts_32_bytes_rejects_others() {
        let raw = test_key();
        let b64 = B64.encode(raw);
        let parsed = parse_master_key(&b64).unwrap();
        assert_eq!(&parsed[..], &raw[..]);
        // 空白を跨いでも trim される。
        assert!(parse_master_key(&format!("  {b64}\n")).is_ok());
        // 長さ違い（31 バイト）。
        assert!(parse_master_key(&B64.encode([0u8; 31])).is_err());
        // 長さ違い（33 バイト）。
        assert!(parse_master_key(&B64.encode([0u8; 33])).is_err());
        // base64 不正。
        assert!(parse_master_key("not valid base64 !!!").is_err());
    }

    /// 封筒とマスターキーが一体で動く（parse したキーで復号できる）。
    #[test]
    fn parsed_key_decrypts_envelope() {
        let raw = test_key();
        let b64 = B64.encode(raw);
        let env = encrypt(b"payload", &raw).unwrap();
        let parsed = parse_master_key(&b64).unwrap();
        assert_eq!(&decrypt(&env, &parsed).unwrap()[..], b"payload");
    }
}
