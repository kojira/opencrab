//! NIP-01 / NIP-19 の純関数と、プラグインプロトコル 版1 への写し。
//!
//! ここは**外界に触れない**——鍵・イベント id・schnorr 署名・bech32（npub/nsec/note）・
//! 住所→Nostr フィルタの変換・受信 note→core の `event` の組み立て。全部ネットワーク無しで単体テストできる
//! （タスクの検証規律: 既定の `cargo test` はオフラインで緑）。実リレーに繋ぐのは main と tests/relay.rs だけ。
//!
//! core の型（opencrab-*）は一切使わない。線に載るのは serde_json::Value で手で組んだ JSON だけ。

use secp256k1::{All, Keypair, Secp256k1, SecretKey, XOnlyPublicKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// ---- 16 進（hex 依存クレートを足さないので手で書く）----

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("hex length not even: {}", s.len()));
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let val = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("non-hex char: {}", c as char)),
        }
    };
    let mut i = 0;
    while i < b.len() {
        out.push((val(b[i])? << 4) | val(b[i + 1])?);
        i += 2;
    }
    Ok(out)
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

// ---- bech32（NIP-19 の基本エンティティ: npub / nsec / note）----
//
// 基本エンティティは hrp + 32 バイトのペイロードを標準 bech32（Bech32m ではない）で符号化しただけ。
// TLV を持つ nprofile/nevent 等は扱わない（このゲートの住所・origin には要らない）。

fn bech32_encode(hrp: &str, data: &[u8]) -> Result<String, String> {
    let hrp = bech32::Hrp::parse(hrp).map_err(|e| e.to_string())?;
    bech32::encode::<bech32::Bech32>(hrp, data).map_err(|e| e.to_string())
}

fn bech32_decode(s: &str, want_hrp: &str) -> Result<Vec<u8>, String> {
    let (hrp, data) = bech32::decode(s).map_err(|e| e.to_string())?;
    if hrp.as_str() != want_hrp {
        return Err(format!(
            "bech32 hrp mismatch: want {want_hrp}, got {}",
            hrp.as_str()
        ));
    }
    Ok(data)
}

/// x-only pubkey の hex（64 桁）→ `npub1...`。
pub fn npub_of(pubkey_hex: &str) -> Result<String, String> {
    let raw = from_hex(pubkey_hex)?;
    if raw.len() != 32 {
        return Err(format!("pubkey must be 32 bytes, got {}", raw.len()));
    }
    bech32_encode("npub", &raw)
}

/// `npub1...` → x-only pubkey の hex（64 桁）。
pub fn npub_to_hex(npub: &str) -> Result<String, String> {
    let raw = bech32_decode(npub, "npub")?;
    if raw.len() != 32 {
        return Err(format!("npub payload must be 32 bytes, got {}", raw.len()));
    }
    Ok(to_hex(&raw))
}

/// 秘密鍵の hex → `nsec1...`（使い捨て鍵を stderr に見せる/受けるためだけに使う）。
pub fn nsec_of(secret_hex: &str) -> Result<String, String> {
    let raw = from_hex(secret_hex)?;
    if raw.len() != 32 {
        return Err(format!("secret must be 32 bytes, got {}", raw.len()));
    }
    bech32_encode("nsec", &raw)
}

/// `nsec1...` → 秘密鍵の hex。
pub fn nsec_to_hex(nsec: &str) -> Result<String, String> {
    let raw = bech32_decode(nsec, "nsec")?;
    if raw.len() != 32 {
        return Err(format!("nsec payload must be 32 bytes, got {}", raw.len()));
    }
    Ok(to_hex(&raw))
}

/// イベント id の hex（64 桁）→ `note1...`（origin として使う・§03/§04）。
pub fn note_of(id_hex: &str) -> Result<String, String> {
    let raw = from_hex(id_hex)?;
    if raw.len() != 32 {
        return Err(format!("id must be 32 bytes, got {}", raw.len()));
    }
    bech32_encode("note", &raw)
}

/// `note1...` → イベント id の hex（64 桁）。
pub fn note_to_hex(note: &str) -> Result<String, String> {
    let raw = bech32_decode(note, "note")?;
    if raw.len() != 32 {
        return Err(format!("note payload must be 32 bytes, got {}", raw.len()));
    }
    Ok(to_hex(&raw))
}

