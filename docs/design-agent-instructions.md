# 設計書: Agent Instructions フィールド追加

## 背景・問題

### OpenClawの仕組み
OpenClawはシステムプロンプトに `## Project Context` セクションを持ち、
SOUL.md / AGENTS.md / USER.md 等の複数ファイルを**毎回**注入する。

- **SOUL.md** → キャラクター・人格
- **AGENTS.md** → 操作ルール・行動指針（NO_REPLYの使い方、安全ルール等）

### opencrabの現状の問題
- `soul.personality` = SOUL.md（毎回システムプロンプトに反映）✅
- AGENTS.md → `memory_curated`（RAG検索でヒットした時のみ参照）❌

RAG依存のため、AGENTS.mdの「NO_REPLYを使う」「グループチャットで黙る」等の基本ルールが
常に参照されるとは限らない。これがかいろとcrabらぼみんのループの根本原因。

## 解決策: `instructions` フィールド追加

### 設計方針
- `soul.personality` と並列に `soul.instructions` を追加
- instructionsは**毎回**システムプロンプトに展開される（RAG不要）
- ユーザー/エージェント自身がダッシュボードで編集可能
- インポート時: SOUL.md → personality, AGENTS.md → instructions

### OpenClawとの対応
| OpenClaw | opencrab |
|----------|----------|
| SOUL.md (Project Context) | soul.personality |
| AGENTS.md (Project Context) | soul.instructions（新規） |

## DB変更

### `souls` テーブルにカラム追加
```sql
ALTER TABLE souls ADD COLUMN instructions TEXT NOT NULL DEFAULT '';
```

後方互換不要のため、既存データはinstructions=''でOK。

## API変更

### `GET /api/agents/{id}/soul`
```json
{
  "agent_id": "...",
  "persona_name": "...",
  "personality": "...",
  "instructions": ""  // 追加
}
```

### `PUT /api/agents/{id}/soul`
```json
{
  "persona_name": "...",
  "personality": "...",
  "instructions": "..."  // 追加
}
```

## システムプロンプトへの組み込み

`build_agent_context()` での展開順序:
```
[System]
{personality}  // キャラクター定義

## Instructions
{instructions}  // 操作ルール（AGENTS.md相当）

## Skills
...

## Silent Reply
NO_REPLY ...
```

instructionsが空の場合はセクション自体を省略する。

## ダッシュボードUI変更

SoulタブにInstructionsのテキストエリアを追加。
- ラベル: "Instructions (操作ルール)"
- プレースホルダー: "NO_REPLYの使い方、安全ルール、グループチャットの振る舞い等"
- personality と横並び or 下に配置

エージェント自身はAPIを直接呼び出す方法ではなく、`update_instructions` ゲートウェイアクションを通じてのみ編集できる。

## update_instructions ゲートウェイアクション

### 概要
instructionsを更新する専用のゲートウェイアクション。

### 制約（重要）
- **ownerからのメッセージへの返信時のみ実行可能**（`CallerIdentity::Owner`限定）
- 通常の会話中・グループチャット・サブタスク中は使用不可
- ownerが直接話しかけてきた文脈でのみ、エージェントが自分のinstructionsを更新できる

### 理由
- instructionsはエージェントの行動基盤となる重要な設定
- 任意の会話中に書き換えられると不正変更・プロンプトインジェクションのリスクがある
- owner承認が得られる文脈でのみ変更を許可することでセキュリティを確保

### アクション定義
```json
{
  "name": "update_instructions",
  "description": "自分のinstructionsを更新する（ownerへの返信時のみ使用可能）",
  "parameters": {
    "instructions": {
      "type": "string",
      "description": "新しいinstructionsの内容"
    },
    "reason": {
      "type": "string",
      "description": "更新する理由"
    }
  }
}
```

## インポート変更

`POST /api/import/execute` の処理を修正:
- SOUL.md → `personality` に格納（現状通り）
- AGENTS.md → `instructions` に格納（変更）
  - 現状: `memory_curated` の `agent_rules` カテゴリに入れている
  - 変更後: `instructions` に直接格納

AGENTS.md以外のルール系MDファイル（USER.md等）はinstructionsに追記 or memory_curatedのまま（要議論）。

## 移行計画

1. DB migration: `souls.instructions` カラム追加
2. API: GET/PUT soul エンドポイントにinstructionsを追加
3. `build_agent_context()`: instructions をシステムプロンプトに展開
4. ダッシュボードUI: InstructionsテキストエリアをSoulタブに追加
5. インポート: AGENTS.md → instructions に変更
6. crabらぼみん再インポート
7. かいろのinstructionsを手動 or 自律的に設定

## 未解決事項（レビューで議論）

1. **USER.mdはどこに入れるか？** instructionsに追記？memory_curatedのまま？
2. **instructionsの文字数制限** トークン節約のため上限設けるか？
3. **かいろの初期instructions** 誰が書くか（kojira？かいろ自身？）
4. **他のMDファイル（HEARTBEAT.md等）はどう扱うか**

## 実装コスト

- DB migration: 1ファイル
- API: soul.rs を修正
- process.rs: build_agent_context に3行追加程度
- web/: SoulタブにTextarea追加
- import.rs: AGENTS.md の格納先変更

合計: 中規模（5ファイル程度の変更）
