# D-1087 phase 1: activity webhook を gateway リポジトリへ移す（crate 削除の実測記録）

webhook は gateway の概念なので、実装・宛先解決・配送は gateway が持つ。core は中立の
ツールイベント口（`opencrab_actions::ToolEventSink`）と、ターン・ツール・発話の記録だけを持つ。
この記録は、core から concrete な gateway crate を 2 つ削除する**前**に、linked edge を
実測して分類した証拠である（V15: dead-or-legacy-pending-reachability の更新根拠）。

## 1. 削除前の linked edge（実測コマンドと結果）

測った対象: `crates/discord`（package `opencrab-discord`）と `crates/nostr`（`opencrab-nostr`）。
基準 commit: D-1085 §13.6 反映後の `0faf9d36`。

| 検査 | コマンド | 結果 |
|---|---|---|
| workspace の宣言 | `git grep -nE "^\s*\"crates/(discord\|nostr)\"" -- Cargo.toml` | `Cargo.toml:10`, `Cargo.toml:11`（members） |
| workspace 依存の宣言 | `git grep -nE "^opencrab-(discord\|nostr) = " -- Cargo.toml` | `Cargo.toml:94`, `Cargo.toml:95` |
| 依存している crate | `git grep -n "opencrab-discord\|opencrab-nostr" -- 'crates/*/Cargo.toml'` | 自分の manifest 以外に **0 件** |
| Rust からの参照 | `git grep -nE "opencrab_discord\|opencrab_nostr" -- '*.rs'` | 自分の crate 以外は **doc コメント 3 件のみ**（`crates/actions/src/transcript.rs:83`, `:98`, `crates/server/src/transcript.rs:17`。いずれも「旧 `opencrab_discord::...`」という履歴注記で、コード参照ではない） |
| Cargo.lock | `grep -n 'name = "opencrab-discord"\|name = "opencrab-nostr"' Cargo.lock` | 2 つの package 定義のみ（依存元の package エントリ無し） |
| テスト実行 | `cargo test -p opencrab-discord` | 49 tests: 48 passed / 1 ignored（移設先の gateway でも同じ 48 passed / 1 ignored） |
| 実行時配線 | `git grep -n "DiscordWebhookNotifier\|subtask_lifecycle_notifier" -- crates` | `AppState.subtask_lifecycle_notifier` へ差し込む呼び出しは **0 件**（`d16eddfb` で削除され、`NoopLifecycleNotifier` に落ちる） |

`opencrab-discord` を参照する crate: **無し**。`opencrab-nostr` を参照する crate: **無し**。
したがって両 crate は、リポジトリ内に linked edge を持たない concrete gateway 実装である。

## 2. 削除後の行き先

- `crates/discord/src/gateway_actions/*`（整形・配送 worker・走行通知・activity sink）は
  gateway リポジトリの `crates/discord-gateway/src/webhook/*` へ移した（同じテストを移設。
  `cargo test -p opencrab-discord-gateway --lib webhook` で 48 passed / 1 ignored。
  feature gate は無い）。
- `crates/nostr` は gateway リポジトリの `crates/nostr-gateway` が既に担っており、core 側の
  コピーは残す理由が無い（参照元 0 件）ため、移動せず削除した。

## 3. phase 2 で消したもの（この節は削除後の状態を記す）

phase 2 で core から撤去した（gateway 側が持つ）:

- `crates/actions/src/webhook_target/*`（宛先解決・URL 検証・秘匿・整形、1581 行）と
  `crates/actions/src/lib.rs` の re-export。
- 6 つの webhook 管理ツール（`get_default_webhook` / `set_default_webhook` / `list_webhooks` /
  `get_default_subtask_webhook` / `set_default_subtask_webhook` / `list_subtask_webhooks`）と、
  その定義・dispatch・権限表・More tools 分類・spawn_subtask の `webhook` 引数。
- `AppState.default_subtask_webhook` と `config::AppConfig::default_subtask_webhook`、
  `SubtaskConfig.default_webhook`（`[subtask] default_webhook` の TOML 設定）。
