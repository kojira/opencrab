# OpenClaw → OpenCrab インポート機能 設計ドキュメント

**作成日**: 2026-03-22  
**ステータス**: Draft

---

## 1. 概要

OpenClaw（既存エージェントシステム）のワークスペースディレクトリをOpenCrabにインポートする機能。
エージェントのソウル・アイデンティティ・長期記憶・スキルを移行し、OpenCrabで継続的に動作させることを目的とする。

### 1.1 インポートの対象範囲

| OpenClaw ファイル/ディレクトリ | OpenCrab テーブル/フィールド | 優先度 |
|---|---|---|
| `SOUL.md` | `soul.personality` | 必須 |
| `IDENTITY.md` | `soul.persona_name`, `identity.*` | 必須 |
| `USER.md` | `memory_curated` (category=user_profile) | 推奨 |
| `AGENTS.md` | `memory_curated` (category=agent_rules) | 推奨 |
| `MEMORY.md` | `memory_curated` (category=long_term) | 必須 |
| `memory/YYYY-MM-DD.md` | `memory_curated` (category=daily_log) | オプション |
| `skills/*/SKILL.md` | `skills.*` | 推奨 |

### 1.2 除外対象（インポートしないもの）

セキュリティおよび互換性の理由から、以下はインポートしない：

```
# セキュリティ（シークレット・トークン）
openclaw.json           # bot_token, API keys
.env                    # 環境変数
*.json (webhooks等)     # Webhook URL等

# バイナリ・メディア
*.mp4 *.png *.gif *.wav # 音声・動画・画像ファイル
*.db *.sqlite           # DBファイル
node_modules/           # 依存パッケージ
target/                 # ビルド成果物

# システムファイル
.git/                   # gitリポジトリ
tmp/                    # 一時ファイル
*.log *.gz              # ログファイル

# OpenClaw固有の設定（OpenCrab非対応）
HEARTBEAT.md            # ハートビート設定（OpenCrab側で再設定）
BOOTSTRAP.md            # ブートストラップ設定（同上）
```

---

## 2. OpenClawデータ構造の詳細

### 2.1 ワークスペースの種類

OpenClawには2種類のワークスペースがある：

1. **メインワークスペース** (`/Volumes/2TB/openclaw/workspace/` など)
   - エージェントが実際に使うワークスペース
   - カスタムSOUL.md、MEMORY.md、スキル群が存在する
   - インポートのメイン対象

2. **OpenClaw設定ワークスペース** (`~/.openclaw/workspace/`)
   - openclaw本体のデフォルト設定
   - ほぼデフォルト内容（インポート対象外）

### 2.2 ファイルフォーマット

#### SOUL.md
```markdown
# SOUL.md - Who You Are
## Core Truths
（エージェントの信条・行動原則）

## Vibe
（ペルソナ定義：名前・年齢・スタイル・口調）

## Continuity
（セッション間の継続性に関するルール）
```

**抽出方針**:
- `## Vibe` セクションから `**Name:**` 行でペルソナ名を抽出 → `soul.persona_name`
- ファイル全文を `soul.personality` に格納（Markdown そのまま）

#### IDENTITY.md
```markdown
# IDENTITY.md - Who Am I?
- **Name:** のすたろう
- **Creature:** Nostr空間上に住む電脳存在
- **Vibe:** 17歳男子高校生。...
- **Emoji:** ⚡
- **Avatar:** (URL or path)
```

**抽出方針**:
- `Name:` → `identity.name` および `soul.persona_name`
- `Avatar:` → `identity.image_url`
- 残りのキー/バリューペア → `identity.metadata_json` にJSONとして格納

#### MEMORY.md
```markdown
# MEMORY.md - のすたろうの長期記憶

## 重要な仲間
（人物情報）

## Agent Hub
（サーバー設定）

## セキュリティルール
（ルール群）
```

**抽出方針**:
- H2セクション（`## セクション名`）を単位として分割
- 各セクション → `memory_curated`の1エントリ
  - `category`: セクション名（例: `long_term/重要な仲間`）
  - `content`: セクションのMarkdown本文