/// origin（core が不透明に扱う外界識別子・§03/§04）→ イベント id の hex。
/// このゲートが返す origin は `note1...`。生の 64 hex も後方から受ける（タスクの「note1... か生 event id」）。
pub fn event_id_of_origin(origin: &str) -> Result<String, String> {
    if origin.starts_with("note1") {
        note_to_hex(origin)
    } else if is_hex64(origin) {
        Ok(origin.to_ascii_lowercase())
    } else {
        Err(format!("origin is neither note1... nor 64-hex: {origin}"))
    }
}

// ---- 鍵（使い捨て・既定は起動のたびに新規生成）----

/// 使い捨ての鍵。**既定は起動のたびに secp256k1 でその場生成**し、永続しない。
/// これで本番エージェントの鍵が構造的に紛れ込めない（鍵ファイルも本番の鍵パスも一切読まない）。
pub struct Key {
    secp: Secp256k1<All>,
    keypair: Keypair,
    /// x-only pubkey の hex（64 桁）。NIP-01 のイベントの `pubkey` 欄。
    pub pubkey_hex: String,
    /// `npub1...`（stderr に出して本番でないと確認できるように）。
    pub npub: String,
}

impl Key {
    fn from_secret(secp: Secp256k1<All>, sk: SecretKey) -> Key {
        let keypair = Keypair::from_secret_key(&secp, &sk);
        let (xonly, _parity) = keypair.x_only_public_key();
        let pubkey_hex = to_hex(&xonly.serialize());
        let npub = npub_of(&pubkey_hex).expect("npub of valid pubkey");
        Key {
            secp,
            keypair,
            pubkey_hex,
            npub,
        }
    }

    /// 起動のたびに新しい鍵を生成する（既定・使い捨て）。
    pub fn generate() -> Key {
        use rand::RngCore;
        let secp = Secp256k1::new();
        let mut rng = rand::rngs::OsRng;
        loop {
            let mut bytes = [0u8; 32];
            rng.fill_bytes(&mut bytes);
            if let Ok(sk) = SecretKey::from_slice(&bytes) {
                return Key::from_secret(secp, sk);
            }
            // 32 バイトが曲線の位数を超える確率は極小。超えたら引き直す（フォールバックではなく再試行）。
        }
    }

    /// 明示的な使い捨て用途に限り env `NOSTR_GATE_NSEC` を受ける（既定では使わない）。
    pub fn from_nsec(nsec: &str) -> Result<Key, String> {
        let hex = nsec_to_hex(nsec.trim())?;
        let raw = from_hex(&hex)?;
        let sk = SecretKey::from_slice(&raw).map_err(|e| e.to_string())?;
        Ok(Key::from_secret(Secp256k1::new(), sk))
    }

    /// イベント id（32 バイト）への schnorr 署名を hex で返す。
    /// aux-rand 無し（決定的）——テストで署名が再現でき、乱数源に依存しない。
    pub fn sign(&self, id: &[u8; 32]) -> String {
        let sig = self.secp.sign_schnorr_no_aux_rand(id, &self.keypair);
        to_hex(&sig.to_byte_array())
    }
}

// ---- NIP-01 イベント ----

/// いまの UNIX 秒。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// NIP-01 の id 計算の入力となる正規化 JSON 配列 `[0,pubkey,created_at,kind,tags,content]`。
/// serde_json の compact 直列化が NIP-01 の直列化規則（空白なし・所定のエスケープ）と一致する。
pub fn serialize_for_id(
    pubkey_hex: &str,
    created_at: i64,
    kind: u16,
    tags: &Value,
    content: &str,
) -> String {
    let arr = json!([0, pubkey_hex, created_at, kind, tags, content]);
    serde_json::to_string(&arr).expect("serialize id array")
}

/// NIP-01 の id = sha256(正規化 JSON 配列)。
pub fn event_id(
    pubkey_hex: &str,
    created_at: i64,
    kind: u16,
    tags: &Value,
    content: &str,
) -> [u8; 32] {
    let s = serialize_for_id(pubkey_hex, created_at, kind, tags, content);
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out = h.finalize();
    let mut id = [0u8; 32];
    id.copy_from_slice(&out);
    id
}

