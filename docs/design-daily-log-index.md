# daily_log → memory_index 取り込み設計

**作成日**: 2026-03-25  
**ステータス**: Draft  
**対象**: `memory_curated` の `daily_log` カテゴリを `memory_index` に自動投入し、エージェントが `retrieve_memory_nodes` で過去の日次ログを効率的に検索できる状態にする

---

## 1. 背景と課題

### 1.1 現状の daily_log 管理

`design-openclaw-import.md` および `design-import-sync.md` の設計では、OpenClaw ワークスペースの `memory/YYYY-MM-DD.md` ファイルは以下のように扱われる：

```
memory/2026-02-01.md
memory/2026-02-02.md
...
memory/2026-03-25.md
              ↓ import/sync
memory_curated テーブル
  category = "daily_log/2026-02-01"
  content  = ファイル全文（数KB〜数十KB）
```

これは「データを失わない」という観点では正しいが、**エージェントが活用する観点では問題がある**。

### 1.2 問題点

#### トークン肥大化
エージェントが過去のコンテキストを参照するとき、`daily_log` の全文をシステムプロンプトに詰め込む運用は現実的でない。2月分だけで数十ファイル・数百KBになるケースがある。

#### 検索不能
`memory_curated` に格納された `daily_log` は現在 `search_my_history`（FTS5、セッションログ対象）でもヒットしない。`browse_memory_index` / `retrieve_memory_nodes` でもアクセスできない。

#### 粒度が粗い
ファイル単位での格納のため、「2月に起きた特定のプロジェクト決定」を取り出すには全文を読み込む必要がある。

### 1.3 目標

1. **memory_curated の daily_log を memory_index に自動投入する**（インポート時・sync 時）
2. **LLM で要約・木構造化して** トークン消費を最小化する
3. **エージェントが `browse_memory_index` → `retrieve_memory_nodes` で** 過去の日次ログ（例: 2月分）を効率的に検索できる状態にする

---

## 2. アーキテクチャ概要

```
OpenClaw workspace
  memory/2026-02-01.md
  memory/2026-02-02.md
  ...
        ↓  import/sync
memory_curated
  category="daily_log/2026-02-01"  content=全文
  category="daily_log/2026-02-02"  content=全文
  ...
        ↓  DailyLogIndexer（本設計で追加）
memory_index_nodes（拡張）
  [root]
    └─ [period: 2026-02]       "2月のまとめ"（LLM要約）
         ├─ [daily: 2026-02-01]  "1日の主なトピック"（LLM要約）
         │    ├─ [topic] "Nostrプロトコル議論"
         │    └─ [topic] "FTv9 学習記録"
         └─ [daily: 2026-02-02]  ...
        ↓  エージェント
browse_memory_index  →  retrieve_memory_nodes
```

### 2.1 コンポーネント責務

| コンポーネント | 責務 |
|---|---|
| `DailyLogIndexer` | memory_curated の daily_log を読み出し、LLM で要約・木構造化して memory_index_nodes に投入 |
| `DailyLogWatermark` | daily_log インデックス構築の進捗管理（既に処理済みのファイル日付を記録） |
| import/sync フック | インポート・sync 完了後に `DailyLogIndexer` を非同期起動 |

---

## 3. スキーマ設計

### 3.1 memory_index_nodes（拡張）

既存テーブルに `source_type` カラムを追加し、セッションログ由来と daily_log 由来を区別する：

```sql
-- 既存テーブルに追加
ALTER TABLE memory_index_nodes ADD COLUMN source_type TEXT NOT NULL DEFAULT 'session_log';
-- 値: 'session_log' | 'daily_log'

-- node_type の拡張（既存 root/period/session/topic に加えて）
-- 'daily'  : 特定の1日（YYYY-MM-DD）の要約ノード
-- 'period' : 月単位の集約ノード（daily_log では月）
```

`source_type = 'daily_log'` の場合のノード階層：

```
root (source_type='daily_log')
  └─ period (YYYY-MM)
       └─ daily (YYYY-MM-DD)
            └─ topic (任意のトピック）
```

> **注**: `source_type = 'session_log'` の既存ツリーとは **root ノードを分離** する。  
> エージェントは `browse_memory_index` を呼ぶ際に `source_type` でフィルタできる。

### 3.2 daily_log_index_watermark（新設）

