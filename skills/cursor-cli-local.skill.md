---
name: cursor-cli-local
description: Cursor CLI（コマンド名 `agent` / `cursor-agent`）をローカルで実務利用するときの実用オプション集。起動方法・モデル指定・承認モード・非対話実行・出力フォーマット・セッション再開・worktree隔離を扱う。指定された実行手段を勝手にすり替えない。指定手段が失敗・タイムアウトしたらfallbackせず停止して判断を仰ぐ。
version: 1
permission: agent
actions:
  - execute_shell
---

# Cursor CLI（`agent`）実用ガイド

## コマンド名

正式なコマンド名は **`agent`**（`agent --help` の Usage も `agent`）。

```bash
agent --version    # 2026.07.23-e383d2b
```

`cursor-agent` も同じバイナリへの symlink なので、どちらでも動く。

```
~/.local/bin/agent        -> ~/.local/share/cursor-agent/versions/<version>/cursor-agent
~/.local/bin/cursor-agent -> 同上
```

opencrab 本体（`llm.providers.cursor`）はインストーラが必ず作る安定名 `cursor-agent` を既定にしている。
環境で名前が違う場合は config の `binary_path` で `agent` / `cursor` に切り替える。

## opencrab から見た位置づけ

opencrab の cursor プロバイダは、この CLI を headless で呼んで LLM として使っている。

```bash
cursor-agent -p --output-format json -m <model> --force <prompt>
```

- 認証は `CURSOR_API_KEY`、または `cursor-agent login` 済みのアンビエント認証
- 選べるモデルは `agent models` で変わるので、config の `models` を実機に合わせる
- **プロンプトは positional で渡す**（`-p` で positional 無しだと入力終端を待ってハングする既知の不具合がある）

自分で `execute_shell` から叩くときも、この形を基準にする。

## 結論

自動化・サブエージェントから呼ぶなら、これを基本形にする。

```bash
agent -p "ここに指示" --model gpt-5.2 --output-format text --force
```

理由:
- `-p`（--print）は非対話で完走し、write / shell を含む全ツールが使える
- インタラクティブTUIをバックグラウンド起動するとハングしやすい（Claude Codeと違い `-p` が安定して動く）
- 承認待ちが入るとテンポが死ぬので `--force`

つまり **普段使いは `-p` + `--force`** が基本。危険なのを理解した上で速度を取る。
調査だけなら `--plan`（読み取り専用）に落とす。

## まず押さえるオプション

### `-p, --print`
非対話実行。結果を標準出力に出して終了する。

```bash
agent -p "このリポジトリのテスト失敗原因を調べて"
```

- スクリプト・サブエージェントからの呼び出しはこれ一択
- write / shell も使えるので、修正作業まで任せられる

### `--model`
使うモデルを明示する。

```bash
agent -p "..." --model gpt-5.2
agent -p "..." --model claude-opus-5-thinking-high
```

- 一覧は `agent --list-models`（または `agent models`）
- `auto` がデフォルト。意図した性能で回したいなら毎回付ける
- `-fast` 付きは高速枠、`-high` / `-xhigh` は思考量が多い

### 承認モード
危険操作の扱いを決める。

```bash
agent -p "..." --force          # 明示的に禁止されてない限り全部許可（--yolo と同じ）
agent -p "..." --auto-review    # サーバ側分類器が安全な操作だけ自動実行
```

使い分け:
- `--force` / `--yolo`: 実務の基本。止まらず進めたいとき
- `--auto-review`: 少し安全寄り。自動化と慎重さの中間
- 無指定: 承認プロンプトが出る余地があるので自動化には向かない

### `--mode` / `--plan`
実行モードを制限する。**フォールバックではなく明示的な読み取り専用指定**。

```bash
agent -p "..." --plan --trust        # = --mode plan（分析と計画のみ、編集なし）
agent -p "..." --mode ask --trust    # Q&A向け（読み取り専用）
```

- 原因調査・コードリーディング・影響範囲確認はこれ
- 「まだ直すな」を確実に守らせたいときに使う
- **`--trust` が必要**: 未信頼ディレクトリでは `--force` を付けない起動が信頼確認で止まる（実測）。`--plan` 系は `--trust` を併記する

### `--sandbox`
設定を上書きしてサンドボックスを明示する。

```bash
agent -p "..." --sandbox enabled
agent -p "..." --sandbox disabled
```

### 作業対象の指定

```bash
agent -p "..." --workspace /path/to/project   # 対象ワークスペース
agent -p "..." --add-dir /path/to/other       # 参照させたい別ルートを追加
agent -p "..." --trust                        # 信頼確認をスキップ（初回のディレクトリで必須）
```

