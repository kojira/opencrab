---
name: cursor-cli-local
description: Cursor CLI（コマンド名 `agent`）を execute_shell から使うための手順。プロンプトは positional で渡す、timeout_secs を明示する、シェル機能（パイプ・リダイレクト）は使えない、という3点が守れないと必ず失敗する。指定された実行手段を勝手にすり替えない。失敗・タイムアウトしたらfallbackせず停止して判断を仰ぐ。
version: 1
permission: agent
actions:
  - execute_shell
---

# Cursor CLI（`agent`）を execute_shell から使う

## 前提（ここを外すと必ず失敗する）

1. **`execute_shell` はシェルを介さずバイナリを直接起動する。** `|` `>` `<` `&&` `$(...)` は一切使えない。
   引数は `args` 配列で、1要素 = 1引数として渡す。
2. **プロンプトは positional 引数で渡す。** positional も `stdin` も与えないと、Cursor CLI は入力終端を
   待ってハングし、タイムアウトまで枠を占有する。
3. **`timeout_secs` を毎回明示する。** 省略時は 30 秒で、Cursor CLI の応答には短すぎる（上限 1800）。
4. **コマンド名は完全一致。** 自分に付与された名前だけが通る（既定は `agent`）。`cursor-agent` は
   同じバイナリの別名だが、許可されていなければ弾かれる。

## 基本形

```json
{
  "command": "agent",
  "args": ["-p", "ここに指示", "--model", "cursor-grok-4.6-high", "--plan", "--trust",
           "--output-format", "json"],
  "timeout_secs": 600
}
```

まずこの読み取り専用（`--plan`）を基本にする。編集させる必要が出てから権限を上げる。

## 編集させる場合の注意

`--force`（別名 `--yolo`）を付けると承認を待たずに進むが、**起動される Cursor CLI 自身が write と
shell を持つ**。つまり自分の許可コマンド一覧の外にあることまで実行できてしまう。加えて子プロセスは
親の環境変数（トークンや API キー）を引き継ぐ。

したがって:

- `--force` はオーナーから編集を指示された時だけ使う
- プロンプト側で対象範囲（触っていいファイル）を必ず固定する
- 少し安全寄りにしたい時は `--auto-review`（安全な操作だけ自動実行）

```json
{
  "command": "agent",
  "args": ["-p", "対象は src/foo.rs のみ。最小修正だけ。目的外変更禁止。最後に diff を要約して",
           "--model", "cursor-grok-4.6-high", "--force", "--output-format", "json"],
  "timeout_secs": 900
}
```

## 主要オプション

| オプション | 意味 |
|---|---|
| `-p` | 非対話実行。これが無いと TUI が起動して使えない |
| `--model <id>` | 使うモデル。一覧は `args: ["--list-models"]` で取得 |
| `--plan` / `--mode ask` | 読み取り専用（分析・計画のみ）。編集させない時はこれ |
| `--trust` | 信頼確認をスキップ。`--force` を付けない起動では必須 |
| `--force` / `--yolo` | 承認なしで全許可。上記の注意を読んでから使う |
| `--auto-review` | 安全な操作だけ自動実行する中間モード |
| `--sandbox enabled\|disabled` | サンドボックスの明示指定 |
| `--output-format text\|json\|stream-json` | `-p` 併用時のみ有効 |
| `--resume <session_id>` / `--continue` | セッション再開 |

`--workspace <path>` と `--add-dir <path>` は作業対象を自分のワークスペース外へ広げる。
**オーナーから対象を明示された時以外は使わない。** 未指定なら `execute_shell` の作業ディレクトリ
（自分のワークスペース）が対象になる。

## 出力（`--output-format json`）

1行の JSON が返る。

```json
{"type":"result","subtype":"success","is_error":false,"duration_ms":3319,
 "result":"OK","session_id":"4d779cad-...","request_id":"b276f8ef-...",
 "usage":{"inputTokens":18435,"outputTokens":5,"cacheReadTokens":3456,"cacheWriteTokens":0}}
```

- `result` が本文、`is_error` で成否を判定する
- `session_id` は続きの指示に使うので、多段でやるなら必ず控える

## 続きの指示（セッション再開）

```json
{
  "command": "agent",
  "args": ["--resume", "<前回の session_id>", "-p", "その調査結果に基づいて最小修正を実施して",
           "--model", "cursor-grok-4.6-high", "--force", "--output-format", "json"],
  "timeout_secs": 900
}
```

「まず `--plan` で調査 → 結果を確認 → 同じセッションで修正」が安全な進め方。

## 長い指示を渡す

シェルのリダイレクトは使えないので、次のどちらかにする。

- **positional に全文を入れる**（推奨。改行入りでもそのまま1要素で渡せる）
- `execute_shell` の `stdin` フィールドに本文を入れる（この場合も `-p` は必要）

指示の型はこれで固定すると安定する。

```text
目的:
対象範囲:
完了条件:
禁止事項:
報告形式:
```

## よくある失敗

- **応答が返らずタイムアウトする** → positional プロンプトを渡していない。`args` に指示本文を入れる
- **30秒で切られる** → `timeout_secs` を明示していない（600〜900 を目安に）
- **`Command 'cursor-agent' is not in the allowed list`** → 付与された名前（既定は `agent`）を使う
- **`Pass --trust, --yolo, or -f if you trust this directory`** → `--plan` 等では `--trust` を併記する
- **`--output-format` が効かない** → `-p` と併用していない
- **パイプやリダイレクトを書いて動かない** → `execute_shell` はシェルを通さない。`stdin` を使う
- **意図せず広く触られる** → `--plan` にする、またはプロンプトで対象範囲を固定する

## opencrab 本体との関係（参考）

opencrab の cursor プロバイダは、この CLI を LLM として subprocess で呼んでいる。

```bash
cursor-agent -p --output-format json -m <model> --force <prompt>
```

- これは `execute_shell` とは別経路なので、許可コマンド一覧の影響を受けない
- プロバイダ既定のバイナリ名はインストーラが必ず作る `cursor-agent`。環境で名前が違う場合は
  config の `binary_path` で `agent` / `cursor` に切り替える
- `agent` と `cursor-agent` は同じバイナリへの symlink（正式名は `agent`、Usage 表記も `agent`）
- 認証は `CURSOR_API_KEY` か `agent login` 済みのアンビエント認証

## ターミナルから直接使う場合（人間向け・参考）

シェル経由ならリダイレクトとパイプが使える。**`execute_shell` からは使えない**ので混同しないこと。

```bash
agent -p "指示" --model cursor-grok-4.6-high --force            # 基本形
agent -p "指示" --model cursor-grok-4.6-high --plan --trust     # 調査専用
agent -p --model cursor-grok-4.6-high --force < task.txt        # 長い指示（EOF が来る形で渡す）
agent --resume "<session_id>" -p "続きの指示"      # 続きから
agent --list-models                                # モデル一覧
```
