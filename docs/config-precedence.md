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
  例: heartbeat 指示の channel 設定は channel(agent) > channel(全体) の順で解決され、
  agent 設定と channel 設定の両方があるときは**連結**される（channel が agent を
  上書きするのではない。`resolve_heartbeat_instructions` 参照）。どちらも無ければ
  組み込み既定。

## ハートビートと定時実行のマスタスイッチ（#439 / #455）

- **ハートビート**は**セッション単位**の設定（`session_heartbeat_config`）に一本化された
  （#439/#456。旧 agent/channel スコープは廃止）。発火・アンカーは中央スケジューラが握る。
- **`[agent].heartbeat_enabled`（G）はハートビートのマスタスイッチ**。中央スケジューラは
  発火時に live（hot-reload 追従）で参照し、`discord-` セッションの HB を G でゲートする
  （`nostr-` は G 非依存）。
- **`agent_schedules`（#455 の定時実行）に G は掛からない。** schedule は自身の `enabled`
  （既定 0）で制御する。**運用者が `heartbeat_enabled=false` にしても定時実行は止まらない**
  （止めるには各 schedule の `enabled=false`）。heartbeat と schedule は別概念なので、G が
  両方を止めると名前が実体を裏切る（意図した分離）。詳細は `docs/design-agent-schedules.md`。

## Discord ゲートウェイの二重起動防止（DB 優先）

同一エージェントが「TOML の共有ゲートウェイ（`[gateway.discord].agent_ids`）」と
「DB の per-agent ゲートウェイ（`agent_discord_config` で enabled）」の両方に載ると、
1 つの発言に二重応答しうる。**専用（per-agent）ゲートウェイが優先**され、共有
ゲートウェイのループがメッセージ処理のたびに
`AgentRunner::served_by_dedicated_gateway`（= `DiscordGatewayManager::is_running`）
を確認して、専用ゲートウェイが**実際に稼働中**のエージェントをスキップする
（`crates/discord/src/message_loop.rs`）。

判定を DB の enabled フラグではなく**ゲートウェイの生死**で行うのが要点:

- **enable/disable が対称に効く**: 稼働中に per-agent 設定を enable → 専用ゲートウェイ
  が上がった時点で共有側が処理をやめる。disable / stop → 専用側が止まった時点で
  共有側が処理を再開する。どちらも再起動不要。
- **「誰も応答しない」状態を作らない**: enabled=1 でも専用ゲートウェイが起動失敗
  （無効トークン、Discord 障害）していれば、共有側がフォールバックとして応答を
  続ける。起動時に enabled な per-agent 設定を持つエージェントを共有リストから
  **除外しない**のも同じ理由（info ログのみ）。
- スキップ判定は agent ループの前にリストごと絞る形で行い、trust 判定
  （`dm_allowed_any` / `resolve_caller`）にもスキップ対象エージェントの
  trusted_users を混入させない。

per-agent ゲートウェイ側のループ（`manager.rs`）はこのチェックを行わない
（enabled な設定**から**起動されるため、チェックすると自分自身を skip してしまう）。

**対象外**: この dedupe は「同一エージェントの二重処理」を防ぐものであって、
別々の Bot トークンを持つ 2 つの bot が同じチャンネルに同居すること自体は制限しない
（それは正当な構成でありうる）。

**既知の縮退**: 専用ゲートウェイの起動直後・停止直前の短い窓では、共有側と専用側の
どちらが処理するかがメッセージ単位で切り替わる。Discord ゲートウェイは接続前の
メッセージを再配信しないため二重応答にはならないが、応答する bot アカウントが
一時的に揺れることはある。

## hot-reload と DB 設定が衝突しない理由

`crates/server/src/hot_reload.rs` は config.toml の変更を検知すると共有の
`ToolsConfig` を**丸ごと差し替える**。一見 DB 由来の許可コマンドが消えるように
見えるが、消えない: `agent_allowed_commands` は保存時に ToolsConfig へは書かれず、
**実行のたびに** DB から読んでエージェント専用のクローンへマージされる。合成関数は
`crates/server/src/process.rs` の `resolve_run_tools_config` 1 箇所だけで、run の冒頭で
必ず呼ばれる。共有 ToolsConfig は常に「TOML の姿」だけを持ち、DB のランタイム追加分は
per-run で合成される。

この不変条件（DB 由来を共有 ToolsConfig へ書かない）は、**許可コマンドの追加・削除
ツールにも適用される**。かつて Discord 実装は DB と共有 ToolsConfig の両方へ書いており、
それが「あるエージェントの許可が全エージェントへ漏れる」不具合（#202）だった。
#157 S1 の移設時に書き込みを撤去した。撤去しても呼び出し元が困らないのは、
上記の合成が毎 run 走るため**次の run で必ず効く**からである。なお**同一ターン内では
元から効かない**（`register_tools_from_config` が run 冒頭で `ShellToolConfig` を
clone してスナップショットする）。

## 新しい設定を足すときの指針

- プロセス全体の既定値・接続情報 → TOML。
- エージェント/チャンネル単位で実行中に変えたい値 → DB（+ 必要なら API/アクション）。
- 両方に載る値を作る場合は、**effective 値の合成関数を 1 箇所に置き**、
  読み手が生の TOML/DB を直接参照しないこと（前例: heartbeat の解決関数、
  allowed_commands の per-call マージ、共有ゲートウェイの agent skip）。
