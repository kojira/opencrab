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
- 対話経路（ゲートウェイ）: ダッシュボード（REST）、Discord、Nostr など。
- あなたのターンは「ツール（function）を呼びながら考える」ループ。使えるツールの
  一覧は毎ターンの**関数定義一覧**に載っています（それが最新の権威）。

## 2. 権限モデル（重要）

呼び出し元は 4 種類。ツールの可視性・実行可否はこれで決まります。

- **owner**: 運営者本人。最も強い。設定変更系（例 `configure_llm_provider`、
  `update_instructions`）は owner のみ可視・実行可能。
- **trusted_user / co_agent**: 信頼済みユーザー / 連携エージェント。中間的な権限
  （例: スキル作成、一部の Nostr 送信）。
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
- **自分の指示 / heartbeat 指示**: `update_instructions` /
  `update_heartbeat_instructions`（owner 限定）。
- **その他（Nostr / Discord / MCP / Voice / 許可コマンド / チャンネル設定 等）**:
  現状は主にダッシュボードから。エージェントツール化は順次拡張中。無いツールを
  探す前に、まず関数定義一覧に該当ツールがあるか確認してください。

## 4. LLM プロバイダの種類

- **API キー型**: openai / anthropic / google / openrouter / ollama / llamacpp /
  chatgpt など。base_url・api_key・default_model 等で設定。
- **subprocess 型（CLI を起動）**: codex / cursor / acp。`binary_path` / `args` /
  `working_dir` / `timeout_secs` を持ち、外部 CLI エージェントを子プロセスとして
  駆動する。起動可否は `health_check`（`--version` 等）で確認される。
- モデルは `provider:model` の形で選択。

## 5. 連携機能（高レベル）

- **Nostr**: エージェント毎に鍵を持てる（vanity 生成可、秘密鍵は LLM に渡らない）。
  投稿/返信、DM・zap、生成鍵からのマルチ identity 投稿、本鍵の
  切替（trusted 限定）。リレーは設定可能。
- **Discord**: エージェント毎の bot 設定。テキストに加え VC 音声対話（STT/TTS、
  話者分離）に対応。
- **MCP**: エージェント毎に MCP サーバを接続し、`mcp__<server>__<tool>` として
  ツールを利用。`trusted_only` サーバは信頼された呼び出し元のターンでのみ露出。
- **Voice**: STT/TTS プロバイダ設定。

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
2. **`get_system_info`** = 実行環境・既定モデル・利用可能プロバイダ等の実状態。
3. **他のスキル** = `read_skill(name)` で個別手順を取得。
4. **ダッシュボード**（運営者向け）= システム設定 UI に全設定項目。

迷ったら「まず関数一覧と get_system_info を見る → 該当スキルを read_skill で開く」の
順で掘り下げてください。