- webhook のためだけに存在した通知配線: `AppState.subtask_notifiers` /
  `AppState.subtask_lifecycle_notifier`、`RunRequest::run_notifier`、
  `process::callbacks::set_run_notifier_callbacks` / `tool_result_progress_line`、
  spawn / cancel / report_progress の通知呼び出し。
- core の tool 結果と親ログから `webhook_source` / `webhook_status` / `webhook_redacted_url` /
  `webhook_error` を外した（宛先メタは gateway のもの）。
- 権限拒否の tracing target `webhook_audit` → `action_rejection_audit`。

core に残した中立の抽象は `ToolEventSink`（ツール実行 1 件）と
`SubtaskCompletionSink::on_subtask_lifecycle`（走行 1 件のライフサイクル）だけで、どちらも
core 自身が呼ぶ観測点ある。`SubtaskCompletionSink` は決着を親会話へ戻す core の責務のまま。

`subtask_notify`（`SubtaskLifecycleNotifier` / `SubtaskRunNotifier` / `SubtaskRunInfo` /
`SubtaskNotifySession` / `NotifyTarget` / `NotifyTargetError` / Noop 実装）は**削除した**。
core から呼ばれず、gateway からの参照も 0 件だった一方で、宛先の語彙
（`redacted_url` / `tool_args` / `SubtaskNotifiers`）を core に残していたため。
core は活動を frame（`tool_activity` / `subtask_lifecycle`）として流すだけで、
宛先・整形・配送は gateway が持つ。

残る webhook 語彙（この変更の範囲外・要フォロー）:
- `crates/db/src/queries/webhook_config.rs` と `agent_webhook_configs` の DDL
  （`schema/sql/full.rs`, `schema/migrations/v32_v36.rs`）。gateway の宛先行の保存先で、
  gateway の解決コードが `opencrab_db` 経由で参照している。gateway 自身の DB へ移すのは
  gateway 側フェーズ。
- 外部イベント**受信**（`POST /api/hooks/{source}`、`intake` / `api/hooks.rs` の
  `webhook_dedup_key`）。配送ではなく受信の概念で、core の HTTP API 面。
- テストの fixture 名（`webhook-user` 等）と、語彙混入を検査する static audit の denylist。

## 4. ゲート（この記録の後に実行）

- `python3 scripts/gateway_boundary_audit.py`
- `bash scripts/check-deps.sh`
- `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`

## 5. 追記: サブタスクの中の活動（depth ≥ 1）

移設直後は、観測に載るのが**親ターン**のツール実行だけで、サブタスクの中で起きたことが
見えなかった。原因は 2 つで、どちらも core 側の欠落だった。

- **自動 dispatch の背景バッチ**（`SubtaskToolDispatcher`）は走行を作りながら
  `on_subtask_lifecycle` を 1 度も呼んでおらず、中のツール実行は親ターンの文脈
  （depth 0・親セッション）で観測されていた。
- `tool_activity` frame に run の相関キーが無く、`spawn_subtask` の副実行（こちらは
  sink が子へ引き継がれている）のツール実行も、どの走行のものか突き合わせられなかった。

対応（core・`eccb9590`）:

- `ActionExecutor::execute_in_run(ExecutorRun { session_id, depth }, …)` を既定実装付きで
  追加。`BridgedExecutor` だけが override し、**観測イベント**の `session_id` / `depth` を
  走行の値にする。`tool_logs` の所属は従来どおり（記録は移さない）。
- `SubtaskToolDispatcher` が `started`（走行を許す前）と終端（`settle_completed` の前）を
  報告し、中のツールを `execute_in_run`（run key = `subtask-<uuid>`、depth = 親 + 1）で走らせる。
- `tool_activity` frame に任意 `sub_session_id`（`depth > 0` のときだけ）を追加。
  `subtask_lifecycle` の同名 field と同じ値で、gateway が両者を突き合わせられる。

gateway（`b28cf48`）は受け取った `sub_session_id` を `run: \`…\`` として行に出す
（親ターンの文面はバイト単位で不変）。知らない gateway は field を無視してよい。
