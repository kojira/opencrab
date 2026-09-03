/// 権限ポリシーによる拒否（実行に到達しなかった）を表す構造的マーカー。
///
/// gateway action 等が permission-check で拒否したときは、エラー文言の先頭へ
/// この安定コードを付ける（この定数を直接前置するか、`gateway_reject` 経由）。分類器は
/// この構造的な接頭辞を第一の根拠にする。`"permission"` / `"denied"` / `"forbidden"`
/// のような広い自然言語の部分一致は、実行されたが失敗した通常のエラー（例: OS の
/// "Permission denied"、shell の "Operation not permitted"）を rejected に誤分類
/// するため使わない。
pub const REJECTION_CODE_PREFIX: &str = "rejected: ";

/// 権限ポリシーによる拒否を `GatewayActionResult` として組み立てる（`rejected:` マーカー付き）。
pub(crate) fn gateway_reject(msg: impl Into<String>) -> opencrab_gateway::GatewayActionResult {
    let msg = msg.into();
    tracing::debug!(
        target: "webhook_audit",
        reason = %msg,
        "gateway action rejected by permission policy"
    );
    opencrab_gateway::GatewayActionResult {
        success: false,
        data: None,
        error: Some(format!("{REJECTION_CODE_PREFIX}{msg}")),
    }
}

/// エラー文言から「権限拒否（実行されなかった）」を判定する。
///
/// 優先: 構造的マーカー（`REJECTION_CODE_PREFIX`）。
/// 後方互換: まだマーカー化されていない経路向けに、曖昧さの少ない明示ドメイン
/// マーカーのみを許可する（広い NL 部分一致は誤検知になるため不可）。
pub(crate) fn is_rejection(error: Option<&str>) -> bool {
    let Some(e) = error else {
        return false;
    };
    // 構造的シグナル（権威）。
    if e.starts_with(REJECTION_CODE_PREFIX) {
        return true;
    }
    // 後方互換の明示ドメインマーカー（未マーカー化の owner-only gateway action 等）。
    // いずれも通常の OS/ツール失敗には現れない十分に固有なトークンに限定する。
    let lower = e.to_ascii_lowercase();
    [
        "owner-only",
        "requires owner",
        "forbidden_scope",
        "redacted read requires",
    ]
    .iter()
    .any(|p| lower.contains(p))
}
