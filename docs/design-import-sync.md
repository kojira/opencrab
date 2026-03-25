# OpenClaw ログ増分同期 API 設計ドキュメント

**作成日**: 2026-03-25
**ステータス**: Draft
**対象**: 既存エージェントへの `memory/*.md` / `MEMORY.md` 追加同期

---

## 1. 概要

### 1.1 背景

`design-openclaw-import.md` で定義した初期インポート機能は「OpenClaw ワークスペース → 新規 OpenCrab エージェント」の一方向一括変換を扱う。しかし実運用では、エージェント作成後もOpenClaw側でログが毎日蓄積し続ける。

本ドキュメントでは、**既に存在する OpenCrab エージェントに対して**、OpenClaw ワークスペースの以下のファイルを追加・更新同期する API を設計する。

| 対象ファイル | 変化パターン | 同期方針 |
|---|---|---|
| `MEMORY.md` | セクション追加・更新 | セクション単位 UPSERT |
| `memory/YYYY-MM-DD.md` | 日次で新規ファイルが追加 | ファイル単位・未同期分を追加 |

### 1.2 初期インポートとの違い

| 観点 | 初期インポート (`POST /api/import/execute`) | 増分同期（本設計） |
|---|---|---|
| 対象エージェント | 新規作成 | 既存エージェント |
| 同期スコープ | SOUL/IDENTITY/MEMORY/スキル全部 | MEMORY.md・daily log のみ |
| 重複チェック | `overwrite_if_exists` フラグ | ファイルハッシュによる差分検知 |
| 同期状態管理 | `import_sync_state` に記録（初回増分同期での重複防止） | `import_sync_state` テーブルで追跡 |
| 実行タイミング | 一度限り（手動） | 定期実行 or ファイル変更検知 |

### 1.3 初期インポートと増分同期の連携

`POST /api/import/execute` 実行時に、インポートした MEMORY.md セクションおよび daily_log ファイルを `import_sync_state` にも記録する。これにより、初回増分同期（`POST /api/agents/{id}/import/sync`）を実行した際に「既にインポート済みのファイルが重複して挿入される」問題を防ぐ。

```
初期インポート実行
  ↓
memory_curated に記録（通常通り）
  ↓
import_sync_state にも記録  ← ここが新設計のポイント
  ↓
後続の増分同期は import_sync_state を見てスキップ判定
```

### 1.4 スコープ外（本設計では扱わない）

- SOUL.md / IDENTITY.md / AGENTS.md の同期（エージェント設定の更新は別途）
- スキルファイルの同期（スキル管理は `skills` APIで対応）
- OpenCrab → OpenClaw への書き戻し（逆方向同期）
- 複数エージェントの一括同期

---

## 2. 現状の課題分析

### 2.1 既存実装の問題点

現在の `upsert_curated_memory` は **UUIDを主キー**としており、同一のファイルを再インポートするたびに新しいレコードが生成される。

```sql
-- 現状: IDは毎回 Uuid::new_v4() で生成
INSERT INTO memory_curated (id, agent_id, category, content, ...)
VALUES (?1, ?2, ?3, ?4, ...)
ON CONFLICT(id) DO UPDATE SET ...
-- → 同じファイルを2回インポートすると2件が蓄積してしまう
```

また、`memory_curated` テーブルには元ファイルのパス・ハッシュ・同期日時を記録するフィールドが存在しない。

### 2.2 必要な機能

1. **同期状態の追跡**: どのファイルをいつ同期したか、内容ハッシュを記録
2. **差分検知**: 前回同期以降に変化したファイルのみを処理
3. **べき等性 (Idempotency)**: 同じファイルを何度同期しても DB に重複が発生しない
4. **セクション単位 UPSERT (MEMORY.md)**: MEMORY.md の特定セクションが更新された場合に既存レコードを上書き

---

## 3. データベース設計

### 3.1 新規テーブル: `import_sync_state`

同期状態を追跡するためのテーブルを新設する。