/// 署名済みイベントを 1 つ組む。返り値は (id の hex, リレーへ送るイベント JSON)。
pub fn build_signed(
    key: &Key,
    kind: u16,
    tags: Value,
    content: &str,
    created_at: i64,
) -> (String, Value) {
    let id = event_id(&key.pubkey_hex, created_at, kind, &tags, content);
    let id_hex = to_hex(&id);
    let sig = key.sign(&id);
    let event = json!({
        "id": id_hex,
        "pubkey": key.pubkey_hex,
        "created_at": created_at,
        "kind": kind,
        "tags": tags,
        "content": content,
        "sig": sig,
    });
    (id_hex, event)
}

/// イベントの完全検証（テスト用）。NIP-01 のとおり **id を中身から再計算して一致を確かめ**、その id への
/// schnorr 署名を検証する。中身（content/tags/…）を改竄すると id が変わって落ちる。
pub fn verify_event(event: &Value) -> Result<(), String> {
    let secp = Secp256k1::new();
    let pubkey_hex = event
        .get("pubkey")
        .and_then(|x| x.as_str())
        .ok_or("no pubkey")?;
    let sig_hex = event.get("sig").and_then(|x| x.as_str()).ok_or("no sig")?;
    let id_hex = event.get("id").and_then(|x| x.as_str()).ok_or("no id")?;
    let created_at = event
        .get("created_at")
        .and_then(|x| x.as_i64())
        .ok_or("no created_at")?;
    let kind = event
        .get("kind")
        .and_then(|x| x.as_u64())
        .ok_or("no kind")? as u16;
    let tags = event.get("tags").cloned().unwrap_or_else(|| json!([]));
    let content = event.get("content").and_then(|x| x.as_str()).unwrap_or("");

    // id は中身から一意に決まる。宣言された id と一致しなければ改竄（§03 のログは書き換えない と同じ原理）。
    let computed = event_id(pubkey_hex, created_at, kind, &tags, content);
    if to_hex(&computed) != id_hex.to_ascii_lowercase() {
        return Err("event id does not match its content".to_string());
    }

    let xonly = XOnlyPublicKey::from_slice(&from_hex(pubkey_hex)?).map_err(|e| e.to_string())?;
    let sig = secp256k1::schnorr::Signature::from_slice(&from_hex(sig_hex)?)
        .map_err(|e| e.to_string())?;
    secp.verify_schnorr(&sig, &computed, &xonly)
        .map_err(|e| e.to_string())
}

// ---- 効果（core → plugin・§04）を Nostr のイベントへ ----

/// say を kind-1 note に組む。返信（reply_target=返信先の origin）が付いていれば e-tag を足す（§04・reply_to は e-tag）。
pub fn build_say(
    key: &Key,
    text: &str,
    reply_target: Option<&str>,
    created_at: i64,
) -> Result<(String, Value), String> {
    let mut tags: Vec<Value> = vec![];
    if let Some(t) = reply_target {
        let id_hex = event_id_of_origin(t)?;
        tags.push(json!(["e", id_hex]));
    }
    Ok(build_signed(key, 1, Value::Array(tags), text, created_at))
}

/// react を kind-7 に組む（NIP-25）。対象（target=相手の origin）は必須。content は symbol。
pub fn build_react(
    key: &Key,
    symbol: &str,
    target: &str,
    created_at: i64,
) -> Result<(String, Value), String> {
    let id_hex = event_id_of_origin(target)?;
    let tags = json!([["e", id_hex]]);
    Ok(build_signed(key, 7, tags, symbol, created_at))
}

/// boost（リポスト）を kind-6 に組む（NIP-18）。対象（target=相手の origin）は必須。本文は持たない。
/// 対象の e-tag のみ付ける（元著者の p-tag は著者を追跡していないので付けない・最小構成）。
pub fn build_boost(key: &Key, target: &str, created_at: i64) -> Result<(String, Value), String> {
    let id_hex = event_id_of_origin(target)?;
    let tags = json!([["e", id_hex]]);
    Ok(build_signed(key, 6, tags, "", created_at))
}

