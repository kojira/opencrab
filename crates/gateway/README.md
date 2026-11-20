# opencrab-gateway

エージェントに見せる「gateway アクション」（道具）の抽象だけを持つ crate。具象 transport
（Discord / Nostr などの SDK や接続）は含まない。gateway は別プロセスで
（[opencrab-gateways](https://github.com/kojira/opencrab-gateways)、
[opencrab-bluesky-gateway](https://github.com/kojira/opencrab-bluesky-gateway)）、
core とは gate socket でつながる（`docs/design/external-gate.md`）。

中身は `src/traits.rs` の 1 ファイル。

## GatewayActions trait

```rust
#[async_trait]
pub trait GatewayActions: Send + Sync {
    fn definitions(&self) -> Vec<GatewayActionDef>;
    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult;
}
```

実装しているもの:

- `SystemGatewayActions`（`crates/server/src/system_actions/`）— core 自身の道具（設定、許可コマンド、サブタスク、定期実行、スキル作成など）。どのターンにもある。
- `ExtgateOpsGatewayActions`（`crates/extgate/src/ops_projection.rs`）— gateway が `hello` で宣言した道具。その gateway の会話のターンにだけ出し、呼ばれたら `invoke` で gateway に送る。
- MCP（`crates/mcp`）— `mcp__<server>__<tool>`。

`args` は **LLM 由来のツール引数のみ**。呼び出し元の権限・セッション・返信先などの
実行コンテキストは JSON に混ぜず `GatewayCallContext` で型付きに渡す（#36）。

### GatewayCallContext

| フィールド | 説明 |
|---|---|
| `caller` | `GatewayCaller`（`Owner` / `Agent` / `CoAgent` / `TrustedUser`）。権限判定は enum match で行う |
| `session_id` | 呼び出し元エンジンのセッション ID。無ければ `None`（セッション必須のアクションは fail-closed） |
| `depth` | sub-engine のネスト深さ（メイン = 0） |
| `agent_id` | 実行中のエージェント |
| `root_gateway` | 自身を包む合成 gateway へのハンドル（#152 S2） |
| `reply_target` | inbound メッセージの返信先（gateway 不透明トークン / #158 S1） |
| `tool_call_id` | この実行を起こした tool_call の id（#915） |

### GatewayActionDef と ToolClass

各道具は名前・説明・パラメータ（JSON Schema）と、必須の分類 `ToolClass`
（`dispatch`: `DispatchMode`、`sub_engine`: `SubEngineAccess`、`sharing`: `ToolSharing`）を持つ。
`ToolClass` は `Default` を持たないので、新しい道具は分類を書かないとコンパイルできない。

## 使い方

```toml
# Cargo.toml
[dependencies]
opencrab-gateway = { workspace = true }
```

feature は持たない。SDK（serenity / songbird など）には依存しない（`scripts/check-deps.sh` が検査する）。
