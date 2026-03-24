# design-query-subtask.md

## 概要

メインエージェントがサブタスク実行中に能動的に状況確認できる `query_subtask` ツールを追加する。

## 設計方針

- `query_subtask(subtask_id)` ツール → サブのsession_logの直近N件をそのままテキストで返す
- LLM呼び出しなし（メインのLLMもサブのLLMも呼ばない）
- サブが実行中（ツール待ち・ブロック中）でも関係なく取得できる
- メインのLLMがツールの戻り値を読んで状況を判断する

## 実装詳細

### ツール定義

```
query_subtask(subtask_id: String, limit: Option<usize>) -> String
```

### 処理フロー

1. `subtask_id` からDBのsubtask_spawned logで対応する `session_id` を取得
2. `session_logs` テーブルから `session_id` の直近N件（デフォルト: 20件）を取得
3. 各ログにtimestamp（`created_at`）を含めてフォーマット
4. フォーマット例:
   ```
   [2026-03-24T10:00:00Z] [tool_call] execute_shell: curl wttr.in/Hokkaido
   [2026-03-24T10:00:01Z] [tool_result] hokkaido: 🌧 +3°C
   [2026-03-24T10:00:02Z] [assistant] 調べてみる。取れた。
   ```
5. テキストとしてツールの戻り値に返す

### 変更箇所

- `crates/actions/src/tools/` に `query_subtask.rs` 追加
- `crates/server/src/db/queries.rs` に `get_subtask_session_logs(subtask_id, limit)` 追加
- `crates/server/src/api/` の allowed_commands にデフォルト追加

## タイムスタンプ

`session_logs.created_at` カラムを使用（ISO 8601形式で出力）

## 制限事項

- サブタスクが存在しない場合は `"subtask not found"` を返す
- session_logが空の場合は `"no logs yet"` を返す

## 期待動作

メインが `query_subtask("abc123")` を呼ぶと：

```
[10:00:00Z] [tool_call] execute_shell: apt install...
[10:00:05Z] [tool_result] (apt output 300 chars)
[10:00:06Z] [assistant] インストール中。次は設定ファイルを...
[10:00:07Z] [tool_call] execute_shell: vim /etc/...
```

が返ってきて、メインLLMが「Step 2まで完了、設定ファイル編集中」と判断できる。
