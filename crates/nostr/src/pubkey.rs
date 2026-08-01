//! Nostr 公開鍵の表現をひとつに揃える（#319）。
//!
//! Nostr の公開鍵は **同じ鍵が 2 通りの見た目**を持つ:
//!
//! - `npub1...`（bech32 / NIP-19）— 人間が貼るのはほぼこちら
//! - 64 桁 hex — 受信イベントの `pubkey` フィールドはこちら
//!
//! この 2 つを素の文字列比較にかけると **同一人物が一致しない**。既に実データで
//! 起きている（同じ人物が npub 形式と hex 形式で別レコードとして入っていた）。
//! そのため「保存時にどちらかへ寄せる」だけでは足りず、**比較の前に必ず正規化する**。
//!
//! ## 揃える先は hex
//!
//! 受信イベントの `pubkey`（＝ Nostr セッションの発言者識別子）が hex なので、
//! **受信側の表現を基準**にする。逆向き（npub へ寄せる）にすると、受信のたびに
//! bech32 エンコードが要り、エンコードに失敗した瞬間に照合が落ちる（＝オーナーが
//! 黙って権限を失う）。hex へ寄せれば、受信側は入力をそのまま使える。
//!
//! ## 依存を増やさない
//!
//! bech32 は 30 行程度で、外部クレートを足す理由が無い。実装は BIP-173 準拠
//! （チェックサム検証つき）で、壊れた npub は `None` になる（黙って通さない）。

/// bech32 のデータ部の文字集合（BIP-173）。
const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// Nostr の公開鍵の hex 表現の長さ（32 バイト）。
const PUBKEY_HEX_LEN: usize = 64;

/// NIP-19 の公開鍵の human readable part。
const NPUB_HRP: &str = "npub";

fn polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [
        0x3b6a_57b2,
        0x2650_8e6d,
        0x1ea1_19fa,
        0x3d42_33dd,
        0x2a14_62b3,
    ];
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x01ff_ffff) << 5) ^ u32::from(*v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|c| c >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|c| c & 31));
    out
}

/// 5bit 群 → 8bit 群（パディング無し）。余りビットが 0 でなければ不正。
fn convert_5_to_8(data: &[u8]) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(data.len() * 5 / 8);
    for &v in data {
        if v >> 5 != 0 {
            return None;
        }
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // 端数は 5bit 未満、かつ 0 でなければならない（BIP-173）。
    if bits >= 5 || (acc << (8 - bits)) & 0xff != 0 {
        return None;
    }
    Some(out)
}

/// 8bit 群 → 5bit 群（パディングあり）。
fn convert_8_to_5(data: &[u8]) -> Vec<u8> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(data.len() * 8 / 5 + 1);
    for &v in data {
        acc = (acc << 8) | u32::from(v);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (5 - bits)) & 31) as u8);
    }
    out
}

/// bech32 文字列を `(hrp, データ部の 5bit 群)` に分解する。チェックサム不一致は `None`。
fn bech32_decode(s: &str) -> Option<(String, Vec<u8>)> {
    if !s.is_ascii() {
        return None;
    }
    // 大文字と小文字の混在は不正（BIP-173）。
    if s.chars().any(|c| c.is_ascii_lowercase()) && s.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    let sep = lower.rfind('1')?;
    // hrp が空 / チェックサム 6 文字に足りないものは不正。
    if sep == 0 || sep + 7 > lower.len() {
        return None;
    }
    let hrp = &lower[..sep];
    if hrp.bytes().any(|c| !(33..=126).contains(&c)) {
        return None;
    }
    let mut data = Vec::with_capacity(lower.len() - sep - 1);
    for c in lower[sep + 1..].bytes() {
        data.push(CHARSET.iter().position(|&x| x == c)? as u8);
    }
    let mut checked = hrp_expand(hrp);
    checked.extend_from_slice(&data);
    if polymod(&checked) != 1 {
        return None;
    }
    data.truncate(data.len() - 6);
    Some((hrp.to_string(), data))
}

/// `(hrp, データ部の 5bit 群)` を bech32 文字列にする。
fn bech32_encode(hrp: &str, data: &[u8]) -> String {
    let mut values = hrp_expand(hrp);
    values.extend_from_slice(data);
    values.extend_from_slice(&[0u8; 6]);
    let checksum = polymod(&values) ^ 1;
    let mut out = String::with_capacity(hrp.len() + 1 + data.len() + 6);
    out.push_str(hrp);
    out.push('1');
    for &d in data {
        out.push(CHARSET[d as usize] as char);
    }
    for i in 0..6 {
        out.push(CHARSET[((checksum >> (5 * (5 - i))) & 31) as usize] as char);
    }
    out
}

