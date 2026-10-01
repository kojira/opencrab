//! system prompt のキャッシュ区切り（D-1056）。

/// system prompt の区切り（D-1056）。組み立て側は `[固定] BREAK [caller 依存] BREAK [リクエスト毎]`
/// の順に置く。エンジンが [`join_system_segments`] で `"\n\n"` 連結した 1 本の system 本文に
/// 戻し、区切り位置は [`SYSTEM_CACHE_SEGMENTS_METADATA`] として ChatRequest.metadata に載せる。
/// プロバイダへこの文字自体は届かない。
pub const SYSTEM_SEGMENT_BREAK: char = '\u{1E}';

/// ChatRequest.metadata のキー。値は結合後 system 本文のバイト終端オフセット配列
/// （固定部の末尾・caller 依存部の末尾。空セグメントの分は載せない）。
pub const SYSTEM_CACHE_SEGMENTS_METADATA: &str = "system_cache_segments";

/// `SYSTEM_SEGMENT_BREAK` で区切った system を、空セグメントを落として `"\n\n"` で連結する。
/// 戻り値の 2 つ目は、セグメント 0（固定部）と 1（caller 依存部）のうち非空のものの結合後
/// バイト終端（= キャッシュの区切り位置）。区切りが 1 つも無ければ `None`。
pub fn join_system_segments(system: &str) -> (String, Option<Vec<usize>>) {
    if !system.contains(SYSTEM_SEGMENT_BREAK) {
        return (system.to_string(), None);
    }
    let parts: Vec<&str> = system.split(SYSTEM_SEGMENT_BREAK).collect();
    let mut joined = String::new();
    let mut ends = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if !joined.is_empty() {
            joined.push_str("\n\n");
        }
        joined.push_str(part);
        if i < 2 {
            ends.push(joined.len());
        }
    }
    (joined, Some(ends))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-1056: 区切り文字は結合後に残らず、終端オフセットは固定部・caller 部の末尾を指す。
    #[test]
    fn join_system_segments_offsets_and_no_break_char() {
        let b = SYSTEM_SEGMENT_BREAK;
        let (joined, ends) = join_system_segments(&format!("stable{b}skills{b}nostr"));
        assert_eq!(joined, "stable\n\nskills\n\nnostr");
        let ends = ends.unwrap();
        assert_eq!(&joined[..ends[0]], "stable");
        assert_eq!(&joined[..ends[1]], "stable\n\nskills");
        assert!(!joined.contains(b));

        // 空の caller 部は落とす（区切りは固定部の 1 点だけ）。
        let (joined, ends) = join_system_segments(&format!("stable{b}{b}nostr"));
        assert_eq!(joined, "stable\n\nnostr");
        assert_eq!(ends.unwrap(), vec!["stable".len()]);

        // caller 部が末尾でも区切り位置に載る（リクエスト毎の部分が無い経路）。
        let (joined, ends) = join_system_segments(&format!("stable{b}skills"));
        assert_eq!(ends.unwrap(), vec!["stable".len(), joined.len()]);

        // 区切りなしは素通し。
        let (joined, ends) = join_system_segments("plain");
        assert_eq!(joined, "plain");
        assert!(ends.is_none());
    }
}
