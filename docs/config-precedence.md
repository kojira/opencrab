# 設定の優先順位（TOML と DB の真実の源）

opencrab の設定は 2 系統ある。どちらが勝つかの規則をここに固定する（#40）。

## 原則

- **TOML（config.toml）= グローバル既定・起動時設定。** プロセス全体の既定値
  （プロバイダ、共有 Discord ゲートウェイ、workspace ベースパス、ツール既定など）。
  変更はファイル編集で行い、hot-reload またはプロセス再起動で反映される。
- **DB = per-agent / per-channel のランタイム設定。** エージェント個別の Discord
  Bot（`agent_discord_config`）、チャンネル設定（`channel_config`）、エージェント別
  許可コマンド（`agent_allowed_commands`）、heartbeat 指示など。API / Discord
  アクション経由で実行中に変更でき、再起動不要で反映される。
- **粒度が細かい方が勝つ**: effective = channel 設定 ?? agent 設定 ?? グローバル既定。
  例: heartbeat 指示の解決順は channel(agent) > channel(全体) > agent > 組み込み既定
  （`resolve_heartbeat_instructions`）。

## Discord ゲートウェイの二重起動防止（DB 優先）

同一エージェントが「TOML の共有ゲートウェイ（`[gateway.discord].agent_ids`）」と
「DB の per-agent ゲートウェイ（`agent_discord_config` で enabled）」の両方に載ると、
1 つの発言に二重応答しうる。**DB の per-agent 設定が優先**され、次の 2 層で防ぐ:

1. **起動時**（`crates/server/src/main.rs`）: 共有ゲートウェイの対象エージェントを
   組み立てる際、enabled な `agent_discord_config` 行を持つエージェントを warn ログ
   付きで除外する。
2. **ランタイム**（`crates/discord/src/message_loop.rs`）: 共有ゲートウェイのループは
   メッセージ処理のたびに `AgentRunner::has_enabled_discord_config` を確認し、enabled
   な per-agent 設定を持つエージェントをスキップする。稼働中に per-agent 設定を
   enable した場合も、再起動なしで即座に共有側が処理をやめる。
   per-agent ゲートウェイ側のループ（`manager.rs`）はこのチェックを行わない
   （enabled な設定**から**起動されるため、チェックすると自分自身を skip してしまう）。

**対象外**: この dedupe は「同一エージェントの二重処理」を防ぐものであって、
別々の Bot トークンを持つ 2 つの bot が同じチャンネルに同居すること自体は制限しない
（それは正当な構成でありうる）。

DB 参照に失敗した場合、ランタイムチェックは false（= 共有側が処理を続行）に倒す。
per-agent ゲートウェイも同じ DB に依存しているため、DB 断で両方が沈黙するより
可用性を優先する。まれに二重応答する可能性はあるが、DB 断時の縮退として許容する。

## hot-reload と DB 設定が衝突しない理由

`crates/server/src/hot_reload.rs` は config.toml の変更を検知すると共有の
`ToolsConfig` を**丸ごと差し替える**。一見 DB 由来の許可コマンドが消えるように
見えるが、消えない: `agent_allowed_commands` は保存時に ToolsConfig へは書かれず、
**実行のたびに** DB から読んでエージェント専用のクローンへマージされる
（`process.rs` / `subtask_engine.rs` の executor 組み立て時）。共有 ToolsConfig は
常に「TOML の姿」だけを持ち、DB のランタイム追加分は per-call で合成される。

## 新しい設定を足すときの指針

- プロセス全体の既定値・接続情報 → TOML。
- エージェント/チャンネル単位で実行中に変えたい値 → DB（+ 必要なら API/アクション）。
- 両方に載る値を作る場合は、**effective 値の合成関数を 1 箇所に置き**、
  読み手が生の TOML/DB を直接参照しないこと（前例: heartbeat の解決関数、
  allowed_commands の per-call マージ、共有ゲートウェイの agent skip）。