/// quote（引用）を kind-1 に組む（NIP-18 の q-tag）。本文（payload の text）＋引用元の q-tag。
/// **新しい投稿として識別子が付く**ので、ack で origin（note1…）を返す（呼び手が付ける）。target 必須。
pub fn build_quote(
    key: &Key,
    text: &str,
    target: &str,
    created_at: i64,
) -> Result<(String, Value), String> {
    let id_hex = event_id_of_origin(target)?;
    let tags = json!([["q", id_hex]]);
    Ok(build_signed(key, 1, tags, text, created_at))
}

/// retract（取り消し）を kind-5 に組む（NIP-09 削除）。対象は**自分が出したものの origin**。
/// 消す対象の e-tag のみ付ける（本文＝理由は持たない・最小構成）。
pub fn build_retract(key: &Key, target: &str, created_at: i64) -> Result<(String, Value), String> {
    let id_hex = event_id_of_origin(target)?;
    let tags = json!([["e", id_hex]]);
    Ok(build_signed(key, 5, tags, "", created_at))
}

// ---- 住所（address_form）→ Nostr フィルタ（§02 の bind = REQ 購読）----

/// 住所を Nostr フィルタへ写す。住所は core が address_form（`^(npub1[a-z0-9]+|filter:.+)$`）で検証済み。
/// - `npub1...` → その著者の kind-1（`{authors:[hex], kinds:[1]}`）。
/// - `filter:kind=1&author=npub1xyz` → クエリを写す。npub は hex に直す。
///
/// **知らないキーは err**（近いものに寄せない・黙って通さない）。写せない住所は bind の err にする。
pub fn parse_address(address: &str) -> Result<Value, String> {
    if address.starts_with("npub1") {
        let hex = npub_to_hex(address)?;
        return Ok(json!({"authors": [hex], "kinds": [1]}));
    }
    let query = address
        .strip_prefix("filter:")
        .ok_or_else(|| format!("address is neither npub1... nor filter:...: {address}"))?;
    if query.is_empty() {
        return Err("filter: query is empty".to_string());
    }
    let mut kinds: Vec<Value> = vec![];
    let mut authors: Vec<Value> = vec![];
    for pair in query.split('&') {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("filter term is not key=value: {pair}"))?;
        match k {
            "kind" => {
                let n: u64 = v
                    .parse()
                    .map_err(|_| format!("kind is not an integer: {v}"))?;
                kinds.push(json!(n));
            }
            "author" => {
                let hex = if v.starts_with("npub1") {
                    npub_to_hex(v)?
                } else if is_hex64(v) {
                    v.to_ascii_lowercase()
                } else {
                    return Err(format!("author is neither npub1... nor 64-hex: {v}"));
                };
                authors.push(json!(hex));
            }
            other => return Err(format!("unknown filter key: {other}")),
        }
    }
    let mut filter = serde_json::Map::new();
    if !kinds.is_empty() {
        filter.insert("kinds".into(), Value::Array(kinds));
    }
    if !authors.is_empty() {
        filter.insert("authors".into(), Value::Array(authors));
    }
    if filter.is_empty() {
        return Err("filter matched no recognized terms".to_string());
    }
    Ok(Value::Object(filter))
}

// ---- 受信 note（リレー → plugin）→ core の `event`（§03）----

/// リレーから来た kind-1 イベントを core の `event`（kind=said）へ組む。
/// - author.id = npub、content.text = 本文、origin = note1（この note の id）。
/// - reply_to は最初の e-tag（→ note1）、mentions は p-tag（→ npub の並び）。
/// - **自分（このゲートの pubkey）の投稿は None を返す**——自分の出力を入力へ戻さない（自己エコー抑止）。
///
/// 呼び手（main）が `id` 欄を足して core へ送る。
pub fn incoming_to_core_event(event: &Value, address: &str, our_pubkey_hex: &str) -> Option<Value> {
    let pubkey = event.get("pubkey").and_then(|x| x.as_str())?;
    if pubkey.eq_ignore_ascii_case(our_pubkey_hex) {
        return None;
    }
    let id = event.get("id").and_then(|x| x.as_str())?;
    let content = event.get("content").and_then(|x| x.as_str()).unwrap_or("");
    let author_npub = npub_of(pubkey).ok()?;
    let origin = note_of(id).ok()?;

    let mut reply_to: Option<String> = None;
    let mut mentions: Vec<Value> = vec![];
    if let Some(tags) = event.get("tags").and_then(|x| x.as_array()) {
        for t in tags {
            let arr = match t.as_array() {
                Some(a) => a,
                None => continue,
            };
            let tag = arr.first().and_then(|x| x.as_str()).unwrap_or("");
            let val = arr.get(1).and_then(|x| x.as_str()).unwrap_or("");
            match tag {
                "e" => {
                    if reply_to.is_none() {
                        if let Ok(n) = note_of(val) {
                            reply_to = Some(n);
                        }
                    }
                }
                "p" => {
                    if let Ok(n) = npub_of(val) {
                        mentions.push(Value::String(n));
                    }
                }
                _ => {}
            }
        }
    }

    let mut ev = json!({
        "m": "event",
        "kind": "said",
        "address": address,
        "author": {"id": author_npub, "display": author_npub},
        "content": {"text": content},
        "origin": origin,
    });
    if let Some(r) = reply_to {
        ev["reply_to"] = Value::String(r);
    }
    if !mentions.is_empty() {
        ev["mentions"] = Value::Array(mentions);
    }
    Some(ev)
}

