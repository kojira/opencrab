# 設計ドキュメント: サブタスク実行統計

> TODO #25
> ステータス: **設計確定**

---

## 1. 問題定義

### 1.1 現状の課題

OpenCrabのサブタスク機能（`spawn_subtask`）は日常的に使われているが、その実行状況を可視化する手段がない。

```
現在わからないこと:
- どのタスクが重いか（実行時間・ステップ数）
- どのツールが失敗しやすいか（外部API障害など）
- どのモデルが何のタスクに向いているか
- max_iterationsに詰まりやすいタスクタイプは何か
- 「同じタスクを何回もやり直している」という無駄が起きていないか
```

### 1.2 かいろ（エージェント当事者）からのフィードバック

以下は実際にサブタスクを実行しているエージェント（かいろ）が感じている問題点:

1. **外部API応答時間・失敗率**が一番困っている
   - `wttr.in`, `coinapi.io`, `duckduckgo` などが最近よく落ちている
   - `execute_shell` で `curl` を叩いた結果（exit_code, stderr）から集計すべき
   - API障害を「運悪くエラーになった」ではなく「このAPIは信頼性が低い」として可視化

2. **ツール別の失敗パターン**の方が使用頻度統計より行動につながる
   - DNSの問題、タイムアウト、認証エラーなどをパターン分類できる
   - exit_code=6（curl: DNS問題）などから問題を検出できる

3. **タスク完了までのステップ数分布**
   - 何イテレーションで完了できているかを把握したい
   - 多すぎるタスクは設計見直しの指標になる

4. **「最大ステップ到達」の頻度**
   - `stopped_by_limit` になった回数・パターンが重要
   - どのタスクタイプで詰まりやすいか可視化できる

5. **モデル別タスク成功率**
   - モデル選択の判断材料がほしい
   - データドリブンで「このモデルはこの種類のタスクが得意」を示す

### 1.3 設計原則

- **Phase 1: 既存データの集計クエリで実現**（DBスキーマ変更なし）
- `memory_sessions` + `sessions` のデータから取得できるものに絞る
- Phase 2でダッシュボードUI追加

---

## 2. 既存データ構造

### 2.1 テーブル構造

#### `sessions` テーブル

```sql
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL DEFAULT 'facilitated',  -- "subtask" がサブタスク
    theme TEXT NOT NULL,                         -- タスク内容（spawn_subtaskのtask引数）
    status TEXT NOT NULL DEFAULT 'active',
    metadata_json TEXT,                          -- JSON: parent_session_id, subtask_id, depth など
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

`metadata_json` の例（spawn_subtask時に記録）:

```json
{
  "parent_session_id": "main-session-uuid",
  "subtask_id": "uuid",
  "depth": 1,
  "created_at": "2026-03-22T10:00:00Z"
}
```

#### `memory_sessions` テーブル（実際のsession_logs）

```sql
CREATE TABLE IF NOT EXISTS memory_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    log_type TEXT NOT NULL,   -- "system", "user", "assistant", "tool_result" など
    content TEXT NOT NULL,
    speaker_id TEXT,
    turn_number INTEGER,
    metadata_json TEXT,       -- JSON: typeフィールドで詳細を分類
    created_at TEXT NOT NULL
);
```

### 2.2 サブタスク関連イベント（metadata_json の type フィールド）

| type | 記録先 session_id | 内容 |
|------|-------------------|------|
| `subtask_spawned` | 親セッション | サブタスクの起動を記録 |
| `subtask_progress` | 親セッション | サブの進捗レポート |
| `subtask_completed` | 親セッション | 完了（exit_reason: "completed"/"stopped_by_limit"） |
| `subtask_cancelled` | 親セッション | キャンセル |

`subtask_completed` の metadata_json 例:

```json
{
  "type": "subtask_completed",
  "subtask_id": "uuid",
  "result": "タスク完了...",
  "exit_reason": "completed"
}
```

`exit_reason` の値:
- `"completed"` — 正常完了
- `"stopped_by_limit"` — max_iterations 到達（タイムアウト扱い）
- `"cancelled"` — キャンセル

### 2.3 execute_shell のツール実行結果

`memory_sessions` の `log_type = "tool_result"` で記録される content の例:

```json
{
  "tool_name": "execute_shell",
  "result": {
    "stdout": "...",
    "stderr": "curl: (6) Could not resolve host: wttr.in",
    "exit_code": 6,
    "truncated": false
  }
}
```

`exit_code` の意味（curl）:

| exit_code | 意味 |
|-----------|------|
| 0 | 成功 |
| 6 | DNSエラー（ホスト解決失敗） |
| 7 | 接続拒否 |
| 22 | HTTPエラー（4xx/5xx） |
| 28 | タイムアウト |
| -1 | プロセス異常終了 |

---

## 3. 集計クエリ設計

### 3.1 サブタスク実行統計（subtask stats）

#### クエリ 1: exit_reason 別の件数と実行時間

```sql
-- サブタスクの exit_reason 別集計
WITH subtask_events AS (
  SELECT
    ms.session_id,
    ms.agent_id,
    json_extract(ms.metadata_json, '$.exit_reason') AS exit_reason,
    json_extract(ms.metadata_json, '$.subtask_id') AS subtask_id,
    ms.created_at AS completed_at
  FROM memory_sessions ms
  WHERE ms.agent_id = ?1
    AND json_extract(ms.metadata_json, '$.type') IN ('subtask_completed', 'subtask_cancelled')
),
subtask_spawned AS (
  SELECT
    json_extract(ms.metadata_json, '$.subtask_id') AS subtask_id,
    ms.created_at AS spawned_at
  FROM memory_sessions ms
  WHERE ms.agent_id = ?1
    AND json_extract(ms.metadata_json, '$.type') = 'subtask_spawned'
)
SELECT
  COALESCE(e.exit_reason, 'cancelled') AS exit_reason,
  COUNT(*) AS count,
  AVG(
    (julianday(e.completed_at) - julianday(s.spawned_at)) * 86400.0
  ) AS avg_duration_secs,
  MAX(
    (julianday(e.completed_at) - julianday(s.spawned_at)) * 86400.0
  ) AS max_duration_secs
