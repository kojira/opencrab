/// 会話ローカルの短縮参照（§9A）。u=外部話者 / e=受信イベント / c=tool call。全順序ログの
/// 初出順で採番する。写像は決定的（同じログ列 → 同じ番号）なので追加の永続や migration は
/// 不要で、番号は不変（append-only ログの初出順が安定なため）。core の enum/DB/error に個別
/// platform 語彙を足さず、log の汎用 field（speaker_id / external_origin / tool_call id）で採る。
#[derive(Debug, Default, Clone)]
pub struct ConversationRefs {
    agent_id: String,
    /// 自分（agent_id）の表示名（agents.name）。空/未設定なら agent_id のまま。
    agent_name: Option<String>,
    speakers: std::collections::HashMap<String, usize>,
    events: std::collections::HashMap<String, usize>,
    /// event_id（origin 末尾の 64hex）→ e 番号。返信/リアクションの対象解決（row295c 6b）に使う。
    event_ids: std::collections::HashMap<String, usize>,
    calls: std::collections::HashMap<String, usize>,
    /// subtask_id → s 番号（セッション局所・初出順）。spawn 受理と完了本文の両方から採る。
    subtasks: std::collections::HashMap<String, usize>,
}

impl ConversationRefs {
    /// 全順序ログから初出順で採番する。
    pub fn build(logs: &[opencrab_db::queries::SessionLogRow], agent_id: &str) -> Self {
        let mut refs = ConversationRefs {
            agent_id: agent_id.to_string(),
            ..Default::default()
        };
        for log in logs {
            match log.log_type.as_str() {
                "speech" => {
                    if let Some(sp) = log.speaker_id.as_deref() {
                        if sp != agent_id && !refs.speakers.contains_key(sp) {
                            let n = refs.speakers.len() + 1;
                            refs.speakers.insert(sp.to_string(), n);
                        }
                    }
                    if let Some(origin) = external_origin_of(log) {
                        if !refs.events.contains_key(&origin) {
                            let n = refs.events.len() + 1;
                            // event_id（origin 末尾の 64hex）→ e 番号も引けるようにする（row295c 6b
                            // の (reply→e番号) 解決）。origin lane が違っても event_id で照合する。
                            if let Some(eid) = event_id_of_origin(&origin) {
                                refs.event_ids.entry(eid).or_insert(n);
                            }
                            refs.events.insert(origin, n);
                        }
                    }
                }
                "tool_call" => {
                    for id in tool_call_ids_of(log) {
                        refs.assign_call(&id);
                    }
                }
                "tool_result" | "tool_cancelled" => {
                    if let Some(id) = tool_call_id_of_result(log) {
                        refs.assign_call(&id);
                    }
                    // spawn 受理の tool_result 本文 `{"data":{"subtask_id":…}}` から採番（初出）。
                    if let Some(sid) = subtask_id_of_tool_result(log) {
                        refs.assign_subtask(&sid);
                    }
                }
                "system" => {
                    // 完了本文 `{"type":"subtask_completed","subtask_id":…}` からも採番。
                    if let Some(sid) = subtask_id_of_system(log) {
                        refs.assign_subtask(&sid);
                    }
                }
                _ => {}
            }
        }
        refs
    }

    /// 自分の表示名を設定する（組み立て側が agents.name を引いて渡す）。空は無視。
    pub fn set_agent_name(&mut self, name: impl Into<String>) {
        let name = name.into();
        if !name.is_empty() {
            self.agent_name = Some(name);
        }
    }

    fn assign_call(&mut self, id: &str) {
        if !self.calls.contains_key(id) {
            let n = self.calls.len() + 1;
            self.calls.insert(id.to_string(), n);
        }
    }

    fn assign_subtask(&mut self, id: &str) {
        if !id.is_empty() && !self.subtasks.contains_key(id) {
            let n = self.subtasks.len() + 1;
            self.subtasks.insert(id.to_string(), n);
        }
    }

    /// 話者表示。自分は名前だけ（§9A.2・生 UUID を出さない）、外部話者は u 番号。
    pub(super) fn speaker_label(&self, speaker: &str) -> String {
        if speaker == self.agent_id {
            self.agent_name
                .clone()
                .unwrap_or_else(|| speaker.to_string())
        } else if let Some(n) = self.speakers.get(speaker) {
            format!("u{n}")
        } else {
            speaker.to_string()
        }
    }

