# opencrab-extgate

Core side of the gate socket protocol (protocol 3, operation protocol 1). The contract is
`docs/design/external-gate.md`; its D-1099 section is the current wire.

## 境界

- core が UDS（`gate.listen_socket`）を listen する。空なら listen しない（startup recover は実行する）。
  資格情報は無く、ソケットを開けることだけが条件（D-1098）。
- core はセッションだけを持つ（会話記録・記憶・ターン・ツール実行・定期実行・モデル設定、D-1099）。
  接続先・Bot・オーナーと権限・受付・表示・設定画面は gateway が持つ。
- gateway は `hello` で instance・担当エージェント・道具（operations）を名乗る。instance が無ければ core が作る。
- セッションキーは gateway が決める不透明な文字列。core は (instance, キー) を `gate_bindings` 行に解決する。
- 接続は process-local registry。startup は空。DB から復元しない。

## モジュール

| ファイル | 役割 |
|---|---|
| `protocol.rs` / `protocol/` | frame と message の読み書き |
| `json.rs` | 全階層 duplicate member 拒否 |
| `error.rs` | error code と wire 写像 |
| `ids.rs` | 識別子・digest・時刻 |
| `listen/` | UDS listen と接続状態機械（PRE_HELLO / RUNNING / CLOSED）。`hello`・`provisional`・`interrupt`・活動通知・応答 |
| `registry.rs` | live 接続の process-local 表 |
| `close.rs` | 接続 close。未決の say を indeterminate にする |
| `inbound/` | `said` の受け入れ。`session.rs` がセッションキー → セッション（D-1099）、`record.rs` が記録、`attachments.rs` が受信添付、`turn.rs` がターン起動 |
| `turn_queue.rs` | 記録後のセッションごとのターン待ち行列 |
| `completion.rs` | ターンとサブタスクの決着 |
| `delivery.rs` | ターン結果 → `say` 1 回 |
| `partial_stream.rs` | realtime ターンの途中経過（`partial`、D-1083） |
| `provisional.rs` / `provisional/` | 先行生成（`provisional`、D-1085） |
| `interrupt.rs` | 走っているターンを止める（`interrupt`、D-1096） |
| `activity_stream.rs` | ツール実行・サブタスクの活動通知（D-1087） |
| `commands.rs` | `command` frame（`/model` などの管理コマンド） |
| `operations.rs` | 道具の宣言の parse・検証・digest。schema の許可キーと core 用の印（`x-opencrab-short-ref` / `x-opencrab-workspace-file`、D-1134） |
| `ops_projection.rs` | 宣言された道具を `GatewayActions` としてエンジンに見せ、`invoke` で呼ぶ。短縮参照の解決 |
| `operation_calls.rs` | 道具の呼び出しの記録と決着 |
| `outbound_files.rs` / `file_copy.rs` / `secure_path.rs` | ワークスペースのファイルを outbox にコピーして渡す |
| `fire.rs` | 定期実行のターンをセッションに流す |
| `race.rs` | 試験用 barrier |

## 非目標

`docs/design/external-gate.md` §8。