FROM subtask_events e
LEFT JOIN subtask_spawned s ON e.subtask_id = s.subtask_id
GROUP BY exit_reason;
```

#### クエリ 2: ステップ数分布（イテレーション回数）

サブセッション内のアシスタント発言数 ≒ イテレーション数として近似:

```sql
-- サブセッション別のステップ数
SELECT
  s.id AS session_id,
  s.theme AS task_theme,
  json_extract(s.metadata_json, '$.depth') AS depth,
  COUNT(ms.id) AS step_count,
  s.created_at,
  s.updated_at
FROM sessions s
JOIN memory_sessions ms ON ms.session_id = s.id
WHERE s.agent_id = ?1
  AND s.mode = 'subtask'
  AND ms.agent_id = ?1
  AND ms.log_type = 'assistant'
GROUP BY s.id
ORDER BY step_count DESC;
```

#### クエリ 3: ステップ数の分布（ヒストグラム用）

```sql
SELECT
  CASE
    WHEN step_count <= 3 THEN '1-3'
    WHEN step_count <= 7 THEN '4-7'
    WHEN step_count <= 15 THEN '8-15'
    ELSE '16+'
  END AS range,
  COUNT(*) AS count
FROM (
  SELECT COUNT(ms.id) AS step_count
  FROM sessions s
  JOIN memory_sessions ms ON ms.session_id = s.id
  WHERE s.agent_id = ?1
    AND s.mode = 'subtask'
    AND ms.log_type = 'assistant'
  GROUP BY s.id
) sub
GROUP BY range;
```

#### クエリ 4: max_iterations 到達率

```sql
SELECT
  COUNT(*) AS total_subtasks,
  SUM(CASE WHEN json_extract(ms.metadata_json, '$.exit_reason') = 'stopped_by_limit' THEN 1 ELSE 0 END) AS stopped_by_limit_count,
  ROUND(
    100.0 * SUM(CASE WHEN json_extract(ms.metadata_json, '$.exit_reason') = 'stopped_by_limit' THEN 1 ELSE 0 END)
    / COUNT(*), 1
  ) AS stopped_by_limit_pct
FROM memory_sessions ms
WHERE ms.agent_id = ?1
  AND json_extract(ms.metadata_json, '$.type') IN ('subtask_completed', 'subtask_cancelled');
```

### 3.2 ツール使用統計（tool stats）

#### クエリ 5: ツール別の呼び出し回数・失敗率

tool_result の content から tool_name と exit_code を抽出:

```sql
SELECT
  json_extract(ms.content, '$.tool_name') AS tool_name,
  COUNT(*) AS total_calls,
  SUM(CASE WHEN COALESCE(json_extract(ms.content, '$.result.exit_code'), 0) != 0 THEN 1 ELSE 0 END) AS error_calls,
  ROUND(
    100.0 * SUM(CASE WHEN COALESCE(json_extract(ms.content, '$.result.exit_code'), 0) != 0 THEN 1 ELSE 0 END)
    / COUNT(*), 1
  ) AS error_rate_pct
