// ピアレビュー依頼（#157 S7）。定義は gateway 非依存層が持つ（transport の
// 配送口の有無に関わらず露出する）。
fn request_peer_review_definition() -> GatewayActionDef {
    crate::peer_review::request_peer_review_definition()
}