```sql
CREATE TABLE IF NOT EXISTS import_sync_state (
    id TEXT PRIMARY KEY,                    -- UUID v4
    agent_id TEXT NOT NULL,
    source_dir TEXT NOT NULL,               -- ソースワークスペースディレクトリ (絶対パス)
    file_type TEXT NOT NULL,                -- 'memory_md' | 'daily_log'
    file_name TEXT NOT NULL,                -- 'MEMORY.md' または 'memory/2026-03-25.md'
    content_hash TEXT NOT NULL,             -- SHA-256 of file content (hex 64文字)
    synced_at TEXT NOT NULL,                -- 最終同期日時 (RFC3339)
    created_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_import_sync_state_key
    ON import_sync_state(agent_id, source_dir, file_name);

CREATE INDEX IF NOT EXISTS idx_import_sync_state_agent
    ON import_sync_state(agent_id);
```

**設計の根拠**:
- `(agent_id, source_dir, file_name)` をユニークキーとすることで、同一ファイルの同期状態を1レコードで管理し、重複挿入を防ぐ
- `content_hash` の変化を検知して差分同期を実現
- `memory_curated` の ID は通常の `Uuid::new_v4()` で生成する。`import_sync_state` 側のユニーク制約で重複防止するため、決定論的IDは不要

### 3.2 既存テーブル変更: `memory_curated` への unique index 追加

`memory_curated` テーブルに `(agent_id, category)` の **UNIQUE INDEX** を追加し、同一カテゴリのレコードを UPSERT（上書き）で管理する。

```sql
-- 追加するインデックス
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_curated_agent_category
    ON memory_curated(agent_id, category);
```

**UPSERT 時の SQL 変更**:

```sql
-- 変更前: id の ON CONFLICT だけなので同一カテゴリが重複挿入される
INSERT INTO memory_curated (id, agent_id, category, content, ...)
VALUES (?1, ?2, ?3, ?4, ...)
ON CONFLICT(id) DO UPDATE SET ...

-- 変更後: (agent_id, category) の衝突時に既存レコードを上書き
INSERT INTO memory_curated (id, agent_id, category, content, updated_at, ...)
VALUES (?1, ?2, ?3, ?4, ?5, ...)
ON CONFLICT(agent_id, category) DO UPDATE SET
    content    = excluded.content,
    updated_at = excluded.updated_at
```

**設計の根拠**:

| 観点 | 説明 |
|---|---|
| 重複防止の二重防護 | `import_sync_state` のハッシュ差分検知 + `memory_curated` の UNIQUE 制約の二層構造。どちらか一方が欠落しても重複が発生しない |
| 初期インポート後の増分同期 | 初期インポートで `category = "daily_log/2026-03-25"` が挿入済みでも、増分同期が同じ category で INSERT を実行した場合は上書きになる（内容が同じであれば実質的に変化なし） |
| force_resync 対応 | `force_resync = true` の場合、`import_sync_state` をスキップしてすべてのファイルを再処理するが、UNIQUE 制約により `memory_curated` への重複挿入は防がれる |
| 既存データとの後方互換性 | インデックス追加の DDL を migration で実行するため、既存レコードが重複している場合は事前にクリーンアップが必要（migration スクリプトに dedup 処理を含める） |

**migration 時の注意**:

```sql
-- 既存の重複レコードをマージしてから unique index を作成する
-- (同一 agent_id + category で複数レコードが存在する場合、最新の updated_at を残す)
DELETE FROM memory_curated
WHERE id NOT IN (
    SELECT id FROM memory_curated mc2
    WHERE mc2.agent_id = memory_curated.agent_id
      AND mc2.category = memory_curated.category
    ORDER BY updated_at DESC
    LIMIT 1
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_curated_agent_category
    ON memory_curated(agent_id, category);
```

---

## 4. API 設計

### 4.1 エンドポイント一覧

| メソッド | パス | 概要 |
|---|---|---|
| `GET` | `/api/agents/{id}/import/sync/status` | 同期状態の確認（差分プレビュー） |
| `POST` | `/api/agents/{id}/import/sync` | 同期実行 |
| `GET` | `/api/agents/{id}/import/sync/history` | 同期履歴一覧 |

### 4.2 GET /api/agents/{id}/import/sync/status

同期を実行せず、「何が新しいか・何が変わったか」をプレビューする。