/// Nostr 公開鍵を **64 桁小文字 hex** に正規化する。
///
/// `npub1...`（大文字表記も可）と 64 桁 hex（大文字小文字どちらでも）を受け付け、
/// それ以外・壊れたチェックサム・長さ違いはすべて `None`（黙って通さない）。
/// 前後の空白は無視する。
pub fn normalize_pubkey(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.len() == PUBKEY_HEX_LEN && s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Some(s.to_ascii_lowercase());
    }
    let (hrp, data) = bech32_decode(s)?;
    if hrp != NPUB_HRP {
        return None;
    }
    let bytes = convert_5_to_8(&data)?;
    if bytes.len() != PUBKEY_HEX_LEN / 2 {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Nostr 公開鍵を `npub1...` 表現にする（受け付ける入力は [`normalize_pubkey`] と同じ）。
///
/// 表記ゆれで登録された行（npub で入っている `trusted_users` 行など）も引けるよう、
/// 読み出し側が「hex と npub の両方で引く」ために使う。
pub fn to_npub(raw: &str) -> Option<String> {
    let hex = normalize_pubkey(raw)?;
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    Some(bech32_encode(NPUB_HRP, &convert_8_to_5(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用のダミー鍵（実在の pubkey は一切書かない）。
    const DUMMY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const DUMMY_HEX_2: &str = "00000000000000000000000000000000000000000000000000000000000000ff";

    /// hex はそのまま（小文字化のみ）。
    #[test]
    fn hex_is_normalized_to_lowercase() {
        assert_eq!(normalize_pubkey(DUMMY_HEX).unwrap(), DUMMY_HEX);
        assert_eq!(
            normalize_pubkey(&DUMMY_HEX_2.to_ascii_uppercase()).unwrap(),
            DUMMY_HEX_2
        );
        // 前後の空白は無視する。
        assert_eq!(
            normalize_pubkey(&format!("  {DUMMY_HEX}\n")).unwrap(),
            DUMMY_HEX
        );
    }

    /// **本丸**: npub ↔ hex を往復しても同じ鍵に落ちる。
    #[test]
    fn npub_and_hex_normalize_to_the_same_value() {
        for hex in [DUMMY_HEX, DUMMY_HEX_2] {
            let npub = to_npub(hex).expect("encode");
            assert!(npub.starts_with("npub1"), "npub: {npub}");
            assert_eq!(
                normalize_pubkey(&npub).unwrap(),
                hex,
                "npub で来ても hex と同じ値にならない"
            );
            // 大文字の npub（bech32 は大文字表記も正）も同じ値へ落ちる。
            assert_eq!(
                normalize_pubkey(&npub.to_ascii_uppercase()).unwrap(),
                hex,
                "大文字 npub が取りこぼされた"
            );
        }
    }

    /// 別の鍵は別の値に落ちる（正規化が全部同じ値へ潰さない）。
    #[test]
    fn different_keys_stay_different() {
        assert_ne!(
            normalize_pubkey(&to_npub(DUMMY_HEX).unwrap()).unwrap(),
            normalize_pubkey(&to_npub(DUMMY_HEX_2).unwrap()).unwrap()
        );
    }

    /// 壊れた入力は黙って通さない（`None`）。
    #[test]
    fn malformed_input_is_rejected() {
        assert!(normalize_pubkey("").is_none());
        assert!(normalize_pubkey("   ").is_none());
        // 長さ違いの hex。
        assert!(normalize_pubkey("abcd").is_none());
        assert!(normalize_pubkey(&"a".repeat(63)).is_none());
        assert!(normalize_pubkey(&"a".repeat(65)).is_none());
        // hex に見えない 64 文字。
        assert!(normalize_pubkey(&"z".repeat(64)).is_none());
        // hrp 違い（nsec を貼っても pubkey にはならない）。
        let nsec_like = bech32_encode("nsec", &convert_8_to_5(&[7u8; 32]));
        assert!(normalize_pubkey(&nsec_like).is_none());
        // チェックサムを壊した npub。
        let mut broken = to_npub(DUMMY_HEX).unwrap();
        let last = broken.pop().unwrap();
        broken.push(if last == 'q' { 'p' } else { 'q' });
        assert!(
            normalize_pubkey(&broken).is_none(),
            "チェックサムが壊れた npub を通した"
        );
        // 大文字小文字の混在は不正。
        let npub = to_npub(DUMMY_HEX).unwrap();
        let mixed = format!("NPUB1{}", &npub[5..]);
        assert!(normalize_pubkey(&mixed).is_none());
        // 32 バイトでない npub（データ長違い）。
        let short = bech32_encode(NPUB_HRP, &convert_8_to_5(&[1u8; 31]));
        assert!(normalize_pubkey(&short).is_none());
    }

    /// bech32 のチェックサムが BIP-173 の既知ベクタと一致する（実装の妥当性）。
    #[test]
    fn bech32_roundtrip_matches_reference_vectors() {
        for v in [
            "A12UEL5L",
            "an83characterlonghumanreadablepartthatcontainsthenumber1andtheexcludedcharactersbio1tt5tgs",
            "abcdef1qpzry9x8gf2tvdw0s3jn54khce6mua7lmqqqxw",
        ] {
            let (hrp, data) = bech32_decode(v).expect("decode reference vector");
            assert_eq!(bech32_encode(&hrp, &data), v.to_ascii_lowercase());
        }
    }
}