// ============================ tests（オフライン・ネットワーク無し）============================
#[cfg(test)]
mod tests {
    use super::*;

    // ---- hex ----
    #[test]
    fn hex_roundtrip() {
        let b = [0x00u8, 0x0f, 0xa5, 0xff];
        assert_eq!(to_hex(&b), "000fa5ff");
        assert_eq!(from_hex("000fa5ff").unwrap(), b);
        assert!(from_hex("0g").is_err());
        assert!(from_hex("abc").is_err());
    }

    // ---- NIP-19 の正準ベクトル（NIP-19 仕様の test vectors・標準 bech32）----
    // npub: hex 3bf0c63f... ↔ npub180cvv07...（pubkey のペア）
    const NPUB_HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
    const NPUB_STR: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
    // nsec: hex 67dea2ed... ↔ nsec1vl029mg...（privkey のペア・末尾 fe5）
    const NSEC_HEX: &str = "67dea2ed018072d675f5415ecfaed7d2597555e202d85b3d65ea4e58d2d92ffa";
    const NSEC_STR: &str = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";

    // BIP-173 の正準ベクトル（hrp="a"・データ空）。**標準 bech32（Bech32m ではない）**であることを釘付けにする。
    // NIP-19 は標準 bech32。ここで変種が入れ替わると実ユーザーの npub をデコードできなくなる。
    #[test]
    fn bech32_variant_is_standard_not_m() {
        assert_eq!(bech32_encode("a", &[]).unwrap(), "a12uel5l");
    }

    #[test]
    fn npub_matches_nip19_vector() {
        assert_eq!(npub_of(NPUB_HEX).unwrap(), NPUB_STR);
        assert_eq!(npub_to_hex(NPUB_STR).unwrap(), NPUB_HEX);
    }

    #[test]
    fn nsec_matches_nip19_vector() {
        assert_eq!(nsec_of(NSEC_HEX).unwrap(), NSEC_STR);
        assert_eq!(nsec_to_hex(NSEC_STR).unwrap(), NSEC_HEX);
    }

    #[test]
    fn note_roundtrips_and_has_note_hrp() {
        let note = note_of(NPUB_HEX).unwrap();
        assert!(note.starts_with("note1"), "note hrp: {note}");
        assert_eq!(note_to_hex(&note).unwrap(), NPUB_HEX);
    }

    #[test]
    fn origin_accepts_note_and_raw_hex() {
        let note = note_of(NPUB_HEX).unwrap();
        assert_eq!(event_id_of_origin(&note).unwrap(), NPUB_HEX);
        assert_eq!(event_id_of_origin(NPUB_HEX).unwrap(), NPUB_HEX);
        assert!(event_id_of_origin("garbage").is_err());
    }

    // nsec から鍵を導けて、pubkey/npub が整合し、同じ nsec からは決定的に同じ鍵になる。
    // その鍵で署名 → 検証が往復する（導出・署名が正しい）。値の丸暗記はしない（実測は統合テストで）。
    #[test]
    fn key_from_nsec_is_consistent_and_deterministic() {
        let key = Key::from_nsec(NSEC_STR).unwrap();
        assert_eq!(key.pubkey_hex.len(), 64);
        assert!(is_hex64(&key.pubkey_hex));
        assert_eq!(key.npub, npub_of(&key.pubkey_hex).unwrap());
        let again = Key::from_nsec(NSEC_STR).unwrap();
        assert_eq!(key.pubkey_hex, again.pubkey_hex, "同じ nsec は同じ鍵");
        let (_id, ev) = build_signed(&key, 1, json!([]), "x", 1_700_000_000);
        verify_event(&ev).expect("nsec-derived key signs verifiably");
    }