FROM memory_sessions ms
JOIN sessions s ON ms.session_id = s.id
WHERE ms.agent_id = ?1
  AND ms.log_type = 'tool_result'
  AND s.mode = 'subtask'
  AND json_extract(ms.content, '$.tool_name') IS NOT NULL
GROUP BY tool_name
ORDER BY total_calls DESC;
```

#### クエリ 6: execute_shell の exit_code 別集計（外部API失敗の可視化）

```sql
SELECT
  json_extract(ms.content, '$.result.exit_code') AS exit_code,
  COUNT(*) AS count,
  -- stderr からURLやホスト名を抽出してパターン分類
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%wttr.in%' THEN 1 ELSE 0 END) AS wttr_in_errors,
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%coinapi%' THEN 1 ELSE 0 END) AS coinapi_errors,
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%duckduckgo%' THEN 1 ELSE 0 END) AS duckduckgo_errors,
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%Could not resolve%' THEN 1 ELSE 0 END) AS dns_errors,
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%timed out%' THEN 1 ELSE 0 END) AS timeout_errors
FROM memory_sessions ms
WHERE ms.agent_id = ?1
  AND ms.log_type = 'tool_result'
  AND json_extract(ms.content, '$.tool_name') = 'execute_shell'
  AND json_extract(ms.content, '$.result.exit_code') != 0
GROUP BY exit_code
ORDER BY count DESC;
```

#### クエリ 7: 外部API別の成功/失敗率（時系列）

```sql
-- 直近30日の外部API失敗率（日別）
SELECT
  date(ms.created_at) AS day,
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%wttr.in%' AND json_extract(ms.content, '$.result.exit_code') != 0 THEN 1 ELSE 0 END) AS wttr_failures,
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%coinapi%' AND json_extract(ms.content, '$.result.exit_code') != 0 THEN 1 ELSE 0 END) AS coinapi_failures,
  SUM(CASE WHEN json_extract(ms.content, '$.result.stderr') LIKE '%duckduckgo%' AND json_extract(ms.content, '$.result.exit_code') != 0 THEN 1 ELSE 0 END) AS duckduckgo_failures,
  COUNT(*) AS total_shell_calls
FROM memory_sessions ms
WHERE ms.agent_id = ?1
  AND ms.log_type = 'tool_result'
  AND json_extract(ms.content, '$.tool_name') = 'execute_shell'
  AND ms.created_at >= date('now', '-30 days')
GROUP BY day
ORDER BY day DESC;
```

### 3.3 モデル別統計

`llm_logs` テーブルとサブセッションを JOIN して集計:

#### クエリ 8: モデル別タスク成功率

```sql
SELECT
  ll.model,
  COUNT(DISTINCT ll.session_id) AS total_sessions,
  SUM(CASE WHEN json_extract(ms.metadata_json, '$.exit_reason') = 'completed' THEN 1 ELSE 0 END) AS completed,
  SUM(CASE WHEN json_extract(ms.metadata_json, '$.exit_reason') = 'stopped_by_limit' THEN 1 ELSE 0 END) AS stopped_by_limit,
  ROUND(
    100.0 * SUM(CASE WHEN json_extract(ms.metadata_json, '$.exit_reason') = 'completed' THEN 1 ELSE 0 END)
    / COUNT(DISTINCT ll.session_id), 1
  ) AS success_rate_pct
FROM llm_logs ll
JOIN memory_sessions ms ON ms.session_id = ll.session_id
WHERE ll.agent_id = ?1
  AND json_extract(ms.metadata_json, '$.type') = 'subtask_completed'