**リクエスト**:
```
GET /api/agents/{id}/import/sync/status?source_dir=/Volumes/2TB/openclaw/workspace
```

| パラメータ | 型 | 必須 | 説明 |
|---|---|---|---|
| `source_dir` | string | ✅ | OpenClaw ワークスペースの絶対パス |
| `include_daily_logs` | bool | - | daily_log を含めるか（デフォルト: true） |
| `daily_log_days` | int | - | 最新N日分のみチェック（デフォルト: 30） |

**レスポンス例**:
```json
{
  "agent_id": "agent-abc-123",
  "source_dir": "/Volumes/2TB/openclaw/workspace",
  "last_sync_at": "2026-03-22T10:00:00+09:00",
  "changes": {
    "memory_md": {
      "status": "changed",
      "sections_total": 8,
      "sections_new": 1,
      "sections_updated": 2,
      "sections_unchanged": 5,
      "details": [
        {
          "section": "long_term/重要な仲間",
          "status": "updated",
          "prev_hash": "abc123...",
          "new_hash": "def456..."
        },
        {
          "section": "long_term/新セクション",
          "status": "new"
        }
      ]
    },
    "daily_logs": {
      "total_files": 45,
      "new_files": 3,
      "already_synced": 42,
      "new_file_names": [
        "memory/2026-03-23.md",
        "memory/2026-03-24.md",
        "memory/2026-03-25.md"
      ]
    }
  },
  "has_changes": true
}
```

### 4.3 POST /api/agents/{id}/import/sync

差分を検知して実際に同期を実行する。

**リクエスト**:
```json
{
  "source_dir": "/Volumes/2TB/openclaw/workspace",
  "options": {
    "include_daily_logs": true,
    "daily_log_days": 30,
    "force_resync": false
  }
}
```

| フィールド | 型 | 説明 |
|---|---|---|
| `source_dir` | string | OpenClaw ワークスペースの絶対パス |
| `include_daily_logs` | bool | daily_log を含めるか（デフォルト: true） |
| `daily_log_days` | int | 最新N日分のみ同期（デフォルト: 30） |
| `force_resync` | bool | ハッシュが一致しても強制的に再同期（デフォルト: false） |

**レスポンス例**:
```json
{
  "agent_id": "agent-abc-123",
  "synced_at": "2026-03-25T14:30:00+09:00",
  "result": {
    "memory_md": {
      "sections_upserted": 3,
      "sections_skipped": 5
    },
    "daily_logs": {
      "files_imported": 3,
      "files_skipped": 42,
      "entries_created": 3
    }
  },
  "warnings": [],
  "errors": []
}
```

**エラーレスポンス例**:
```json
{
  "error": "source_dir does not exist: /invalid/path",
  "code": "INVALID_SOURCE_DIR"
}
```

### 4.4 GET /api/agents/{id}/import/sync/history

過去の同期履歴を返す。

**リクエスト**:
```
GET /api/agents/{id}/import/sync/history?limit=20&offset=0
```

**レスポンス例**:
```json
{
  "total": 42,
  "items": [
    {
      "id": "sync-uuid-1",
      "agent_id": "agent-abc-123",
      "source_dir": "/Volumes/2TB/openclaw/workspace",
      "file_type": "daily_log",
      "file_name": "memory/2026-03-25.md",
      "content_hash": "abc123...",
      "synced_at": "2026-03-25T14:30:00+09:00"
    }
  ]
}
```

---

## 5. 同期ロジック詳細

### 5.1 MEMORY.md の同期フロー

```
[1] MEMORY.md を読み込む
         ↓
[2] H2 セクション単位に分割
    例: "## 重要な仲間" → {heading, body}
         ↓
[3] import_sync_state を検索（file_name = "MEMORY.md::{heading}"）
    - レコードなし → 新規 (status=new)
    - content_hash が変化 → 更新 (status=updated)
    - content_hash が一致 → スキップ (status=unchanged)
         ↓
[4] new / updated のセクションのみ UPSERT
    upsert_curated_memory(id=Uuid::new_v4(), category="long_term/{heading}", content=body)
    ※ memory_curated の (agent_id, category) UNIQUE INDEX により、
       同一カテゴリが既に存在する場合は content / updated_at を上書き
       (→ 3.2 参照)
         ↓
[5] import_sync_state を更新
    (agent_id, source_dir, "MEMORY.md::{heading}") → {hash, synced_at}
```