#### memory/YYYY-MM-DD.md
```markdown
# 2026-03-21 のすたろう日記
## Nostr会話（00:41〜）
...
```

**抽出方針**:
- 各ファイル → 1エントリまたは日付でグループ化して `memory_curated`
  - `category`: `daily_log`
  - `content`: ファイル全文
- **注意**: 日次ログは量が多いため、最新N日分のみインポートするオプションを提供

#### skills/*/SKILL.md
```
skills/
├── discord-webhook/
│   └── SKILL.md       # スキル定義
├── python-sandbox/
│   └── SKILL.md
└── ...
```

**抽出方針**:
- ディレクトリ名 → `skills.name`
- SKILL.mdの1行目（`# タイトル`）→ `skills.description`
- SKILL.mdの全文 → `skills.guidance`
- `situation_pattern`: SKILL.mdの説明文からLLMで自動生成（またはdescription流用）
- `source_type`: `"openclaw_import"`
- `source_context`: 元のファイルパス

---

## 3. OpenCrabデータ構造との対応

### 3.1 soulテーブル

```sql
CREATE TABLE soul (
    agent_id TEXT PRIMARY KEY,
    persona_name TEXT NOT NULL,          -- IDENTITY.md の Name
    social_style_json TEXT DEFAULT '{}', -- 今回は空で初期化
    thinking_style_json TEXT DEFAULT '{}', -- 今回は空で初期化
    personality TEXT,                    -- SOUL.md 全文
    updated_at TEXT NOT NULL
)
```

### 3.2 identityテーブル

```sql
CREATE TABLE identity (
    agent_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,           -- IDENTITY.md の Name
    job_title TEXT,               -- IDENTITY.md の Vibe等から抽出
    organization TEXT,            -- 未使用 or IDENTITY.md の Creature
    image_url TEXT,               -- IDENTITY.md の Avatar
    metadata_json TEXT,           -- IDENTITY.md の残りキー/バリュー
    updated_at TEXT NOT NULL
)
```

### 3.3 memory_curatedテーブル

```sql
CREATE TABLE memory_curated (
    id TEXT PRIMARY KEY,          -- UUID v4生成
    agent_id TEXT NOT NULL,
    category TEXT NOT NULL,       -- "long_term/セクション名", "daily_log", "user_profile", "agent_rules"
    content TEXT NOT NULL,        -- Markdownテキスト
    updated_at TEXT NOT NULL
)
```

### 3.4 skillsテーブル

```sql
CREATE TABLE skills (
    id TEXT PRIMARY KEY,          -- UUID v4生成
    agent_id TEXT NOT NULL,
    name TEXT NOT NULL,           -- スキルディレクトリ名
    description TEXT NOT NULL,    -- SKILL.md H1見出し
    situation_pattern TEXT NOT NULL, -- description流用 or 自動抽出
    guidance TEXT NOT NULL,       -- SKILL.md 全文
    source_type TEXT DEFAULT 'openclaw_import',
    source_context TEXT,          -- 元ファイルパス
    is_active INTEGER DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)
```

---

## 4. インポートUI/UX設計

### 4.1 インターフェース方針

**推奨**: REST API + ダッシュボードUIの組み合わせ

- **REST API**: 自動化・スクリプト連携・CI/CD対応
- **ダッシュボードUI**: 視覚的なdryrun確認・進捗表示

CLIは既存の `crates/cli/` に追加コマンドとして実装。

### 4.2 フロー設計

```
[1] ディレクトリ指定
         ↓
[2] スキャン & 解析 (dryrun)
         ↓
[3] プレビュー確認
         ↓
[4] 実行確認
         ↓
[5] インポート実行
         ↓
[6] 完了レポート
```

#### フロー詳細