GROUP BY ll.model
ORDER BY success_rate_pct DESC;
```

---

## 4. APIエンドポイント設計

### 4.1 エンドポイント一覧

| Method | Path | 説明 |
|--------|------|------|
| GET | `/api/agents/{id}/stats/subtasks` | サブタスク実行統計 |
| GET | `/api/agents/{id}/stats/tools` | ツール使用統計 |

### 4.2 `GET /api/agents/{id}/stats/subtasks`

**クエリパラメータ:**

| パラメータ | 型 | デフォルト | 説明 |
|----------|-----|--------|------|
| `days` | integer | 30 | 集計対象の日数 |
| `depth` | integer | - | サブタスクの深さでフィルタ（省略時は全深さ） |

**レスポンス例:**

```json
{
  "period_days": 30,
  "summary": {
    "total": 142,
    "completed": 118,
    "stopped_by_limit": 19,
    "cancelled": 5,
    "stopped_by_limit_pct": 13.4,
    "avg_duration_secs": 187.3,
    "max_duration_secs": 1782.0
  },
  "step_distribution": {
    "1-3": 34,
    "4-7": 61,
    "8-15": 38,
    "16+": 9
  },
  "duration_percentiles": {
    "p50": 142.0,
    "p90": 480.0,
    "p99": 1200.0
  },
  "stopped_by_limit_tasks": [
    {
      "session_id": "subtask-uuid",
      "theme": "Rustコードのリファクタリング",
      "step_count": 20,
      "duration_secs": 1782.0
    }
  ]
}
```

### 4.3 `GET /api/agents/{id}/stats/tools`

**クエリパラメータ:**

| パラメータ | 型 | デフォルト | 説明 |
|----------|-----|--------|------|
| `days` | integer | 30 | 集計対象の日数 |
| `tool` | string | - | 特定ツールでフィルタ（省略時は全ツール） |

**レスポンス例:**

```json
{
  "period_days": 30,
  "tool_stats": [
    {
      "tool_name": "execute_shell",
      "total_calls": 834,
      "error_calls": 47,
      "error_rate_pct": 5.6
    },
    {
      "tool_name": "read_file",
      "total_calls": 412,
      "error_calls": 3,
      "error_rate_pct": 0.7
    }
  ],
  "external_api_stats": {
    "by_api": [
      {
        "api_name": "wttr.in",
        "total_calls": 89,
        "failures": 12,
        "failure_rate_pct": 13.5
      },
      {
        "api_name": "coinapi.io",
        "total_calls": 34,
        "failures": 8,
        "failure_rate_pct": 23.5
      },
      {
        "api_name": "duckduckgo",
        "total_calls": 67,
        "failures": 4,
        "failure_rate_pct": 6.0
      }
    ],
    "by_error_type": [
      {
        "error_type": "dns_error",
        "exit_code": 6,
        "count": 15
      },
      {
        "error_type": "timeout",
        "exit_code": 28,
        "count": 9
      },
      {
        "error_type": "http_error",
        "exit_code": 22,
        "count": 23
      }
    ]
  },
  "model_stats": [
    {
      "model": "claude-sonnet-4-5",
      "total_subtasks": 89,
      "success_rate_pct": 88.8,
      "avg_steps": 6.2
    },
    {
      "model": "gpt-4o",
      "total_subtasks": 31,
      "success_rate_pct": 77.4,
      "avg_steps": 8.1
    }
  ]
}
```

---

## 5. 実装計画

### 5.1 Phase 1: APIエンドポイント実装（スキーマ変更なし）

#### 優先度順

1. **外部API失敗率（最優先）**
   - クエリ 6, 7 を実装
   - かいろが最も困っている問題を解決
   - `execute_shell` の stderr パターンマッチングで API を識別

2. **ツール別失敗率**
   - クエリ 5 を実装
   - 全ツールの error_rate_pct を表示

3. **サブタスク完了統計**
   - クエリ 1, 4 を実装
   - exit_reason 別の件数・実行時間

4. **ステップ数分布**
   - クエリ 2, 3 を実装
   - ヒストグラム用データ

5. **モデル別成功率**
   - クエリ 8 を実装
   - llm_logs との JOIN が必要

#### 実装場所

```
crates/server/src/api/
├── agents.rs          -- 既存（変更なし）
├── stats.rs           -- 新規追加
└── mod.rs             -- ルーティング追加
```

`stats.rs` に以下を実装:

```rust
pub async fn get_subtask_stats(
    Path(agent_id): Path<String>,
    Query(params): Query<SubtaskStatsParams>,
    State(state): State<AppState>,
) -> Json<SubtaskStatsResponse> { ... }