    // ---- NIP-01 の直列化と id ----
    // 正準直列化の文字列を厳密に固定する（id = sha256(この文字列) なので、これで id 算法が完全に釘付けになる）。
    #[test]
    fn id_serialization_is_exact() {
        let tags = json!([["e", "abc"], ["p", "def"]]);
        let s = serialize_for_id(NPUB_HEX, 1_700_000_000, 1, &tags, "hello \"world\"\nline2");
        assert_eq!(
            s,
            r#"[0,"3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d",1700000000,1,[["e","abc"],["p","def"]],"hello \"world\"\nline2"]"#
        );
    }

    // 生成鍵で署名 → 検証が往復する（署名・検証の実装が正しい）。
    #[test]
    fn sign_then_verify_roundtrips() {
        let key = Key::generate();
        let (_id, event) = build_signed(&key, 1, json!([]), "テスト本文", 1_700_000_000);
        verify_event(&event).expect("own signature must verify");
        // 中身を 1 文字変えると検証が落ちる（id が本文に依存している）。
        let mut tampered = event.clone();
        tampered["content"] = json!("改竄");
        assert!(verify_event(&tampered).is_err());
    }

    #[test]
    fn generated_keys_are_distinct() {
        let a = Key::generate();
        let b = Key::generate();
        assert_ne!(a.pubkey_hex, b.pubkey_hex, "使い捨て鍵は毎回別");
    }

    // ---- 効果の組み立て ----
    #[test]
    fn say_without_target_has_no_tags() {
        let key = Key::generate();
        let (_id, ev) = build_say(&key, "やあ", None, 1_700_000_000).unwrap();
        assert_eq!(ev["kind"], json!(1));
        assert_eq!(ev["tags"], json!([]));
        assert_eq!(ev["content"], json!("やあ"));
    }

    #[test]
    fn say_with_reply_target_adds_e_tag() {
        let key = Key::generate();
        let note = note_of(NPUB_HEX).unwrap();
        let (_id, ev) = build_say(&key, "返信", Some(&note), 1_700_000_000).unwrap();
        assert_eq!(ev["tags"], json!([["e", NPUB_HEX]]));
    }

    #[test]
    fn react_requires_target_and_is_kind7() {
        let key = Key::generate();
        let note = note_of(NPUB_HEX).unwrap();
        let (_id, ev) = build_react(&key, "+", &note, 1_700_000_000).unwrap();
        assert_eq!(ev["kind"], json!(7));
        assert_eq!(ev["content"], json!("+"));
        assert_eq!(ev["tags"], json!([["e", NPUB_HEX]]));
    }

    // boost = NIP-18 kind-6。対象の e-tag のみ・本文なし。
    #[test]
    fn boost_is_kind6_with_e_tag() {
        let key = Key::generate();
        let note = note_of(NPUB_HEX).unwrap();
        let (id_hex, ev) = build_boost(&key, &note, 1_700_000_000).unwrap();
        assert_eq!(ev["kind"], json!(6));
        assert_eq!(ev["content"], json!(""));
        assert_eq!(ev["tags"], json!([["e", NPUB_HEX]]));
        // リポストは外界に新しい投稿（kind6）を作る → 識別子が付き、ack で origin(note1) を返せる（§04）。
        // これが無いと、自分のリポストを後から取り消せない・反応できない。
        let origin = note_of(&id_hex).unwrap();
        assert!(origin.starts_with("note1"), "boost origin: {origin}");
        assert_eq!(
            event_id_of_origin(&origin).unwrap(),
            id_hex,
            "origin ↔ event id 往復"
        );
        // 生の 64-hex の target も受ける。
        let (_id2, ev2) = build_boost(&key, NPUB_HEX, 1_700_000_000).unwrap();
        assert_eq!(ev2["tags"], json!([["e", NPUB_HEX]]));
    }

