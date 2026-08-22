//! 発火方針。場が持つ（基本§05）。ゲートの性質ではない。
//!
//! 溜まりは状態として持たない — 「read_seq より後があるか」で毎回求まる（詳細§02）。

use opencrab_port::Property;
use opencrab_port::SubjectId;
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImmediateFrom {
    /// 参加者からのものだけ即応の対象にする。それ以外は溜める。
    ParticipantsOnly,
    /// 誰からのものでも即応の対象にする（公開の場で効く）。
    Anyone,
}

#[derive(Clone, Debug)]
pub struct Policy {
    /// どの性質が即応を起こすか。空なら即応しない。
    pub immediate: BTreeSet<Property>,
    pub immediate_from: ImmediateFrom,
    /// まとめの窓（ミリ秒）。None なら「まとめない」（既定・詳細§14）。
    pub batch_window_ms: Option<i64>,
    /// 無条件に撃つ間隔（ミリ秒）。None なら「撃たない」（既定）。
    pub unconditional_interval_ms: Option<i64>,
    /// 宛先が無いとき、誰のターンにするか。None なら誰も返さない。
    pub default_subject: Option<SubjectId>,
}

impl Default for Policy {
    /// 既定は「まとめない・撃たない」（詳細§14）。即応は無し。
    fn default() -> Policy {
        Policy {
            immediate: BTreeSet::new(),
            immediate_from: ImmediateFrom::Anyone,
            batch_window_ms: None,
            unconditional_interval_ms: None,
            default_subject: None,
        }
    }
}

fn prop_str(p: Property) -> &'static str {
    match p {
        Property::MentionsMe => "mentions_me",
        Property::RepliesToMe => "replies_to_me",
        Property::Direct => "direct",
    }
}
fn prop_from(s: &str) -> Option<Property> {
    Some(match s {
        "mentions_me" => Property::MentionsMe,
        "replies_to_me" => Property::RepliesToMe,
        "direct" => Property::Direct,
        _ => return None,
    })
}

impl Policy {
    pub fn immediate_on(props: &[Property]) -> Policy {
        Policy {
            immediate: props.iter().copied().collect(),
            ..Policy::default()
        }
    }

    pub fn with_immediate(mut self, props: &[Property]) -> Policy {
        self.immediate = props.iter().copied().collect();
        self
    }
    pub fn with_from(mut self, f: ImmediateFrom) -> Policy {
        self.immediate_from = f;
        self
    }
    pub fn with_batch_ms(mut self, ms: i64) -> Policy {
        self.batch_window_ms = Some(ms);
        self
    }
    pub fn with_unconditional_ms(mut self, ms: i64) -> Policy {
        self.unconditional_interval_ms = Some(ms);
        self
    }
    pub fn with_default(mut self, s: SubjectId) -> Policy {
        self.default_subject = Some(s);
        self
    }

    pub fn to_json(&self) -> String {
        let imm: Vec<&str> = self.immediate.iter().map(|p| prop_str(*p)).collect();
        serde_json::json!({
            "immediate": imm,
            "immediate_from": match self.immediate_from {
                ImmediateFrom::ParticipantsOnly => "participants_only",
                ImmediateFrom::Anyone => "anyone",
            },
            "batch_window_ms": self.batch_window_ms,
            "unconditional_interval_ms": self.unconditional_interval_ms,
            "default_subject": self.default_subject,
        })
        .to_string()
    }

    /// 壊れた JSON や未知の値を既定・緩い方へ倒さない。読めないものは `Err` を返す（詳細§15）。
    ///
    /// **落とし方は呼び手が決める**（§15「落とし方は 2 通りある」）:
    /// - 自分が保存した値（`places.policy_json`）を読む時は `expect` で落ちてよい（DB の破損＝異常）。
    /// - エージェントが組んだ引数を読む時は `Err` を失敗として返す — **エージェントの引数で core を殺さない**。
    ///
    /// `to_json` が書く形は常に完全なので、健全なデータでは `Err` にならない。
    pub fn from_json(s: &str) -> Result<Policy, String> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("invalid policy JSON: {e}"))?;
        let mut immediate = BTreeSet::new();
        if let Some(arr) = v.get("immediate").and_then(|x| x.as_array()) {
            for e in arr {
                let name = e
                    .as_str()
                    .ok_or_else(|| "immediate entry must be a string".to_string())?;
                let p = prop_from(name).ok_or_else(|| format!("unknown property: {name}"))?;
                immediate.insert(p);
            }
        }
        // 未知値を緩い方（Anyone）へ倒さない。欠落・未知はどちらも失敗として返す。
        let immediate_from = match v.get("immediate_from").and_then(|x| x.as_str()) {
            Some("participants_only") => ImmediateFrom::ParticipantsOnly,
            Some("anyone") => ImmediateFrom::Anyone,
            other => return Err(format!("unknown or missing immediate_from: {other:?}")),
        };
        Ok(Policy {
            immediate,
            immediate_from,
            batch_window_ms: v.get("batch_window_ms").and_then(|x| x.as_i64()),
            unconditional_interval_ms: v.get("unconditional_interval_ms").and_then(|x| x.as_i64()),
            default_subject: v.get("default_subject").and_then(|x| x.as_i64()),
        })
    }
}
