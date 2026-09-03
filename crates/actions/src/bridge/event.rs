/// ツール 1 件の実行イベント種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolEventStatus {
    Started,
    Completed,
    Failed,
    Rejected,
}

/// 1 ツール実行イベントの観測データ（webhook 等の sink へ渡す）。
///
/// #620: 旧来の「`nsec` キー名でマスクする」sink ゲート（SECRET_KEYS / #519・#526）は撤去した。
/// キー名一致は実際の混入（別の文字列値の中に鍵が含まれる形）を検出できず、`nsec` を JSON の
/// キーに持つ引数/結果を出す producer も皆無だった（列挙で確認）。鍵は at-rest 暗号化と実行時
/// env 注入で「エージェントの読める範囲の外」に置く方式へ移し、事後マスクに依存しない。
/// 整形（要約・サイズ分割）は従来どおり sink 側。
pub struct ToolEvent<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub agent_id: &'a str,
    pub session_id: Option<&'a str>,
    pub depth: u32,
    pub status: ToolEventStatus,
    pub started_at: &'a str,
    pub duration_ms: Option<u64>,
    pub args: &'a serde_json::Value,
    pub result: Option<&'a serde_json::Value>,
    pub error: Option<&'a str>,
}

/// ツール実行イベントの sink。executor が start/terminal で呼ぶ。
pub trait ToolEventSink: Send + Sync {
    fn on_event(&self, event: &ToolEvent<'_>);
}
