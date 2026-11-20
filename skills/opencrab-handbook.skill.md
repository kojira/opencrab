---
name: opencrab-handbook
description: "OpenCrab（あなたが動いている基盤）の機能・設定・権限モデルの手引き。設定変更や機能の使い方に迷ったら read_skill で開く"
version: 1
permission: agent
actions:
  - read_skill
  - get_system_info
---

# OpenCrab ハンドブック

あなたは **OpenCrab** というマルチエージェント基盤の上で動いています。このスキルは
OpenCrab 自体の全体像と、設定・機能をどう使う／変えるかの地図です。**具体的な値や
パラメータは変化しうる**ため、ここでは概念と「どこを見れば最新が分かるか」を示します。
細かい仕様は下記の「最新の一次情報の探し方」で都度確認してください。

## 1. OpenCrab とは

- 複数のエージェント（あなたを含む）を動かす Rust 製の基盤 + React ダッシュボード。
- 各エージェントは独立した人格・モデル・指示・鍵・記憶・スキルを持つ。
- 対話経路（ゲートウェイ）: Discord、Nostr、Web、Bluesky、CLI など。ゲートウェイは OpenCrab 本体とは
  別のプロセスで、Unix ソケットでつながる。ゲートウェイは自分の道具（例: 返信・リアクション）を宣言し、
  その会話のターンにだけそれがあなたの関数一覧に載る。
- あなたのターンは「ツール（function）を呼びながら考える」ループ。使えるツールの
  一覧は毎ターンの**関数定義一覧**に載っています（それが最新の権威）。

## 2. 権限モデル（重要）

呼び出し元は 4 種類。ツールの可視性・実行可否はこれで決まります。

- **owner**: 運営者本人。最も強い。設定変更系（例 `configure_llm_provider`、
  `update_instructions`）、`execute_shell`、ワークスペースのファイル操作は owner のみ可視・実行可能。
- **co_agent**: 連携エージェント。owner と同じ扱い。
- **trusted_user**: 信頼済みユーザー。中間的な権限（例: スキル作成、定時実行の登録、記憶の整理）。
- **agent**: 最小権限。外部ユーザー起点（Nostr/Discord の受信イベント等）のターンは
  この権限。owner 限定ツールは一覧にも出ず実行もされない（プロンプトインジェクション
  で乗っ取られないための対称ゲート）。

「このツールが見えない/拒否される」場合、まず自分の呼び出し元権限を疑ってください。

## 3. 設定の変え方（2 系統）

OpenCrab の設定はほぼすべて **(a) ダッシュボード** と **(b) エージェントのツール**の
両方から変更できます（UX 最大化の方針）。

- **LLM プロバイダ**: `configure_llm_provider`（owner 限定）で即時変更。DB
  オーバーライドに保存してルーターをホットスワップするため**再起動不要**。
  codex/cursor/acp などの subprocess プロバイダは適用後に起動確認し、失敗したら
  **自動的に直前の設定へロールバック**して、その旨を結果で知らせます。api_key だけは
  安全上このツールから変更不可（ダッシュボードで設定）。
- **自分の指示**: `update_instructions`（owner 限定）。
- **定期実行 / 定時実行**: `set_my_schedule` / `get_my_schedules` / `update_my_schedule` /
  `delete_my_schedule`（間隔は `@every 30m`、定時は cron。行ごとに message を持つ）。
  今すぐ試すなら `run_my_schedule`（owner / co_agent 限定）。
- **この会話のモデル**: `select_llm`（オーナーの `/model` と同じ。次のターンから効く）。
  今のモデルと選べる一覧は `get_system_info` で見る（`llm-selection` スキル参照）。
- **許可コマンド**: `add_allowed_command` / `remove_allowed_command` / `list_allowed_commands`。
- **MCP サーバ**: `configure_mcp_server`（owner 限定）。
- **Discord / Nostr / Bluesky などの設定（Bot、鍵、チャンネル、信頼ユーザー）**: それぞれの
  ゲートウェイが持つ。ゲートウェイの設定画面から変える。無いツールを探す前に、まず関数定義一覧に
  該当ツールがあるか確認してください。

## 4. LLM プロバイダの種類

- **API キー型**: openai / anthropic / google / openrouter / ollama / llamacpp /
  chatgpt など。base_url・api_key・default_model 等で設定。
- **subprocess 型（CLI を起動）**: codex / cursor / acp。`binary_path` / `args` /
  `working_dir` / `timeout_secs` を持ち、外部 CLI エージェントを子プロセスとして
  駆動する。起動可否は `health_check`（`--version` 等）で確認される。
- モデルは `provider:model` の形で選択。

## 5. 連携機能（高レベル）

- **ゲートウェイ（Discord / Nostr / Web / Bluesky / CLI）**: 別プロセス。使える道具（返信、
  リアクション、投稿など）と、その道具を誰が使えるかは各ゲートウェイが宣言する。その会話の
  関数定義一覧に載っているものが使える。
- **MCP**: エージェント毎に MCP サーバを接続し、`mcp__<server>__<tool>` として
  ツールを利用。`trusted_only` サーバは信頼された呼び出し元のターンでのみ露出。

## 6. スキルと記憶（段階的開示）

- **スキル**: あなたの手順書。システムプロンプトには **index（名前 + 説明）だけ**が
  載り、本文は `read_skill(name)` で必要時に取得します（このハンドブックもその一つ）。
  作成/引退/復活は `create_my_skill` / `retire_my_skill` / `restore_my_skill`。
- **記憶インデックス**: 長期記憶の索引。`browse_memory_index` /
  `search_memory_index` / `retrieve_memory_nodes` で辿ります（プロンプトには
  圧縮した索引のみ、本文はツールで取得＝スキルと同じ段階的開示）。

## 7. ワークスペース / シェル

- 各エージェントは作業ディレクトリを持ち、`ws_read` / `ws_write` / `ws_edit` /
  `ws_list` / `ws_delete` / `ws_mkdir` でファイル操作。
- `execute_shell` でコマンド実行（許可コマンドは設定でホワイトリスト管理、
  タイムアウトは呼び出し時に指定可）。

## 8. 最新の一次情報の探し方（drift 対策）

このハンドブックは概念地図です。**具体的な最新値**は次で確認してください。

1. **関数定義一覧**（毎ターン渡される）= 使えるツールとその引数の権威。
2. **`get_system_info`** = このターンのモデル（`current_turn_model`）、この会話のモデル
   （`conversation_model`）、選べるモデル（`selectable_models`）、利用可能プロバイダ等の実状態。
3. **他のスキル** = `read_skill(name)` で個別手順を取得。
4. **ダッシュボード**（運営者向け）= システム設定 UI に全設定項目。

迷ったら「まず関数一覧と get_system_info を見る → 該当スキルを read_skill で開く」の
順で掘り下げてください。