**Step 1: ディレクトリ指定**
```
POST /api/agents/{agent_id}/import/scan
Body: {
  "source_dir": "/Volumes/2TB/openclaw/workspace",
  "options": {
    "include_daily_logs": true,
    "daily_log_days": 30,        // 最新N日分
    "include_skills": true,
    "overwrite_existing": false  // 既存データを上書きするか
  }
}
```

**Step 2: スキャン結果（dryrun）**
```json
{
  "scan_result": {
    "soul": {
      "persona_name": "のすたろう",
      "personality_length": 2048,
      "found": true
    },
    "identity": {
      "name": "のすたろう",
      "image_url": null,
      "found": true
    },
    "memory_curated": {
      "long_term_sections": 8,
      "daily_logs": 45,
      "import_daily_logs": 30
    },
    "skills": {
      "found": 20,
      "importable": 20
    },
    "excluded": [
      "openclaw.json (シークレット)",
      "node_modules/ (バイナリ)",
      "*.mp4 *.png (メディア)"
    ],
    "warnings": [
      "MEMORY.md の '## gateway禁忌ルール' セクションはOpenCrab非対応の記述を含みます"
    ]
  }
}
```

**Step 3: 実行**
```
POST /api/agents/{agent_id}/import/execute
Body: {
  "source_dir": "/Volumes/2TB/openclaw/workspace",
  "options": { ... },
  "confirmed": true
}
```

**Step 4: 進捗レスポンス（SSE or ポーリング）**
```
GET /api/agents/{agent_id}/import/status/{import_id}

{
  "status": "running",  // pending | running | completed | failed
  "progress": {
    "soul": "completed",
    "identity": "completed",
    "memory_curated": "running (23/38)",
    "skills": "pending"
  },
  "errors": []
}
```

### 4.3 ダッシュボードUI

既存の `web/src/pages/` に `ImportPage.tsx` を追加：

```
┌────────────────────────────────────────────────────┐
│  OpenClaw インポート                                │
├────────────────────────────────────────────────────┤
│  対象エージェント: [かいろ ▼]                      │
│  ソースディレクトリ: [/Volumes/2TB/openclaw/workspace] [スキャン] │
├────────────────────────────────────────────────────┤
│  スキャン結果                                       │
│  ✅ SOUL.md      → soul.personality (2.0KB)        │
│  ✅ IDENTITY.md  → identity.name = "のすたろう"    │
│  ✅ MEMORY.md    → 8セクション → memory_curated    │
│  ✅ memory/*.md  → 45日分 (最新30日インポート)     │
│  ✅ skills/      → 20スキル                        │
│  ❌ openclaw.json → 除外 (シークレット)             │
│  ❌ node_modules/ → 除外 (バイナリ)                │
├────────────────────────────────────────────────────┤
│  オプション                                         │
│  [✓] 日次ログを含める  最新 [30] 日                │
│  [✓] スキルを含める                                │
│  [ ] 既存データを上書き                             │
├────────────────────────────────────────────────────┤
│          [インポート実行]                           │
└────────────────────────────────────────────────────┘
```

### 4.4 CLIインターフェース

```bash
# スキャン（dryrun）
opencrab import scan --agent <agent_id> --source /path/to/openclaw/workspace

# 実行
opencrab import run --agent <agent_id> --source /path/to/openclaw/workspace \
  --daily-log-days 30 \
  --overwrite

# 対話式（おすすめ）
opencrab import --agent <agent_id> --source /path/to/openclaw/workspace
> スキャン結果を表示... インポートしますか？ [y/N]
```

---

## 5. 実装ステップ（優先順位付き）

### Phase 1: コア実装（必須・最優先）

**P1-1: データパーサー**  
`crates/core/src/import/` を新規作成

- [ ] `openclaw_parser.rs`: OpenClawファイルの解析
  - `parse_soul_md(path)` → `SoulImportData`
  - `parse_identity_md(path)` → `IdentityImportData`  
  - `parse_memory_md(path)` → `Vec<MemoryCuratedImportData>`
  - `parse_skill_md(skill_dir)` → `SkillImportData`
  - `scan_workspace(dir, options)` → `ScanResult`

