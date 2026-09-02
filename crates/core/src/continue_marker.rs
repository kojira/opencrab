//! `CONTINUE` 継続マーカー（DESIGN-TURN-CONTINUATION §11・末尾マーカー方式・#890）。
//!
//! `CONTINUE` は `NO_REPLY` と対の制御トークン。生成 content の**末尾**
//! （末尾空白を除いた最後の行/トークン）にあれば「このターンで作業を続ける意思」を表す。
//! エンジンはマーカーを剥がして次のイテレーションへ進み、剥がした本文だけを配送・保存する。
//!
//! ## 配置の設計逸脱（§11.5 との差分・要レビュー）
//! 設計 §11.5 は `no_reply.rs`（`crates/actions`）の隣に単一実装を置くことを指示するが、
//! 継続を判定するのは engine（`crates/core::engine::skill_engine`）で、**core は actions に
//! 依存できない**（依存は `actions → core` の一方向）。両者から使える最下層の `crates/core`
//! に単一実装として置く。3 配送点（`session_inbound` / discord / nostr）はいずれも core に
//! 依存するので `opencrab_core::strip_continue_marker` を共有する。`NO_REPLY` の実体は
//! `opencrab_actions::NO_REPLY_SENTINEL` にあるが、core からは参照できないため優先判定用に
//! [`NO_REPLY_SENTINEL`] へ複製する（両者の一致は actions 側テストで固定する）。

/// 「このターンを続ける」継続センチネル。エンジンはこの末尾出現で次イテレーションへ進む。
pub const CONTINUE_SENTINEL: &str = "CONTINUE";

/// 末尾以外出現の WARN タグ（`grep -c` 用に 1 語不変）。
pub const CONTINUE_MIDTEXT_TAG: &str = "continue_marker_midtext";

/// 継続マーカーログの tracing target。
pub const CONTINUE_LOG_TARGET: &str = "opencrab::continue_marker";

/// `NO_REPLY` センチネルのミラー（`opencrab_actions::NO_REPLY_SENTINEL` の複製）。
///
/// core は actions に依存できないため、NO_REPLY 優先（§11.1）の判定用にここへ複製する。
/// 両者の値が一致することは `opencrab_actions` 側の consistency テストで固定する。
pub const NO_REPLY_SENTINEL: &str = "NO_REPLY";

/// `CONTINUE` 末尾マーカーの剥がし結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueMarker {
    /// マーカー除去後の本文。末尾マーカーが無ければ入力そのまま。
    kept: String,
    /// 末尾（末尾空白除去後の最後の行/トークン）が `CONTINUE` で剥がしたか。
    at_tail: bool,
    /// 末尾以外に `CONTINUE` が現れたバイト位置（剥がさない・継続しない・WARN 対象）。
    /// `at_tail` が真のときは常に `None`。
    midtext_offset: Option<usize>,
}

/// content の末尾 `CONTINUE` マーカーを検出して剥がす（純粋・ログは出さない）。
///
/// - 末尾（末尾空白除去後の最後の行/トークン）が `CONTINUE` → 剥がして `at_tail=true`。
/// - 末尾以外に `CONTINUE` があれば剥がさず `midtext_offset=Some(pos)`（WARN 対象）。
/// - どちらでもなければ入力そのまま。
pub fn strip_continue_marker(content: &str) -> ContinueMarker {
    // 末尾空白を除いた最後の行/トークンが `CONTINUE` か。単語密着（例 `DISCONTINUE`）は
    // 末尾トークンではないので、直前が空白のときだけ末尾マーカーとみなす。
    let trimmed_end = content.trim_end();
    let is_tail = trimmed_end == CONTINUE_SENTINEL
        || (trimmed_end.ends_with(CONTINUE_SENTINEL) && {
            let prefix = &trimmed_end[..trimmed_end.len() - CONTINUE_SENTINEL.len()];
            prefix
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace())
        });
    if is_tail {
        let kept = trimmed_end[..trimmed_end.len() - CONTINUE_SENTINEL.len()]
            .trim_end()
            .to_string();
        ContinueMarker {
            kept,
            at_tail: true,
            midtext_offset: None,
        }
    } else {
        // 末尾に無いなら剥がさない。どこかに出現していれば WARN 用に位置だけ拾う。
        let midtext_offset = content.find(CONTINUE_SENTINEL);
        ContinueMarker {
            kept: content.to_string(),
            at_tail: false,
            midtext_offset,
        }
    }
}