    pub(crate) fn event_of(&self, log: &opencrab_db::queries::SessionLogRow) -> Option<usize> {
        external_origin_of(log).and_then(|o| self.events.get(&o).copied())
    }

    pub(super) fn call_of(&self, id: &str) -> Option<usize> {
        self.calls.get(id).copied()
    }

    /// 短縮参照トークン（`uN` / `eN` / `cN`）を裏の実 ID へ逆引きする（§9A・DI 能力の引数解決）。
    /// `uN`→話者 speaker_id（Nostr では pubkey）、`eN`→受信イベントの external_origin、
    /// `cN`→tool_call id。汎用（platform 非依存）で、未知トークンや未割当番号は None。
    pub fn resolve_short_ref(&self, token: &str) -> Option<String> {
        let token = token.trim();
        let mut chars = token.chars();
        let prefix = chars.next()?;
        let num: usize = chars.as_str().parse().ok()?;
        let map = match prefix {
            'u' => &self.speakers,
            'e' => &self.events,
            'c' => &self.calls,
            _ => return None,
        };
        map.iter()
            .find(|(_, &n)| n == num)
            .map(|(id, _)| id.clone())
    }

    /// subtask_id → s 番号（未知は None）。
    pub(super) fn subtask_of(&self, id: &str) -> Option<usize> {
        self.subtasks.get(id).copied()
    }

    /// event_id → e 番号（会話内に無い＝未知は None → 表示側は `→外部`）。
    pub(super) fn event_num_by_id(&self, event_id: &str) -> Option<usize> {
        self.event_ids.get(event_id).copied()
    }
}

/// origin（`…:<lane>:<event_id>`）末尾の 64hex を取り出す。特定 SDK 名に依存しない。
fn event_id_of_origin(origin: &str) -> Option<String> {
    let last = origin.rsplit(':').next()?;
    if last.len() == 64 && last.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(last.to_ascii_lowercase())
    } else {
        None
    }
}

/// 受信メタが記録する対象ノート event_id（`reply_target`・row295c 6b）。旧行は未記録＝None。
pub(super) fn reply_target_of(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(log.metadata_json.as_deref()?).ok()?;
    meta.get("reply_target")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 完了本文（system・type=subtask_completed）の subtask_id。
fn subtask_id_of_system(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&log.content).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("subtask_completed") {
        return None;
    }
    v.get("subtask_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// spawn 受理の tool_result 本文の subtask_id（flat `{"subtask_id":…}` / data 包み形の両対応）。
fn subtask_id_of_tool_result(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&log.content).ok()?;
    let scope = v.get("data").unwrap_or(&v);
    scope
        .get("subtask_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 受信イベントの external_origin（inbound 記録が metadata に載せる汎用 field）。
pub(super) fn external_origin_of(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(log.metadata_json.as_deref()?).ok()?;
    meta.get("external_origin")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn tool_call_id_of_result(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(log.metadata_json.as_deref()?).ok()?;
    meta.get("tool_call_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// spawn 受理 tool_result（`data.status=="spawned"` の subtask_id）。並行バッチは 1 つの subtask を
/// call ごとに重複記録する（同一 subtask_id の spawn 受理が call 数だけ並ぶ）ので、表示では初出だけ
/// 残し 2 件目以降を落とす（row295 item4・二重表示）。組み立て側が seen 集合で判定する。
pub(crate) fn spawn_ack_subtask_id(log: &opencrab_db::queries::SessionLogRow) -> Option<String> {
    if log.log_type != "tool_result" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&log.content).ok()?;
    // spawn 受理は flat 形（`{"status":"spawned","subtask_id":…,"tool":…}`）。data 包み形にも一応対応。
    let scope = v.get("data").unwrap_or(&v);
    if scope.get("status").and_then(|s| s.as_str()) != Some("spawned") {
        return None;
    }
    scope
        .get("subtask_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn tool_call_ids_of(log: &opencrab_db::queries::SessionLogRow) -> Vec<String> {
    let Some(meta) = log
        .metadata_json
        .as_deref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
    else {
        return Vec::new();
    };
    let Some(tcj) = meta.get("tool_calls_json").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let Ok(calls) = serde_json::from_str::<serde_json::Value>(tcj) else {
        return Vec::new();
    };
    calls
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|it| it.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
