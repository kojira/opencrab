//! DESIGN-WEBGATE §7.4 connected / disconnected プロセス E2E。
//! Binding PUT は使わず POST /api/agents/{id}/web-conversations だけが binding を作る。

include!("web_conversation_create_e2e/support.rs");
include!("web_conversation_create_e2e/connected.rs");
include!("web_conversation_create_e2e/disconnected.rs");
include!("web_conversation_create_e2e/reconnect.rs");
