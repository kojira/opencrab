# Harness 棚卸し（モデルの弱さを補う足場の台帳）

> LOOPS.md 原則 VIII「harness を消せ」— モデルが賢くなるたびに、モデルの弱さを補うために
> 書いた足場は負債になる。この文書は「このコードはモデルの弱さ X を補うために存在する」という
> 前提を明文化し、消し時を判断可能にするための台帳である（Issue #51）。

## 棚卸しのタイミング

- 使用モデル / プロバイダを変更・追加・更新したとき
- `config/default.toml` の `[llm]` セクションを触るとき
- 四半期に一度（目安）

各エントリの「計測」を実行し、前提が消えていれば足場を削除する PR を立てる。

## 台帳

### 1. XML `<function_calls>` フォールバックパース

- **場所**: `crates/core/src/engine/xml_parser.rs`、発火箇所は `crates/core/src/engine/skill_engine.rs`（tool_calls が空で content に `<function_calls>` がある場合）
- **前提（補っている弱さ）**: native tool calling を返さず、XML をテキストで出力するモデルがある（当初 DeepSeek via OpenRouter）
- **計測**:
  ```sql
  SELECT created_at, agent_id, message FROM agent_logs
  WHERE context = 'harness.xml_fallback'
  ORDER BY created_at DESC LIMIT 20;
  ```
  発火時のモデル名が message に入る。`EngineResult.xml_fallback_parses` でも run 単位で取得可能。
- **削除条件**: 現行モデル構成で N 週間（目安4週）発火ゼロ、**かつ** codex プロバイダ（下記 2）を使っていないこと。codex はこのフォールバックに**意図的に依存**しているため、2 と運命共同体。

### 2. codex プロバイダのプロンプト擬似 tool calling

- **場所**: `crates/llm/src/providers/codex.rs`（`render_tool_definitions` — ツール定義を XML ブロックとしてプロンプトに注入し、`<function_calls>` を出力させる）
- **前提**: codex CLI に native tool calling が無い
- **計測**: **config を一次情報にする** — `config/default.toml` の `default_provider`、
  `[llm.providers.codex]` の有無、fallback chain / aliases に codex が含まれるか。
  注意: `llm_logs.model` には**ルーティング前のリクエスト文字列**がそのまま入る。
  bare なモデル名（例 `gpt-5.5`）や alias は default provider に解決されるため、
  `WHERE model LIKE 'codex:%'` の SQL では codex 経由の呼び出しを取りこぼす。
  SQL で見るなら発火側（エントリ 1 の `harness.xml_fallback`）を使う。
- **削除条件**: codex プロバイダを使わなくなった、または codex が native tool calling に対応した。削除時は 1 の削除可否も再評価する。

### 3. 旧形状 tool_calls JSON のパース互換

- **場所**: `crates/server/src/process.rs::format_single_log`、`crates/discord/src/gateway_actions/subtask_engine.rs::summarize_tool_calls`（正準形状 `{function:{name,arguments}}` と旧形状 `{name,arguments}` の両対応）
- **前提**: モデルの弱さではなく**過去データ互換**。#31 のメッセージモデル統一以前に session_logs へ保存された旧形状の行が残っている
- **計測**: metadata_json の `tool_calls_json` は**エスケープされた JSON 文字列**として
  格納される（正準行は `\"function\"` を含む）ため、パターンは引用符を含めない形にする:
  ```sql
  SELECT COUNT(*) FROM memory_sessions
  WHERE log_type = 'tool_call'
    AND metadata_json LIKE '%tool_calls_json%'
    AND metadata_json NOT LIKE '%function%';
  ```
  （arguments の中身に "function" という語が含まれる旧形状行は取りこぼすヒューリスティック。
  0 に近づいたら個別確認する）
- **削除条件**: 旧形状の行が実質参照されなくなった（保持期間経過 or 一括変換マイグレーション実施後）

### 4. LLM 応答の markdown フェンス除去

- **場所**: `crates/core/src/llm_text.rs::strip_code_fences`（memory_index / daily_log_indexer / evaluator が使用）
- **前提**: 「JSON のみで応答せよ」の指示に対し ```` ```json ```` フェンス付きで返すモデルが依然多い
- **計測**: 容易な自動計測なし。structured output / JSON mode をプロバイダ横断で使えるようになった時点で再評価
- **削除条件**: 全プロバイダで JSON mode を使う実装に置き換えたとき

### 5. プロバイダ固有ハック（cache_control 等）

- **場所**: `crates/core/src/engine/skill_engine.rs`（system prompt への 1h ephemeral cache_control）ほか provider 実装内の差異吸収
- **前提**: 弱さの補填ではなく**最適化**（プロンプトキャッシュ）。剪定対象ではないが、プロバイダ仕様変更時に見直す
- **削除条件**: 対象外（仕様変更追従のみ）

### 6. JSON パース失敗時のブラインドリトライ

- **場所**: `crates/core/src/memory/daily_log_indexer.rs::summarize_day_with_retry`
  （serde パース失敗は `NON_RETRYABLE_PATTERNS` に該当せず最大3回リトライされる）
- **前提**: 「JSON のみで応答せよ」の指示を無視するモデルがある（エントリ 4 と同根、
  ただしこちらは最大3倍の LLM コスト増を伴う）
- **計測**: 容易な自動計測なし。エントリ 4 と同時に再評価
- **削除条件**: structured output / JSON mode への置き換え時にリトライも撤去

## 記載ルール

- 新しく「モデルの弱さを補うコード」を書くときは、この台帳にエントリを追加し、
  コード側コメントに前提を書く（前提が書かれていない足場は消し時を判断できない）。
- エントリを削除したら、この台帳から該当行を消す PR を同時に出す。