**セクション単位の差分検知**:
- MEMORY.md 全体のハッシュが変わっていても、変更されたセクションのみ再 UPSERT する
- 各セクションを個別の `import_sync_state` レコードとして管理する

> **設計選択**: セクション単位で `import_sync_state` に個別レコードを持つ。
> `(agent_id, source_dir, "MEMORY.md::long_term/重要な仲間")` をユニークキーとする。

```
file_name カラムの例:
- MEMORY.md::long_term/重要な仲間
- MEMORY.md::long_term/セキュリティルール
- memory/2026-03-25.md       (daily_log はファイル単位)
```

**セクション削除・名称変更時の扱い**:
- 増分同期では**追加・更新のみ**を行い、削除は一切しない
- MEMORY.md からセクションが削除・名称変更されても、対応する `memory_curated` レコードは保持される
- これにより意図しないデータ消失を防ぐ。不要なレコードの削除は手動またはアーカイブ機能で対応する

### 5.2 daily_log の同期フロー

```
[1] memory/ ディレクトリの YYYY-MM-DD.md ファイルを列挙
         ↓
[2] daily_log_days オプションに基づき日付フィルタ
    例: 直近30日 → 2026-02-24 以降のファイルのみ
         ↓
[3] 各ファイルについて import_sync_state を検索
    - レコードなし → 未同期 (新規追加対象)
    - content_hash が一致 → スキップ
    - content_hash が変化 → 更新 (通常は日次ファイルは変化しないが念のため)
         ↓
[4] 未同期ファイルを memory_curated に追加
    - id = Uuid::new_v4()  ← 通常のUUID（決定論的IDは使わない）
    - category = "daily_log/2026-03-25"
    - content = ファイル全文
    ※ (agent_id, category) UNIQUE INDEX により、
       同一 category が既存の場合は INSERT が ON CONFLICT UPDATE に
       フォールバックし、重複レコードを防止（→ 3.2 参照）
         ↓
[5] import_sync_state に記録
    (agent_id, source_dir, "memory/2026-03-25.md") → {hash, synced_at}
```

**削除・ファイル消去時の扱い**:
- daily_log ファイルがソースディレクトリから削除されても、対応する `memory_curated` レコードは保持される（追加・更新のみ）

---

## 6. Rust 実装構造

### 6.1 新規ファイル・変更ファイル

```
crates/
├── core/src/import/
│   ├── mod.rs                    (変更: sync モジュールを追加)
│   ├── openclaw_parser.rs        (既存: 変更なし)
│   ├── import_service.rs         (既存: 変更・import_sync_state記録を追加)
│   └── sync_service.rs           (新規: 同期ロジック本体)
│
├── db/src/
│   ├── schema.rs                 (変更: import_sync_state テーブル追加)
│   └── queries.rs                (変更: sync_state CRUD 関数追加)
│
└── server/src/api/
    ├── mod.rs                    (変更: sync ルート追加)
    └── import_sync.rs            (新規: HTTP ハンドラ)
```

### 6.2 sync_service.rs の主要関数

```rust
pub struct SyncOptions {
    pub include_daily_logs: bool,
    pub daily_log_days: u32,
    pub force_resync: bool,
}

pub struct SyncStatusResult {
    pub agent_id: String,
    pub source_dir: String,
    pub last_sync_at: Option<String>,
    pub memory_md_changes: MemoryMdChanges,
    pub daily_log_changes: DailyLogChanges,
    pub has_changes: bool,
}

pub struct SyncResult {
    pub agent_id: String,
    pub synced_at: String,
    pub memory_md_upserted: usize,
    pub memory_md_skipped: usize,
    pub daily_logs_imported: usize,
    pub daily_logs_skipped: usize,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// 同期状態チェック（プレビュー、DBへの書き込みなし）
pub fn check_sync_status(
    conn: &Connection,
    agent_id: &str,
    source_dir: &str,
    options: &SyncOptions,
) -> anyhow::Result<SyncStatusResult>;

/// 実際に同期を実行
pub fn execute_sync(
    conn: &Connection,
    agent_id: &str,
    source_dir: &str,
    options: &SyncOptions,
) -> anyhow::Result<SyncResult>;
```

