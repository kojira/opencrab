//! DESIGN-WEBGATE §7.4 connected / disconnected プロセス E2E。
//! Binding PUTは使わず、Web gateway自身のPOST /api/web-conversationsだけがbindingを作る。

include!("web_conversation_create_e2e/support.rs");
include!("web_conversation_create_e2e/connected.rs");
include!("web_conversation_create_e2e/disconnected.rs");
include!("web_conversation_create_e2e/reconnect.rs");
