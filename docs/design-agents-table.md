# agents テーブル集約設計

## 背景
エージェント設定が `soul` / `identity` / `agent_discord_config` / `agent_memory_index_config` に分散しており、直感的でない。`agents` テーブルに集約し、エージェントごとのLLMモデル設定も追加する。

## 現状のテーブル

### soul
| Column | Type | Notes |
|--------|------|-------|
| agent_id | TEXT PK | |
| persona_name | TEXT NOT NULL | |
| personality | TEXT | |
| instructions | TEXT DEFAULT '' | |
| updated_at | TEXT NOT NULL | |

### identity
| Column | Type | Notes |
|--------|------|-------|
| agent_id | TEXT PK | |
| name | TEXT NOT NULL | |
| job_title | TEXT | |
| organization | TEXT | |
| image_url | TEXT | |
| metadata_json | TEXT | |
| updated_at | TEXT NOT NULL | |

### agent_discord_config
| Column | Type | Notes |
|--------|------|-------|
| agent_id | TEXT PK | |
| bot_token | TEXT NOT NULL | |
| owner_discord_id | TEXT DEFAULT '' | |
| enabled | INTEGER DEFAULT 1 | |
| updated_at | TEXT NOT NULL | |

### agent_memory_index_config
| Column | Type | Notes |
|--------|------|-------|
| agent_id | TEXT PK | |
| batch_size | INTEGER DEFAULT 50 | |
| threshold | INTEGER DEFAULT 20 | |
| updated_at | TEXT NOT NULL | |

## 新設計

### agents テーブル（新規）
`soul` と `identity` を統合。`model` カラムを追加。

```sql
CREATE TABLE agents (
    agent_id TEXT PRIMARY KEY,
    -- identity fields
    name TEXT NOT NULL,
    job_title TEXT,
    organization TEXT,
    image_url TEXT,
    -- soul fields
    persona_name TEXT NOT NULL,
    personality TEXT,
    instructions TEXT NOT NULL DEFAULT '',
    -- new: per-agent model
    model TEXT,  -- NULL = use default_model from config
    -- metadata
    metadata_json TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

### 残すテーブル（変更なし）
- **agent_discord_config**: Discord固有設定。分離が自然。
- **agent_memory_index_config**: メモリインデックス設定。分離が自然。

### 削除するテーブル
- **soul**: `agents` に統合
- **identity**: `agents` に統合

## マイグレーション SQL

```sql
-- 1. agents テーブル作成
CREATE TABLE agents (
    agent_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    job_title TEXT,
    organization TEXT,
    image_url TEXT,
    persona_name TEXT NOT NULL,
    personality TEXT,
    instructions TEXT NOT NULL DEFAULT '',
    model TEXT,
    metadata_json TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- 2. 既存データ移行（soul + identity を JOIN）
INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, metadata_json, updated_at)
SELECT
    s.agent_id,
    i.name,
    i.job_title,
    i.organization,
    i.image_url,
    s.persona_name,
    s.personality,
    s.instructions,
    i.metadata_json,
    MAX(s.updated_at, i.updated_at)
FROM soul s
JOIN identity i ON s.agent_id = i.agent_id;

-- 3. 旧テーブル削除
DROP TABLE soul;
DROP TABLE identity;
```

> **旧世代 DB の注意（#480）**: 最初期（2026-02・b6a145e）の `soul` は `personality` ではなく
> `personality_json`（構造化 JSON）を持ち、`personality` / `instructions` 列を欠く。この統合
> クエリは `s.personality` を読むため、そのままだと `no such column: s.personality` で起動不能に
> なる。実装では baseline `migrate()` が集約の直前に `soul.personality`（と `soul.instructions`）を
> 欠落時に `ALTER TABLE ... ADD COLUMN` で用意して塞ぐ。旧 `personality_json` は自由記述 TEXT の
> `personality` へ意味的対応が無いため `agents.personality` へは移送せず NULL のままにする。
>
> **JSON 列のデータ退避（#480）**: 集約は soul から `persona_name` / `personality` /
> `instructions` しか写さないため、残る JSON 列（`social_style_json` / `personality_json`=Big Five /
> `thinking_style_json`=自由記述 `description` を含む / `custom_traits_json`=利用者任意の JSON）は
> そのままだと `DROP TABLE soul` で消える。この修正が無いと、**「起動できない（データはディスク上に
> 無事）」から「起動できる（データは消える）」へ退行**し、オーナー原則「意図して設定した値を勝手に
> 破棄しない」（#456）に反する。よって DROP 前に、実在する JSON 列を
> `agents.metadata_json` の `legacy_soul` キー配下へ入れ子で退避する（不正 JSON / NULL でも
> `json_valid` 分岐で起動を止めない・identity 由来の既存 metadata も壊さない）。
> 以前ここで社会性/思考スタイル JSON を「dead code 削除」として先に個別 DROP していたのは、
> soul ごと DROP される前提の冗長操作だったため撤去した（退避を拾えるように）。回帰は
> `crates/db/src/schema.rs` の `old_db_generations()`（世代 `pre_personality_2026_02`）で固定。

## API 変更

### GET /api/agents/{id}
**変更前**: `soul` + `identity` を個別取得して結合
**変更後**: `agents` テーブルから直接取得

レスポンス:
```json
{
    "agent_id": "...",
    "name": "かいろ",
    "persona_name": "kairo",
    "personality": "...",
    "instructions": "...",
    "model": "claude-sonnet-4-6",
    "job_title": null,
    "organization": null,
    "image_url": null,
    "metadata_json": null
}
```

### PUT /api/agents/{id}
全フィールド更新。`model` が `null` の場合はグローバル `default_model` を使用。

### PATCH /api/agents/{id}
部分更新。送信されたフィールドのみ更新。

### 削除するエンドポイント
- `PUT /api/agents/{id}/soul` → `PATCH /api/agents/{id}` に統合
- `PUT /api/agents/{id}/identity` → `PATCH /api/agents/{id}` に統合
- `POST /api/agents/{id}/update-instructions` → `PATCH /api/agents/{id}` に統合

## ダッシュボード UI 変更

エージェント設定画面（`web/src/pages/AgentDetail.tsx` 等）に:
1. **Model セレクタ**: ドロップダウンで利用可能なモデル一覧から選択
2. **「デフォルトを使用」オプション**: model=null のとき表示

## コード影響範囲

### Rust crates
- `crates/db/src/queries.rs`: `get_soul`, `upsert_soul`, `get_identity`, `upsert_identity` → `get_agent`, `upsert_agent` に統合
- `crates/server/src/api/agents.rs`: soul/identity 個別エンドポイント → agents 統合エンドポイント
- `crates/server/src/api/import.rs`: インポート時の soul/identity 書き込み
- `crates/server/src/process.rs`: `build_agent_context()` での soul/identity 読み取り
- `crates/server/src/api/agents_messages.rs`: `default_model` 参照箇所 → agents.model 優先
- `crates/core/src/memory/`: memory_curated の agent_rules 等

### React UI
- `web/src/pages/AgentDetail.tsx`: soul/identity 個別フォーム → 統合フォーム
- `web/src/api/`: API クライアント関数
- `web/src/types/`: 型定義

## model フィールドの使用ロジック

```
let model = agent.model.unwrap_or(state.default_model.clone());
```

`agents.model` が NULL でなければそれを使用、NULL なら `config/default.toml` の `default_model` にフォールバック。