### 6.3 db/queries.rs への追加

```rust
// import_sync_state のCRUD
pub struct SyncStateRow {
    pub id: String,
    pub agent_id: String,
    pub source_dir: String,
    pub file_name: String,          // "MEMORY.md::long_term/重要な仲間" or "memory/2026-03-25.md"
    pub content_hash: String,
    pub synced_at: String,
    pub created_at: String,
}

pub fn get_sync_state(
    conn: &Connection,
    agent_id: &str,
    source_dir: &str,
    file_name: &str,
) -> Result<Option<SyncStateRow>>;

pub fn upsert_sync_state(conn: &Connection, row: &SyncStateRow) -> Result<()>;

pub fn list_sync_states(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<SyncStateRow>, i64)>;

pub fn delete_sync_states_for_agent(conn: &Connection, agent_id: &str) -> Result<()>;
```

---

## 7. ハッシュ計算

### 7.1 ファイル全体のハッシュ (daily_log)

```rust
use sha2::{Sha256, Digest};

fn hash_content(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}
```

### 7.2 セクション単位のハッシュ (MEMORY.md)

```rust
fn hash_section(heading: &str, body: &str) -> String {
    let combined = format!("{}|{}", heading, body);
    hash_content(&combined)
}
```

---

## 8. セキュリティ・安全性

### 8.1 エージェント存在確認

同期実行前に対象エージェントが存在することを確認する。存在しない場合は `404 Not Found`。

```rust
let agent = get_identity(conn, agent_id)?;
if agent.is_none() {
    return Err(SyncError::AgentNotFound(agent_id.to_string()));
}
```

### 8.2 source_dir の検証

初期インポートと同様のパストラバーサル対策を適用する。

- 絶対パスに正規化 (`canonicalize`)
- シンボリックリンクの解決
- ディレクトリ境界の検証

### 8.3 ファイルサイズ制限

1 ファイルあたり最大 1 MB を上限とする（daily_log が異常に大きい場合を防ぐ）。

---

## 9. ダッシュボード UI（参考）

```
┌────────────────────────────────────────────────────┐
│  OpenClaw ログ同期 (エージェント)                  │
├────────────────────────────────────────────────────┤
│  ソースディレクトリ: /Volumes/2TB/openclaw/workspace│
│  最終同期: 2026-03-22 10:00  [今すぐ同期]          │
├────────────────────────────────────────────────────┤
│  変更検知結果                                       │
│  📝 MEMORY.md: 3セクション変更 (新規1, 更新2)      │
│  📅 daily_log: 3ファイル未同期                     │
│    - memory/2026-03-23.md (未同期)                  │
│    - memory/2026-03-24.md (未同期)                  │
│    - memory/2026-03-25.md (未同期)                  │
├────────────────────────────────────────────────────┤
│  オプション                                         │
│  [✓] daily_log を含める  最新 [30] 日              │
│  [ ] 強制再同期（差分なしも再処理）                 │
├────────────────────────────────────────────────────┤
│        [同期実行]      [ステータス更新]             │
└────────────────────────────────────────────────────┘
```

---

## 10. 将来の拡張

### 10.1 自動同期（ファイルウォッチャー）

`hot_reload.rs` で `notify_debouncer_mini` が既に使われているパターンを流用し、OpenClaw ワークスペースの `memory/` ディレクトリを監視する自動同期モードを将来追加できる。

```rust
// 将来の拡張: config に sync_source_dir を追加
// [agent.sync]
// source_dir = "/Volumes/2TB/openclaw/workspace"
// auto_sync = true
// interval_secs = 300
```

エージェントの設定ファイル (`config/<agent_id>.toml`) に同期元ディレクトリを記述し、OpenCrab 起動時に自動同期ウォッチャーを開始するオプション。

### 10.2 複数ソースからの同期