    // quote = NIP-18 kind-1 + q-tag。本文つき・新しい投稿なので origin が付く（呼び手が note_of する）。
    #[test]
    fn quote_is_kind1_with_q_tag_and_text() {
        let key = Key::generate();
        let note = note_of(NPUB_HEX).unwrap();
        let (id_hex, ev) = build_quote(&key, "これ好き", &note, 1_700_000_000).unwrap();
        assert_eq!(ev["kind"], json!(1));
        assert_eq!(ev["content"], json!("これ好き"));
        assert_eq!(ev["tags"], json!([["q", NPUB_HEX]]));
        // 引用は識別子が付く → origin を後から指せる。
        assert!(note_of(&id_hex).unwrap().starts_with("note1"));
    }

    // retract = NIP-09 kind-5。削除対象の e-tag のみ。
    #[test]
    fn retract_is_kind5_with_e_tag() {
        let key = Key::generate();
        let note = note_of(NPUB_HEX).unwrap();
        let (_id, ev) = build_retract(&key, &note, 1_700_000_000).unwrap();
        assert_eq!(ev["kind"], json!(5));
        assert_eq!(ev["tags"], json!([["e", NPUB_HEX]]));
    }

    // どの効果も不正な target は err（origin を event id に写せない）。
    #[test]
    fn new_effects_reject_bad_target() {
        let key = Key::generate();
        assert!(build_boost(&key, "garbage", 1_700_000_000).is_err());
        assert!(build_quote(&key, "x", "garbage", 1_700_000_000).is_err());
        assert!(build_retract(&key, "garbage", 1_700_000_000).is_err());
    }

    // ---- 住所 → フィルタ ----
    #[test]
    fn npub_address_maps_to_author_kind1() {
        let f = parse_address(NPUB_STR).unwrap();
        assert_eq!(f, json!({"authors": [NPUB_HEX], "kinds": [1]}));
    }

    #[test]
    fn filter_address_maps_kind_and_author() {
        let f = parse_address(&format!("filter:kind=1&author={NPUB_STR}")).unwrap();
        assert_eq!(f["kinds"], json!([1]));
        assert_eq!(f["authors"], json!([NPUB_HEX]));
    }

    #[test]
    fn filter_unknown_key_errs() {
        // 知らないキーは近いものに寄せず err（黙って通さない）。
        assert!(parse_address("filter:since=100").is_err());
        assert!(parse_address("filter:kind=abc").is_err());
        assert!(parse_address("filter:noequalsign").is_err());
    }

    // ---- 受信 note → core の event ----
    #[test]
    fn incoming_builds_said_with_origin_reply_and_mentions() {
        let their_pub = NPUB_HEX; // 相手（自分ではない）
        let note_id = "1111111111111111111111111111111111111111111111111111111111111111";
        let replied = "2222222222222222222222222222222222222222222222222222222222222222";
        let mentioned = "3333333333333333333333333333333333333333333333333333333333333333";
        let raw = json!({
            "id": note_id,
            "pubkey": their_pub,
            "content": "エージェントA、これ見た？",
            "tags": [["e", replied], ["p", mentioned]],
        });
        let ev = incoming_to_core_event(&raw, "filter:kind=1", "ff".repeat(32).as_str()).unwrap();
        assert_eq!(ev["kind"], json!("said"));
        assert_eq!(ev["address"], json!("filter:kind=1"));
        assert_eq!(ev["author"]["id"], json!(npub_of(their_pub).unwrap()));
        assert_eq!(ev["content"]["text"], json!("エージェントA、これ見た？"));
        assert_eq!(ev["origin"], json!(note_of(note_id).unwrap()));
        assert_eq!(ev["reply_to"], json!(note_of(replied).unwrap()));
        assert_eq!(ev["mentions"], json!([npub_of(mentioned).unwrap()]));
    }

    #[test]
    fn incoming_drops_self_authored() {
        let me = NPUB_HEX;
        let raw = json!({
            "id": "1111111111111111111111111111111111111111111111111111111111111111",
            "pubkey": me,
            "content": "自分の投稿",
            "tags": [],
        });
        // 自分の pubkey と一致 → 入力へ戻さない。
        assert!(incoming_to_core_event(&raw, "filter:kind=1", me).is_none());
    }
}
