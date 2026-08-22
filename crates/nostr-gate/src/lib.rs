//! nostr ゲートの純関数（NIP-01/NIP-19・フィルタ変換・core 出来事の組み立て）とリレー補助。
//! bin（`nostr-gate`）と tests/relay.rs（実リレー・#[ignore]）が同じ実装を共有するための lib。

pub mod nostr;
pub mod relay;