```sql
CREATE TABLE IF NOT EXISTS daily_log_index_watermark (
    agent_id TEXT NOT NULL,
    last_indexed_date TEXT NOT NULL,   -- 最後にインデックス化した日付 (YYYY-MM-DD)
    updated_at TEXT NOT NULL,
    PRIMARY KEY (agent_id)
);
```

`import_sync_state`（ファイルハッシュ管理）とは別に保持する。ファイルが再インポートされても、インデックス済みであれば LLM 呼び出しをスキップできる。

---

## 4. DailyLogIndexer 設計

### 4.1 処理フロー

```
DailyLogIndexer::run(agent_id, db, llm_client)

[1] daily_log_index_watermark から last_indexed_date を取得
[2] memory_curated から category LIKE 'daily_log/%' のレコードを取得
    WHERE date_part > last_indexed_date ORDER BY date_part ASC
[3] 日付でグループ化（同一日が複数エントリある場合はマージ）
[4] 各日について:
    a. LLM でトピック抽出・要約生成（後述）
    b. memory_index_nodes に UPSERT
       - daily ノード（その日の要約）
       - topic ノード（主要トピック×1〜5件）
[5] 月集約ノード (period) を更新
    - その月の daily ノード一覧の要約から period 要約を再生成
[6] root ノード (source_type='daily_log') を UPSERT
[7] daily_log_index_watermark を更新
```

### 4.2 LLM 呼び出し設計（トークン最適化）

#### daily ノード要約プロンプト

```
以下は {date} の日次ログです。
このログから以下を抽出してください：
1. その日の主要なトピック（最大5件、各20字以内のタイトル）
2. 各トピックの要約（各100字以内）
3. その日全体の1行要約（50字以内）

JSON形式で出力:
{
  "day_summary": "...",
  "topics": [
    {"title": "...", "summary": "..."},
    ...
  ]
}

ログ:
{content}
```

#### period ノード要約プロンプト

```
以下は {year_month} の日次ログ一覧の要約です。
月全体を50字以内で要約してください。

{day_summaries_list}
```

#### トークン節約の工夫

| 工夫 | 内容 |
|---|---|
| バッチ処理 | 1日分ずつではなく、同月の daily を 1 LLM 呼び出しにまとめる（コンテキスト許容量内） |
| キャッシュ | 既にインデックス済みの daily ノードは LLM を呼ばない（watermark 管理） |
| モデル選択 | 要約には小型モデル（例: gemini-flash, haiku）を使用。`select_llm` で動的切り替え |
| チャンク分割 | 1日のログが 4KB 超の場合はチャンク分割して要約→マージ |

### 4.3 UPSERT 戦略

```sql
-- daily ノード
INSERT INTO memory_index_nodes (
    id, agent_id, node_type, source_type,
    parent_id, title, summary,
    date_from, date_to,
    created_at, updated_at
)
VALUES (?, ?, 'daily', 'daily_log', ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(agent_id, source_type, node_type, date_from)
DO UPDATE SET
    summary    = excluded.summary,
    updated_at = excluded.updated_at;
```

既にインデックス化された daily ノードはウォーターマークでスキップするが、インポート元ファイルの内容が変わった場合（sync で更新検知）は再インデックスを許容する。

#### root ノード・period ノードの date_from = NULL の扱い

`root` ノードと `period` ノードは特定の1日に対応しないため、`date_from` が `NULL` になる。

**問題**: SQLite では `NULL = NULL` が成立しないため、`ON CONFLICT(agent_id, source_type, node_type, date_from)` は `date_from IS NULL` のレコード同士で競合を検出できない。同一の root/period ノードを INSERT するたびに重複行が生成される。

**方針**: `date_from IS NULL` となるノードタイプ（`root`, `period`）は、`date_from` の代わりに別カラムで一意性を確保する。

- `root` ノード: `id` を `'{agent_id}:daily_log:root'` のような固定文字列とし、`INSERT OR REPLACE` で管理する（UNIQUE INDEX は `id` PRIMARY KEY が担う）
- `period` ノード: `date_from` に月初日（`YYYY-MM-01`）を格納することで NULL を回避する（例: 2026年2月 → `'2026-02-01'`）

これにより `idx_memory_index_nodes_key` による UNIQUE 制約が `root`・`period` ノードにも正しく機能する。

---

## 5. インポート・sync との統合

### 5.1 import execute 時のフック

```
POST /api/import/execute
  │
  ├─ [既存] memory_curated に daily_log 投入
  ├─ [既存] import_sync_state に記録
  └─ [追加] DailyLogIndexer を非同期起動（バックグラウンド）
             └─ 完了後: daily_log_index_watermark を更新
```

