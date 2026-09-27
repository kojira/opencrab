//! Issue #1006 S3 generic timed-fire routing contract.
use opencrab_actions::TimedFireRouter;

#[test]
fn generic_router_has_no_static_kind_or_descriptor_registry() {
    let router = TimedFireRouter::new();
    assert!(!router.has_live_sink());
    assert_eq!(router.fire_target_hint(), "ゲートに接続した会話");
}

#[test]
fn persisted_route_uses_only_canonical_binding_and_session_ids() {
    let conn = opencrab_db::init_memory().unwrap();
    assert!(TimedFireRouter::new()
        .resolve_persisted_target(&conn, "missing", "agent")
        .is_none());
}