- 未指定なら cwd がワークスペース
- 自動化では `--workspace` を明示したほうが事故りにくい
- `--force` は信頼確認も兼ねる。`--force` を使わない場合は `--trust` が必要

### `-w, --worktree`
`~/.cursor/worktrees/<repo>/<name>` に隔離した git worktree を作って作業させる。

```bash
agent -p "..." -w my-experiment
agent -p "..." -w my-experiment --worktree-base main
```

- 並列で試したいとき、本体のワークツリーを汚したくないときに使う

## 出力フォーマット

`--output-format` は `-p` と併用時のみ有効。

```bash
agent -p "..." --output-format text          # 人が読む用（デフォルト）
agent -p "..." --output-format json          # 自動化用。1行のJSON
agent -p "..." --output-format stream-json   # 逐次イベント
agent -p "..." --output-format stream-json --stream-partial-output  # トークン単位で流す
```

`json` の中身（実測）:

```json
{"type":"result","subtype":"success","is_error":false,"duration_ms":3319,
 "result":"OK","session_id":"4d779cad-...","request_id":"b276f8ef-...",
 "usage":{"inputTokens":18435,"outputTokens":5,"cacheReadTokens":3456,"cacheWriteTokens":0}}
```

- `result` が本文、`is_error` で成否判定、`session_id` は再開に使う
- スクリプトから使うなら `json` にして `session_id` を必ず保存する

## 長い指示は stdin で渡す

```bash
agent -p --model gpt-5.2 < task.txt
cat task.txt | agent -p --model gpt-5.2
```

- シェルのクォート事故がなくなる
- **EOF が確実に来る形（パイプ / リダイレクト）で渡すこと。** positional も stdin も与えないと入力終端を待ってハングする
- `task.txt` はこの形にしておくと安定する

```text
目的:
対象範囲:
完了条件:
禁止事項:
報告形式:
```

## セッション再開

```bash
agent --resume "<session_id>" -p "続きの指示"   # ID指定で再開
agent --continue -p "続きの指示"                # 直前セッションを継続
agent --resume                                  # 対話でセッションを選ぶ
```

多段タスクの回し方（実測で動作確認済み）:

```bash
SID=$(agent -p "まず調査だけして。修正はしない" --model gpt-5.2 --output-format json \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["session_id"])')

agent --resume "$SID" -p "その調査結果に基づいて最小修正を実施して" \
  --model gpt-5.2 --force --output-format text
```

## 認証

```bash
agent status      # ログイン状態確認（whoami でも同じ）
agent login       # ブラウザ認証。NO_OPEN_BROWSER=1 でブラウザを開かない
agent about       # バージョン・システム・アカウント情報
```

- APIキーで回すなら `CURSOR_API_KEY` 環境変数、または `--api-key`

## 実務でよく使う組み合わせ

### フルパワー実行

```bash
agent -p "この不具合を直して。最後に変更ファイル、実行コマンド、テスト結果をまとめて" \
  --model gpt-5.2 --force --workspace /path/to/project --output-format text
```

### 最小修正モード

```bash
agent -p "対象は src/foo.ts のみ。最小修正だけ。目的外変更禁止。最後に diff を要約して" \
  --model gpt-5.2 --force
```

### 調査専用モード

```bash
agent -p "原因特定だけして。まだ修正しないで。根拠になったファイル名も書いて" \
  --model gpt-5.2 --plan --trust
```

### 隔離して試す

```bash
agent -p < task.txt --model gpt-5.2 --force -w trial-a --worktree-base main
```

## よくある失敗

- **インタラクティブTUIをバックグラウンド起動してハングする** → `-p` を使う
- **モデルを明示せず `auto` で意図しない性能になる** → `--model` を付ける
- **承認プロンプトで止まる** → `--force`（慎重にやるなら `--auto-review`）
- **長い指示をシェルで壊す** → `task.txt` を作って `agent -p < task.txt`
- **`session_id` を捨てて続きができない** → `--output-format json` で拾って保存
- **意図せず広く触られる** → `--plan` か、プロンプト側で対象範囲を固定する
- **`--output-format` が効かない** → `-p` と併用していない
- **`Pass --trust, --yolo, or -f if you trust this directory` で止まる** → 未信頼ディレクトリ。`--trust` を足す

## 最小チートシート

### 基本

```bash
agent -p "指示" --model gpt-5.2 --force
```

### 調査専用

```bash
agent -p "指示" --model gpt-5.2 --plan --trust
```

### 自動化（結果をJSONで受ける）

```bash
agent -p "指示" --model gpt-5.2 --force --output-format json
```

### 指示ファイルを使う

```bash
agent -p --model gpt-5.2 --force < task.txt
```

### 続きから

```bash
agent --resume "<session_id>" -p "続きの指示" --model gpt-5.2 --force
```

### モデル一覧

```bash
agent --list-models
```