インポート自体のレスポンスはインデックス完了を待たない（非同期）。インデックス進捗は別途 API で確認可能にする。

### 5.2 sync 実行時のフック

```
POST /api/agents/{id}/import/sync
  │
  ├─ [既存] 差分検知（ハッシュ比較）
  ├─ [既存] 新規・更新ファイルを memory_curated に UPSERT
  └─ [追加] 更新された daily_log の日付リストを DailyLogIndexer に渡す
             └─ 対象日付のみ再インデックス（差分インデックス）
```

### 5.3 バックグラウンド自動インデックス

セッションログの memory_index と同様に、daily_log インデックスも閾値超過時に自動実行する：

```
条件: memory_curated の daily_log 件数 > daily_log_index_watermark の last_indexed_date より新しい件数
トリガー: import/sync 完了後 | エージェント起動時（未インデックス分がある場合）
実行: バックグラウンドタスク（Tokio spawn）
```

---

## 6. エージェントから見た検索インターフェース

### 6.1 browse_memory_index（変更なし、daily_log ツリーが追加される）

```json
// browse_memory_index の結果例（daily_log ツリー）
{
  "nodes": [
    {
      "id": "root-daily-log",
      "node_type": "root",
      "source_type": "daily_log",
      "title": "日次ログ アーカイブ",
      "children": [
        {
          "id": "period-2026-02",
          "node_type": "period",
          "source_type": "daily_log",
          "title": "2026年2月",
          "summary": "FTv9の学習と実装が中心。Nostrプロトコルとの統合も進んだ月。",
          "children": [
            {
              "id": "daily-2026-02-01",
              "node_type": "daily",
              "title": "2026-02-01",
              "summary": "FTv9学習開始。ownerさんとの長い議論でデータセット設計が固まった。"
            },
            ...
          ]
        }
      ]
    }
  ]
}
```

### 6.2 retrieve_memory_nodes（変更なし）

エージェントは `browse_memory_index` で目的の daily ノードを特定した後、`retrieve_memory_nodes` で全文を取得する：

```json
// retrieve_memory_nodes リクエスト
{
  "node_ids": ["daily-2026-02-01", "daily-2026-02-02"]
}

// レスポンス: memory_curated の content（元の daily_log 全文）
{
  "nodes": [
    {
      "id": "daily-2026-02-01",
      "content": "# 2026-02-01 エージェントC日記\n## Nostr会話（00:41〜）\n..."
    }
  ]
}
```

### 6.3 典型的な検索シナリオ

**シナリオ: 「2月にやってたプロジェクトを思い出したい」**

```
1. browse_memory_index(source_type="daily_log")
   → period ノード一覧とその要約を確認
   → "period-2026-02" の summary に "FTv9の学習と実装" を発見

2. browse_memory_index(node_id="period-2026-02")
   → 2月の daily ノード一覧を確認
   → "2026-02-11" の summary に "FTv9記憶消失事件" を発見

3. retrieve_memory_nodes(node_ids=["daily-2026-02-11"])
   → 該当日の全文を取得
   → 詳細なコンテキストをユーザーに返す
```

**コンテキスト消費比較**:

| 方法 | トークン消費 |
|---|---|
| 全 daily_log をシステムプロンプトに詰め込む | ~200K tokens（月60日分） |
| FTS5 全文検索（既存） | daily_log は対象外 |
| **本設計（browse → retrieve）** | **ツリー閲覧 ~2K + 取得対象 ~5K = ~7K tokens** |

---

## 7. 実装ステップ

### Phase 1: スキーマ拡張（DB マイグレーション）

```sql
-- memory_index_nodes に source_type カラム追加
ALTER TABLE memory_index_nodes 
    ADD COLUMN source_type TEXT NOT NULL DEFAULT 'session_log';

-- daily_log_index_watermark テーブル新設
CREATE TABLE IF NOT EXISTS daily_log_index_watermark (
    agent_id TEXT NOT NULL PRIMARY KEY,
    last_indexed_date TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- インデックス追加
CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_source_type
    ON memory_index_nodes (agent_id, source_type);
CREATE INDEX IF NOT EXISTS idx_memory_curated_daily_log
    ON memory_curated (agent_id, category)
    WHERE category LIKE 'daily_log/%';
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_key
    ON memory_index_nodes(agent_id, source_type, node_type, date_from);
```

### Phase 2: DailyLogIndexer 実装