impl ContinueMarker {
    /// 末尾 `CONTINUE` を剥がしたか（＝このターンを継続する意思）。
    pub fn at_tail(&self) -> bool {
        self.at_tail
    }

    /// マーカー除去後の本文。
    pub fn kept(&self) -> &str {
        &self.kept
    }

    /// マーカー除去後の本文を所有権つきで取り出す。
    pub fn into_kept(self) -> String {
        self.kept
    }

    /// 末尾以外に `CONTINUE` が現れたバイト位置（WARN 対象）。末尾剥がし時は `None`。
    pub fn midtext_offset(&self) -> Option<usize> {
        if self.at_tail {
            None
        } else {
            self.midtext_offset
        }
    }

    /// 末尾以外出現を WARN で残す（§11.1・構造化フィールド session/agent/offset）。
    ///
    /// 末尾剥がし・出現無しでは何も出さない。core は actions の `DeliveryContext` に依存
    /// できないため、相関キーは素の `&str` で受ける。
    pub fn log_midtext(&self, session_id: &str, agent_id: &str, origin: &str) {
        let Some(offset) = self.midtext_offset() else {
            return;
        };
        tracing::warn!(
            target: CONTINUE_LOG_TARGET,
            offset,
            session_id = %session_id,
            agent_id = %agent_id,
            origin = %origin,
            "continue_marker_midtext"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_marker_keeps_full_text() {
        let m = strip_continue_marker("普通の本文\nです");
        assert!(!m.at_tail());
        assert_eq!(m.kept(), "普通の本文\nです");
        assert_eq!(m.midtext_offset(), None);
    }

    #[test]
    fn tail_marker_on_own_line_is_stripped() {
        let m = strip_continue_marker("感想を返すね⚡\nCONTINUE");
        assert!(m.at_tail(), "末尾行が CONTINUE なら剥がす");
        assert_eq!(m.kept(), "感想を返すね⚡");
        assert_eq!(m.midtext_offset(), None);
    }

    #[test]
    fn tail_marker_after_space_is_stripped() {
        let m = strip_continue_marker("作業を続ける CONTINUE");
        assert!(m.at_tail());
        assert_eq!(m.kept(), "作業を続ける");
    }

    #[test]
    fn tail_marker_with_trailing_whitespace_is_stripped() {
        let m = strip_continue_marker("続ける\nCONTINUE  \n\t");
        assert!(m.at_tail(), "末尾空白除去後の最後のトークンが CONTINUE");
        assert_eq!(m.kept(), "続ける");
    }

    #[test]
    fn standalone_marker_yields_empty_kept() {
        let m = strip_continue_marker("CONTINUE");
        assert!(m.at_tail());
        assert_eq!(m.kept(), "");
    }

    #[test]
    fn midtext_marker_is_not_stripped_and_flags_offset() {
        // 途中出現（末尾以外）は剥がさず本文のまま・WARN 対象（§11.1）。
        let text = "まず CONTINUE を確認してから続けます";
        let m = strip_continue_marker(text);
        assert!(!m.at_tail(), "末尾以外は継続の足がかりにしない");
        assert_eq!(m.kept(), text, "途中出現は本文をそのまま残す");
        assert_eq!(m.midtext_offset(), Some(text.find("CONTINUE").unwrap()));
    }

    #[test]
    fn marker_as_substring_of_word_is_not_a_tail_marker() {
        // 単語末尾に密着した CONTINUE は末尾トークンではない（前が空白でない）。
        let m = strip_continue_marker("これはDISCONTINUE");
        assert!(!m.at_tail());
        assert_eq!(m.kept(), "これはDISCONTINUE");
    }
}