`(agent_id, source_dir, file_name)` の複合キーにより、同一エージェントに複数のソースディレクトリからログを集約することが可能。例:

- `/Volumes/2TB/openclaw/workspace` (メイン環境)
- `/Users/username/openclaw/workspace` (サブ環境)

### 10.3 スキルの更新同期

将来的に、SKILL.md の内容が変更された場合に既存スキルを更新するフローを追加できる。`import_sync_state` の `file_name` に `skills/discord-webhook/SKILL.md` 等を記録する拡張が可能。

### 10.4 Semantic Dedup・メモリマージ機能

蓄積された `memory_curated` の中には、異なる日付や異なるソースファイルから取り込まれた**内容的に類似するレコード**が存在する可能性がある。将来課題として、以下の機能を検討する:

- **類似度計算**: `memory_curated` のコンテンツをベクトル埋め込みし、コサイン類似度で近傍レコードを検出
- **Semantic Dedup**: 高類似度のレコードを重複とみなしてマージ候補としてフラグ付け
- **メタ記憶へのアップグレード**: 複数の具体的な日次ログを集約し、より抽象度の高い「メタな記憶」レコードに変換・格納する機能
- **実装候補**: SQLite + sqlite-vss（ベクトル拡張）、または外部の埋め込みAPIを使用

---

## 11. 実装ステップ

| フェーズ | 項目 | 工数見積 |
|---|---|---|
| P1 | DB: `import_sync_state` テーブル追加 + `queries.rs` CRUD | 小（0.5日） |
| P1 | Core: `sync_service.rs` 実装（ハッシュ計算・差分検知・UPSERT） | 中（1-2日） |
| P1 | Core: `import_service.rs` に初期インポート時の `import_sync_state` 記録を追加 | 小（0.5日） |
| P1 | Server: `import_sync.rs` APIハンドラ + ルート登録 | 小（0.5日） |
| P2 | Dashboard: 同期UIコンポーネント (`SyncPanel.tsx`) | 中（1日） |
| P3 | 自動同期ウォッチャー（設定ファイル連動） | 中（1-2日） |
| P4 | スキル更新同期への拡張 | 小（0.5日） |
| P5 | Semantic Dedup・メモリマージ機能 | 大（3-5日） |

**推奨実装順**: P1（DBスキーマ）→ P1（sync_service）→ P1（import_service修正）→ P1（APIハンドラ）→ E2Eテスト → P2（UI）

---

## 12. テスト計画

### 12.1 ユニットテスト

```rust
#[test]
fn test_sync_skips_unchanged_files() { ... }

#[test]
fn test_sync_updates_changed_sections() { ... }

#[test]
fn test_sync_adds_new_daily_logs_only() { ... }

#[test]
fn test_sync_is_idempotent() {
    // 同じ内容で2回 execute_sync を呼んでも結果が重複しないことを確認
    // import_sync_state のユニーク制約により重複挿入が防止されることを検証
    ...
}

#[test]
fn test_sync_does_not_delete_removed_sections() {
    // MEMORY.md からセクションを削除した後に再同期しても
    // memory_curated に既存レコードが残ることを確認
    ...
}

#[test]
fn test_initial_import_records_sync_state() {
    // 初期インポート実行後に import_sync_state が記録されることを確認
    // → 後続の増分同期でスキップされることを確認
    ...
}
```

### 12.2 E2Eテスト

- テスト用フィクスチャーに `MEMORY.md` + `memory/` ディレクトリを用意
- 初回同期: 全ファイルが取り込まれることを確認
- 再同期: ファイルが変化しない場合はスキップされることを確認
- 更新同期: MEMORY.md のセクションを変更後に再同期し、既存レコードが更新されることを確認
- 新規日次ログ: 新しいファイルのみが追加されることを確認
- 初期インポート後の増分同期: 重複レコードが発生しないことを確認

---

## 13. 関連ドキュメント

- [`design-openclaw-import.md`](./design-openclaw-import.md) — 初期インポート機能の設計
- [`api.md`](./api.md) — OpenCrab REST API 一覧
- `crates/core/src/import/` — 既存インポート実装

---

*このドキュメントは OpenCrab 開発の一部として作成されました。*