- [ ] `import_service.rs`: DBへの書き込みロジック
  - `execute_import(agent_id, scan_result, conn)` → `ImportResult`
  - トランザクションで全データを一括コミット
  - `overwrite=false` 時は既存データをスキップ

**P1-2: REST APIエンドポイント**  
`crates/server/src/api/import.rs` を新規作成

- [ ] `POST /api/agents/{agent_id}/import/scan`
- [ ] `POST /api/agents/{agent_id}/import/execute`
- [ ] `GET /api/agents/{agent_id}/import/status/{import_id}`

**P1-3: データ変換**

```rust
// SOUL.md → soul テーブル
fn parse_soul(content: &str) -> SoulImportData {
    SoulImportData {
        persona_name: extract_persona_name(content), // "## Vibe" セクションから
        personality: content.to_string(),
    }
}

// IDENTITY.md → identity テーブル
fn parse_identity(content: &str) -> IdentityImportData {
    let fields = extract_kv_pairs(content); // "- **Key:** Value" 形式をパース
    IdentityImportData {
        name: fields.get("Name").cloned().unwrap_or_default(),
        image_url: fields.get("Avatar").filter(|v| !v.is_empty()).cloned(),
        metadata_json: serde_json::to_string(&fields).unwrap_or_default(),
    }
}

// MEMORY.md → memory_curated テーブル（セクション分割）
fn parse_memory(content: &str) -> Vec<MemoryCuratedImportData> {
    split_h2_sections(content)
        .map(|(heading, body)| MemoryCuratedImportData {
            category: format!("long_term/{}", heading),
            content: body.to_string(),
        })
        .collect()
}

// skills/*/SKILL.md → skills テーブル
fn parse_skill(dir: &Path) -> Option<SkillImportData> {
    let skill_md = dir.join("SKILL.md");
    let content = fs::read_to_string(&skill_md).ok()?;
    let name = dir.file_name()?.to_string_lossy().to_string();
    let description = extract_h1_title(&content).unwrap_or(name.clone());
    Some(SkillImportData {
        name,
        description,
        situation_pattern: description.clone(), // シンプルな初期値
        guidance: content,
        source_type: "openclaw_import".to_string(),
        source_context: Some(skill_md.to_string_lossy().to_string()),
    })
}
```

### Phase 2: UI実装（推奨）

- [ ] `web/src/pages/ImportPage.tsx`
  - ディレクトリパス入力
  - スキャン結果のプレビュー表示
  - オプション設定（日次ログ日数、スキルON/OFF、上書きON/OFF）
  - インポート実行ボタン
  - 進捗表示

- [ ] サイドバーに「インポート」メニュー追加

### Phase 3: CLIコマンド（オプション）

- [ ] `crates/cli/src/commands/import.rs`
  - `scan` / `run` サブコマンド
  - 対話式確認フロー

### Phase 4: 高度な機能（将来）

- [ ] **増分インポート**: 前回インポート以降の差分のみ取り込む
- [ ] **双方向同期**: OpenCrab → OpenClaw への書き戻し
- [ ] **スキルの `situation_pattern` 自動生成**: LLM でSKILL.mdからパターンを抽出
- [ ] **マルチエージェント対応**: 複数エージェントを一括インポート
- [ ] **インポート履歴**: `import_log` テーブルでインポート履歴を管理

---

## 6. セキュリティ考慮事項

### 6.1 パストラバーサル対策

- ソースディレクトリは絶対パスに正規化
- シンボリックリンクはディレクトリ外へのリンクを除外
- ファイルサイズ上限（例: 10MB/ファイル）

### 6.2 除外パターンの実装

```rust
const EXCLUDED_PATTERNS: &[&str] = &[
    "openclaw.json",
    ".env",
    "*.db",
    "*.sqlite",
    "node_modules",
    "target",
    "tmp",
    ".git",
    "*.log",
    "*.gz",
    "*.mp4",
    "*.mp3",
    "*.wav",
    "*.png",
    "*.jpg",
    "*.gif",
];
```

