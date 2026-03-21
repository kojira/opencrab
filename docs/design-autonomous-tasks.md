# 自律的複合タスク実行の設計（改訂版）

**改訂日:** 2026-03-22
**対象:** opencrab コードベース
**目的:** OpenClawのアーキテクチャを参照した最小実装アプローチへの設計簡略化

---

## TL;DR

opencrabは既に「LLMがtoolを選んで順次実行するループ」を実装済み。
不足しているのは道具3つだけ。Phase 3（Multi-step Skill Engine）は不要。

---

## 現状把握：opencrabはほぼ完成している

### OpenClawとの比較

OpenClawはのすたろう（AIエージェント）が動くNode.js製フレームワーク。
「LLMがtoolsを受け取り、tool callを返し、結果を受けてまた考える」ループで複合タスクを実現している。

opencrabのアーキテクチャは本質的に同じ：

```
ユーザー発話
  → SkillEngine（max 20 iterations ループ）
    → LlmClient.chat()（tool_definitions付きでLLM呼び出し）
    → LLMが tool_calls を返す
    → ActionDispatcher（tool callを実行）
      → execute_shell, ws_read, create_my_skill 等
    → ActionResult をLLMにフィードバック
    → LLMが次のtool callまたは最終テキストを返す
    → 繰り返し
```

### 既に実装済みのもの

| コンポーネント | 状態 | 場所 |
|---|---|---|
| `SkillEngine` ループ | ✅ 実装済み | `crates/core/src/engine.rs` |
| `ActionDispatcher` | ✅ 実装済み | `crates/actions/src/dispatcher.rs` |
| `execute_shell` | ✅ 実装済み | `crates/actions/src/tools/shell.rs` |
| `execute_skill` | ✅ 実装済み | `crates/discord/src/gateway_actions.rs` |
| `add_allowed_command` | ✅ 実装済み | `crates/discord/src/gateway_actions.rs` |
| `ws_read/write/edit` | ✅ 実装済み | `crates/actions/src/workspace.rs` |

### 実際に不足しているもの（3つだけ）

1. **Bootstrap allowed_commands** — 初期状態でallowed_commandsが空。curl, jq等が最初から使えない
2. **fetch_web アクション** — HTMLをMarkdownに変換してLLMに渡すアクション（現状はcurlの生出力のみ）
3. **システムプロンプトの改善** — 「複数ステップを計画してよい」という明示がない

---

## 実装計画（Phase 1 + Phase 2のみ）

### Phase 1: Bootstrap と プロンプト改善（1〜2日、最優先）

**目標:** 「まず使える状態」にする

#### 1-1. Bootstrap allowed_commands

`config/default.toml` または初期化処理に以下を追加：

```toml
[tools]
enabled = true

[tools.shell]
enabled = true

[[tools.shell.commands]]
name = "curl"
permission = "agent"
description = "HTTP/HTTPS fetch"

[[tools.shell.commands]]
name = "jq"
permission = "agent"
description = "JSON解析・整形"

[[tools.shell.commands]]
name = "echo"
permission = "agent"

[[tools.shell.commands]]
name = "which"
permission = "agent"
description = "コマンドの場所確認"

[[tools.shell.commands]]
name = "date"
permission = "agent"

[[tools.shell.commands]]
name = "grep"
permission = "agent"

[[tools.shell.commands]]
name = "python3"
permission = "agent"
```

#### 1-2. システムプロンプトにマルチステップ計画の指示追加

`crates/server/src/process.rs` の `build_agent_context()` を修正：

```
あなたは複数のアクションを順番に計画・実行できます。
例えば「Xを調べてYを設定する」という指示に対して：
1. execute_shell で情報収集
2. 結果を解析
3. add_allowed_command でコマンド追加
4. create_my_skill でスキル作成
のように、複数のアクションを連続して呼び出してください。
```

### Phase 2: fetch_web アクション（1週間）

**目標:** LLMがWebコンテンツを理解できるようにする

#### 2-1. fetch_web アクション

新しいアクション `FetchWebAction` を `crates/actions/src/` に追加：

```rust
// パラメータ:
// - url: String（必須）
// - extract_mode: "markdown" | "text"（デフォルト: "markdown"）
// - max_chars: usize（デフォルト: 8000）
```

**処理フロー:**
1. `curl` でHTMLを取得（`execute_shell` 経由またはreqwest）
2. HTMLをMarkdownに変換（`html2text` クレートや `pandoc`）
3. `max_chars` に切り詰めて返す

#### 2-2. search_web アクション（オプション）

外部検索API（Brave Search等）を使った検索。
未設定時はDuckDuckGo HTMLへのcurlにフォールバック。

---

## 削除・後回しにする設計

### ~~Phase 3: Multi-step Skill Engine（DAG）~~

**削除理由:**

SkillEngineのLLMループが既にDAGと同等の役割を果たしている。
OpenClawも同じアプローチで複合タスクを実現しており、明示的なDAG管理は不要。

LLMが計画・実行・修正を自律的に行うため、`TaskPlan`/`SubTask`/`dependencies` のような
構造体を作る必要はない。Phase 1+2が完成してから実際の限界が見えたら検討する。

---

## 理想フロー（Phase 1+2完成後）

```
[かいろへの指示]
「Claude Codeの使い方を調べて、claudeコマンドを使えるようにして」

[かいろの計画（LLMが自律立案）]
1. execute_shell(curl, ["https://docs.anthropic.com/claude-code"]) → ドキュメント取得
   → または fetch_web("https://docs.anthropic.com/claude-code") → Markdown取得
2. add_allowed_command("claude") → コマンド追加
3. create_my_skill({name: "Claude Code実行", code: "claude", skill_type: "executable"})
4. send_speech("claudeコマンドを追加しました！`claude`で起動できます")

[実行結果]
✅ 完了（各アクション結果がLLMにフィードバックされながら進む）
```

---

## まとめ

opencrabの基盤（SkillEngineループ）はOpenClawと同等の設計。
最小の変更（Bootstrap + プロンプト改善 + fetch_web）で複合タスクの自律実行が実現できる。

過去の設計で提案していた「Phase 3: Multi-step Skill Engine（DAG）」は削除。
LLMのループで十分であり、複雑な実行エンジンは現時点では不要。