pub async fn get_tool_stats(
    Path(agent_id): Path<String>,
    Query(params): Query<ToolStatsParams>,
    State(state): State<AppState>,
) -> Json<ToolStatsResponse> { ... }
```

`lib.rs` のルーター追加:

```rust
.route("/api/agents/:id/stats/subtasks", get(stats::get_subtask_stats))
.route("/api/agents/:id/stats/tools", get(stats::get_tool_stats))
```

### 5.2 Phase 2: ダッシュボードUI（将来）

#### 予定機能

- **概要カード**: 直近30日の completed/stopped_by_limit/cancelled 件数
- **外部API信頼性チャート**: API別の失敗率（棒グラフ）
- **ステップ数ヒストグラム**: タスクの複雑さ分布
- **モデル比較ビュー**: モデル別の成功率・平均ステップ数（テーブル or レーダーチャート）
- **問題タスク一覧**: stopped_by_limit になったタスクのリスト（再試行ボタン付き）

#### 技術的考慮事項

- Web UIは `web/` ディレクトリ（既存）に追加
- チャートライブラリ: Chart.js または recharts
- リアルタイム更新は不要（ページロード時の一回取得で十分）

---

## 6. 制約と注意事項

### 6.1 クエリパフォーマンス

`memory_sessions` テーブルは大量データになりうる。以下のインデックスが既に存在する:

```sql
CREATE INDEX IF NOT EXISTS idx_memory_sessions_agent ON memory_sessions(agent_id);
CREATE INDEX IF NOT EXISTS idx_memory_sessions_session ON memory_sessions(agent_id, session_id);
```

JSON 関数（`json_extract`）はインデックスが効かないため、大量データの場合はクエリが遅くなる可能性がある。対策:
- `days` パラメータで期間を絞る（デフォルト30日）
- 必要に応じて `created_at` の範囲条件を追加

### 6.2 外部APIの識別方法

現状は stderr のパターンマッチングで API を識別している。これは脆弱だが、Phase 1 では許容する。

将来的には `execute_shell` の呼び出し時にコマンド文字列をパースして、どの URL/ホストにアクセスしているかを記録する仕組みが望ましい。

### 6.3 tool_name の取得

現在の `memory_sessions` の `content` は `tool_result` 型の場合に `tool_name` フィールドを含む JSON として記録されているが、実装によっては文字列形式が異なる可能性がある。実装時に実際のデータ形式を確認すること。

### 6.4 モデル情報の紐付け

`llm_logs` テーブルには `session_id` カラムがあるが、すべての呼び出しに session_id が記録されているとは限らない。JOIN 時に NULL を含む場合があることを考慮すること。

---

## 7. 検討した代替案

### 7.1 専用の統計テーブルを追加する案

```sql
-- 却下した案
CREATE TABLE subtask_stats (
    subtask_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    parent_session_id TEXT,
    exit_reason TEXT,
    step_count INTEGER,
    duration_secs REAL,
    ...
);
```

**却下理由**: Phase 1 では既存データから取得できるため不要。データの二重管理になりスキーマが複雑になる。将来的にパフォーマンス問題が顕在化した場合に検討。

### 7.2 エンジン側でリアルタイム記録する案

`spawn_subtask` 完了時に `step_count` や `duration_secs` を metadata_json に記録する。

**状態**: Phase 2 候補。現状の `subtask_completed` イベントには result と exit_reason しか記録されていない。Phase 1 では既存データの集計クエリで代替できるため見送り。実装時にはほぼコスト不要で追加できる。

---

## 8. 関連ドキュメント

- [design-subtask-delegation.md](./design-subtask-delegation.md) — サブタスク委譲アーキテクチャ
- [design-resume-subtask.md](./design-resume-subtask.md) — サブセッション Resume 機能（TODO #24）
- [DESIGN.md](./DESIGN.md) — OpenCrab アーキテクチャ概要

---

## 付録: データ確認クエリ

開発時のデータ確認用（SQLite CLI で実行）:

```sql
-- サブタスクセッション一覧
SELECT id, theme, metadata_json FROM sessions WHERE mode = 'subtask' LIMIT 10;

-- subtask_completed イベント確認
SELECT session_id, content, metadata_json, created_at 
FROM memory_sessions 
WHERE json_extract(metadata_json, '$.type') = 'subtask_completed'
LIMIT 10;

-- execute_shell のエラー一覧
SELECT 
  ms.session_id,
  json_extract(ms.content, '$.result.exit_code') AS exit_code,
  json_extract(ms.content, '$.result.stderr') AS stderr,
  ms.created_at
FROM memory_sessions ms
WHERE ms.log_type = 'tool_result'
  AND json_extract(ms.content, '$.tool_name') = 'execute_shell'
  AND json_extract(ms.content, '$.result.exit_code') != 0
ORDER BY ms.created_at DESC
LIMIT 20;
```
