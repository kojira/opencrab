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
  `cargo test -p opencrab-discord-gateway --features webhook-activity --lib webhook` で
  48 passed / 1 ignored）。
- `crates/nostr` は gateway リポジトリの `crates/nostr-gateway` が既に担っており、core 側の
  コピーは残す理由が無い（参照元 0 件）ため、移動せず削除した。

## 3. 残したもの（phase 2 で扱う）

- `crates/actions/src/webhook_target/*`（宛先解決・URL 検証・秘匿・整形）は **core に残す**。
  唯一の利用元が 6 つの webhook 管理ツール（`set_default_webhook` など）とその設定
  フォールバック（`AppState.default_subtask_webhook`）で、これらの削除は phase 2 の
  スラッシュコマンド一本化と同じ変更になる。phase 1 で先に動かすと、残す側の呼び出しが
  コンパイルできなくなる。
- `opencrab-actions::subtask_notify` と `ToolEventSink` は中立の抽象として core に残す。

## 4. ゲート（この記録の後に実行）

- `python3 scripts/gateway_boundary_audit.py`
- `bash scripts/check-deps.sh`
- `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`