### 6.3 シークレット検出（任意）

インポート対象テキストに以下のパターンが含まれる場合に警告：
- `DISCORD_BOT_TOKEN=` / `bot_token:`
- `sk-` (OpenAI APIキー)
- `sk-ant-` (Anthropic APIキー)
- `Bearer ` + 長い文字列

---

## 7. テスト計画

### 7.1 ユニットテスト

```rust
#[test]
fn test_parse_soul_md() { ... }

#[test]
fn test_parse_identity_md_full() { ... }

#[test]
fn test_parse_memory_sections() { ... }

#[test]
fn test_parse_skill_dir() { ... }

#[test]
fn test_excluded_patterns() { ... }
```

### 7.2 E2Eテスト

- テスト用フィクスチャーディレクトリを `tests/fixtures/openclaw_workspace/` に作成
- `scan` → `execute` の完全フローをテスト
- `overwrite=false` でのスキップ動作を確認
- 不正パス・存在しないディレクトリでのエラーハンドリング確認

---

## 8. 実装優先順位まとめ

| 優先度 | 項目 | 工数見積 |
|---|---|---|
| P1 | データパーサー（SOUL/IDENTITY/MEMORY/SKILL） | 中（2-3日） |
| P1 | REST API（scan + execute） | 小（1日） |
| P2 | ダッシュボードUI（ImportPage） | 中（2日） |
| P3 | CLIコマンド | 小（1日） |
| P4 | 増分インポート | 大（3-5日） |
| P4 | LLMによるsituation_pattern自動生成 | 中（1-2日） |

**推奨実装順**: P1-1 → P1-2 → P1-3（E2Eテスト）→ P2（UI） → P3（CLI）

---

## 9. 参考情報

### OpenClawワークスペース構成（実測）

```
/Volumes/2TB/openclaw/workspace/
├── SOUL.md           # エージェント性格・ペルソナ（2KB程度）
├── IDENTITY.md       # 名前・アバター等のメタデータ
├── USER.md           # オーナー情報
├── AGENTS.md         # 運用ルール（サブタスク・安全等）
├── MEMORY.md         # 長期記憶（168行）
├── TOOLS.md          # ツール固有メモ
├── memory/           # 日次ログ（45ファイル）
│   ├── 2026-02-03.md
│   └── ...
└── skills/           # スキル群（20ディレクトリ）
    ├── discord-webhook/
    ├── python-sandbox/
    └── ...
```

### OpenCrab DBテーブル一覧

```
soul, identity, memory_curated, memory_sessions,
skills, sessions, agent_sessions,
agent_discord_config, discord_channel_config,
agent_allowed_commands, trusted_co_agents, trusted_discord_users,
llm_logs, llm_usage_metrics, model_pricing, model_experience_notes,
impressions, heartbeat_log
```

---

*このドキュメントはOpenCrab開発の一部として作成されました。*

## レビューメモ（2026-03-22 by らぼみ）

- **`TOOLS.md` を除外リストに追加**: SSHホスト・IPアドレス等のインフラ情報が含まれる可能性があるため除外対象とする（またはシークレット検出対象に含める）
- **`situation_pattern` の初期値**: descriptionをそのまま流用しているが、「どういう状況で使うか」と「何をするか」は異なる概念。NOTE: インポート後にユーザーが手動で修正することを推奨

## 追加レビューメモ（2026-03-22 by kojira指摘）

- **スキルスクリプトのワークスペースコピーが未考慮**: 現在の設計はSKILL.mdのguidanceテキストのみを取り込む設計になっている
- **必要な追加**: スキルに関連する実行スクリプト（.py等）をかいろのワークスペース内にコピーするフローが必要
  - 例: `scripts/generate_image.py` → `{agent_workspace}/scripts/nano-banana-pro/generate_image.py`
  - guidanceのスクリプトパスもワークスペース相対パスに書き換える
- **設計原則**: ワークスペース外への参照はNG。スクリプト・依存ファイルはすべてワークスペース内に配置する