```rust
// crates/server/src/memory/daily_log_indexer.rs

pub struct DailyLogIndexer {
    db: Arc<Mutex<Connection>>,
    llm_client: Arc<dyn LlmClient>,
}

impl DailyLogIndexer {
    /// インデックス未構築の daily_log を処理する
    pub async fn run(&self, agent_id: &str) -> Result<DailyLogIndexStats>;

    /// 特定の日付リストを強制再インデックス（sync フック用）
    pub async fn reindex_dates(&self, agent_id: &str, dates: &[NaiveDate]) -> Result<()>;

    // 内部
    async fn fetch_unindexed_logs(&self, agent_id: &str) -> Result<Vec<DailyLogEntry>>;
    async fn summarize_day(&self, date: NaiveDate, content: &str) -> Result<DaySummary>;
    async fn summarize_period(&self, year_month: &str, days: &[DaySummary]) -> Result<String>;
    async fn upsert_nodes(&self, agent_id: &str, summaries: &[DaySummary]) -> Result<()>;
    async fn update_watermark(&self, agent_id: &str, date: NaiveDate) -> Result<()>;
}

pub struct DailyLogIndexStats {
    pub days_indexed: usize,
    pub days_skipped: usize,
    pub periods_updated: usize,
    pub llm_calls: usize,
    pub tokens_used: usize,
}
```

### Phase 3: import/sync フック追加

```rust
// POST /api/import/execute ハンドラ内
// 既存の memory_curated 投入処理の後に追加:
let indexer = DailyLogIndexer::new(db.clone(), llm_client.clone());
tokio::spawn(async move {
    if let Err(e) = indexer.run(&agent_id).await {
        tracing::warn!("daily_log indexing failed: {}", e);
    }
});

// POST /api/agents/{id}/import/sync ハンドラ内
// 更新された daily_log の日付を収集して差分インデックス:
if !updated_daily_log_dates.is_empty() {
    let indexer = DailyLogIndexer::new(db.clone(), llm_client.clone());
    tokio::spawn(async move {
        let _ = indexer.reindex_dates(&agent_id, &updated_daily_log_dates).await;
    });
}
```

### Phase 4: browse_memory_index / retrieve_memory_nodes 拡張

- `browse_memory_index` に `source_type` パラメータ追加（オプション、省略時は全ツリー）
- `retrieve_memory_nodes` は `source_type = 'daily_log'` のノードの場合、`memory_curated` から `content` を引く（`log_id` ではなく `date` でルックアップ）

---

## 8. API 拡張

### 8.1 インデックス状態確認

```
GET /api/agents/{id}/daily-log-index/status

Response:
{
  "last_indexed_date": "2026-03-24",
  "total_indexed_days": 52,
  "unindexed_days": 1,
  "total_periods": 2
}
```

### 8.2 手動再インデックスのトリガー

```
POST /api/agents/{id}/daily-log-index/rebuild

Body: { "from_date": "2026-02-01" }  // 省略時は全再構築

Response:
{
  "task_id": "...",
  "status": "started"
}
```

---

## 9. エラーハンドリング

| ケース | 対処 |
|---|---|
| LLM 呼び出し失敗 | そのバッチをスキップ、エラーログ記録、次回実行で再試行 |
| コンテンツが大きすぎる（>4KB） | チャンク分割（2KB ずつ）→個別要約→マージ要約 |
| DB ロック（並行 sync） | Retry with exponential backoff（最大3回） |
| LLM 応答が JSON でない | Regex でフォールバック抽出、失敗時は title のみ保存 |

---

## 10. 関連ドキュメント

- `design-openclaw-import.md` — 初期インポート設計（daily_log の memory_curated 投入）
- `design-import-sync.md` — 増分同期設計（daily_log の差分検知・更新）
- `DESIGN.md` § 3 — Memory Index の基本アーキテクチャ（session_log ツリー）
- `DESIGN.md` § 9 — テーブル一覧

---

## 11. 未解決事項・今後の検討

| 項目 | 内容 |
|---|---|
| topic ノードの粒度 | 1日あたり何 topic まで生成するか（現状: 最大5件） |
| period の単位 | 月単位が基本。週単位オプションが必要か？ |
| 削除同期 | daily_log ファイルが削除された場合、memory_index_nodes も削除するか（現状: 保持） |
| FTS5 統合 | daily_log の topic ノードを FTS5 検索対象にするか（現状: browse のみ） |
| コスト上限 | LLM 要約コストが高騰した場合の safeguard（月次コスト上限など） |
