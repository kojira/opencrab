# スキルシステム再設計 v2

**作成日:** 2026-03-22  
**ステータス:** Draft  
**対象バージョン:** opencrab（現行 main ブランチ）

---

## 1. 現状の問題

### 1.1 `executable` スキルの設計的欠陥

現在のスキルシステムには `skill_type` として `"experience"` と `"executable"` の2種類がある。

`executable` スキルの動作フロー：

```
DB の skills.code フィールドに固定シェルコマンドを保存
         ↓
LLM が execute_skill アクションを呼ぶ
         ↓
gateway が code フィールドを取り出して sh -c で実行
         ↓
結果を返す
```

**問題点：**

1. **パラメータが固定になる**  
   `code` フィールドは静的な文字列。「天気を調べて」なら `curl wttr.in/Tokyo` は書けるが、「大阪の天気を調べて」には対応できない。都市名を動的に埋め込む仕組みがない。

2. **LLM が考える余地がない**  
   LLM は `execute_skill(skill_name="weather")` を呼ぶだけ。コマンドをどう組み立てるかを考えるのではなく、ただ「ボタンを押す」だけになっている。本来 LLM が得意な「文脈を読んで動的にコマンドを構成する」能力を活かせていない。

3. **`execute_shell` が既にある**  
   `execute_shell` アクションは allowed list に登録されたコマンドを LLM が任意の引数で呼べる設計になっている。これが「LLM が動的にコマンドを組み立てる」ための正しい経路。`executable` スキルはこれと役割が重複しており、劣化版になっている。

4. **スキルの本来の役割との乖離**  
   `experience` スキル（`description` + `guidance` テキストのみ）は「LLM への知識注入」として正しく機能している。しかし `executable` スキルは「コードのラッパー」として機能しており、スキルというより設定可能なボタンになっている。

### 1.2 `build_context()` の現状

`skill.rs` の `build_context()` は executable スキルに対して：

```
実行方法: `execute_skill` アクションで skill_name="XXX" を指定
```

とプロンプトに注入している。これにより LLM は「固定コマンドを起動する」という思考に誘導されてしまう。

---

## 2. 新しい設計方針

### 2.1 基本思想：スキルは「LLM への知識注入」

スキルは **LLM が何かを「できるようになる」ための知識** として機能すべき。

```
スキル = 「このツール/コマンドはこういうもので、こう使う」という知識
         ↓ LLM のプロンプトに注入
LLM が execute_shell や他のアクションを使って動的に実行
```

Claude Code の SKILL.md がまさにこのパターン。SKILL.md には「こういう場合はこうするべし」が書いてあり、AI がそれを読んで自分でコマンドを組み立てる。

### 2.2 `executable` スキルの廃止と統合

`skill_type` を `"experience"` 一本に統一する。`"executable"` という区分をなくし、すべてのスキルを「LLM への説明文」として扱う。

**変更後のスキルの役割：**

| フィールド | 役割 |
|---|---|
| `name` | スキルの識別名 |
| `description` | このスキルが何をするか（LLM に伝える） |
| `guidance` | どうやって使うか（コマンド例・手順など） |
| `code` | （後方互換用）使い方の例として `guidance` に移行 |

`guidance` フィールドに「どのコマンドを使うか」「どんな引数を渡すか」「どんな場合に使うか」を自由に書く。LLM はそれを読んで `execute_shell` を自分で呼ぶ。

### 2.3 `execute_skill` gateway action の廃止

`execute_skill` は固定コードを実行するためのアクションなので、新設計では不要になる。

**廃止方針：**

- 段階的廃止（deprecation）: まず deprecation notice をつけて残す
- 最終的に削除: 既存 `executable` スキルの移行が完了したら削除
- LLM へのプロンプト: `execute_skill` を使うよう誘導する文言を `build_context()` から削除

---

## 3. 変更範囲の特定

### 3.1 変えること

| 対象 | 変更内容 |
|---|---|
| `skill.rs` の `build_context()` | executable スキルの「実行方法: execute_skill で...」の文言を削除。代わりに `guidance` をそのまま表示 |
| `build_context()` の出力形式 | skill_type による分岐をなくし、すべて同じフォーマットで表示 |
| `acquire_executable_skill()` | 廃止（または `acquire_skill()` にリダイレクト） |
| `add_skill` API endpoint | `skill_type` フィールドを `"experience"` 固定にするか、フィールドを削除 |
| `execute_skill` gateway action | deprecated マーク → 将来削除 |

### 3.2 残すこと（すぐには変えない）

| 対象 | 理由 |
|---|---|
| DB の `skill_type` カラム | 後方互換性（既存データがある） |
| DB の `code` カラム | 後方互換性。移行期間中は `guidance` の補完として使う |
| `experience` スキルの全動作 | 現行のまま変更なし |
| `execute_shell` action | これが正しい経路。変更不要 |
| `SkillRow` 構造体の `code`, `skill_type` フィールド | 後方互換とマイグレーション用に保持 |

### 3.3 `code` フィールドの扱い

**移行期間中の扱い：**

1. `code` フィールドが存在する既存スキルは、`build_context()` でその内容を `guidance` として表示する
2. 「`execute_skill` で実行せよ」という文言は削除し、「参考コマンド例：」として表示する
3. LLM は参考にしつつ `execute_shell` で動的に実行する

**最終形：**

- 新しく作るスキルに `code` は使わない
- `guidance` に「使い方」「コマンド例」を書く

---

## 4. スキルのデータモデル変更案

### 4.1 DB スキーマ（`crates/db`）

現行スキーマ（`skills` テーブル）：

```sql
CREATE TABLE skills (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    situation_pattern TEXT NOT NULL DEFAULT '',
    guidance TEXT NOT NULL DEFAULT '',
    source_type TEXT NOT NULL,
    source_context TEXT,
    file_path TEXT,
    effectiveness REAL,
    usage_count INTEGER NOT NULL DEFAULT 0,
    is_active INTEGER NOT NULL DEFAULT 1,
    permission TEXT NOT NULL DEFAULT '"agent"',
    archived INTEGER NOT NULL DEFAULT 0,
    skill_type TEXT NOT NULL DEFAULT 'experience',
    code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

**変更案：**

- `skill_type` カラム：デフォルト値を `'experience'` のまま維持（後方互換）、アプリ側で `'executable'` を作らないようにする
- `code` カラム：NULL 許容のまま維持。ただし新規作成 API からは受け付けない
- スキーマ自体は変更しない（マイグレーション不要）

### 4.2 `SkillRow` 構造体（`crates/db/src/queries.rs`）

変更なし。後方互換を維持。

### 4.3 `Skill` 構造体（`crates/core/src/skill.rs`）

`skill_type` と `code` フィールドは残すが、新規取得時の扱いを変える：

```rust
// 変更前
if let Some(ref code) = skill.code {
    ctx.push_str(&format!("Code: `{}`\n", code));
    ctx.push_str(&format!("実行方法: `execute_skill` アクションで skill_name=\"{}\" を指定\n", skill.name));
}

// 変更後
if let Some(ref code) = skill.code {
    ctx.push_str(&format!("参考コマンド例: `{}`\n", code));
    ctx.push_str("execute_shell アクションで上記を参考に動的にコマンドを組み立てて実行すること\n");
}
```

### 4.4 API レイヤー（`crates/server/src/api/skills.rs`）

`AddSkillRequest` から `skill_type` と `code` フィールドを受け付けないようにする（または明示的に無視して `experience` 固定にする）：

```rust
// 変更前
pub struct AddSkillRequest {
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
    pub permission: Option<String>,
}

// 変更後（code / skill_type を追加しない方針を明示）
pub struct AddSkillRequest {
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,      // ← ここにコマンド使用例も書く
    pub permission: Option<String>,
    // skill_type は常に "experience" 固定
    // code フィールドは受け付けない
}
```

---

## 5. LLM へのコンテキスト注入方法

### 5.1 現在の注入タイミング

`engine.rs` → `SkillManager::build_context()` → システムプロンプトに追記

### 5.2 変更後の注入形式

**変更後の `build_context()` 出力イメージ：**

```
## Available Skills

### weather-check (used 5 times)
東京や大阪など日本の都市の天気情報を取得する。

Guidance:
  `curl wttr.in/<都市名>?format=3` を execute_shell で実行する。
  例: 東京なら `curl wttr.in/Tokyo?format=3`、大阪なら `curl wttr.in/Osaka?format=3`
  ユーザーが都市名を言ったら、その都市名を引数に使うこと。

### search-nostr-events (used 2 times)
Nostr ネットワーク上のイベントを検索する。

Guidance:
  `nostr-tools` コマンドを execute_shell で実行する。
  クエリパラメータをユーザーの要求から動的に組み立てること。
```

**ポイント：**

- スキル名・用途・`guidance` をそのまま表示
- 「execute_skill で実行せよ」という指示を完全に削除
- `guidance` に「どのコマンドを使うか」「引数の組み立て方」を書くことを規約にする
- `code` が残っている旧スキルは「参考コマンド例」として表示し、そのまま固定実行ではなく動的適用を促す

### 5.3 `guidance` の書き方ガイドライン（ドキュメント規約）

スキルの `guidance` には以下を書くことを推奨する：

```
1. 使うコマンド・ツール名
2. 基本的な呼び出しパターン（コマンド例）
3. パラメータの動的埋め込み方針（「ユーザーの入力から〇〇を取得して引数に使う」）
4. 注意事項・エラー時の対処
```

**良い guidance の例：**

```
`curl https://api.example.com/search?q=<クエリ>` を execute_shell で実行。
クエリはユーザーの質問から自然に抽出すること。
スペースは %20 でエンコード。
エラー時は 30 秒待って再試行。
```

---

## 6. 後方互換性と移行方針

### 6.1 既存 `executable` スキルの扱い

既存の `skill_type='executable'` スキルは DB に残る。`execute_skill` gateway action も当面は残す。

**動作の変化：**

| ケース | 変更前 | 変更後 |
|---|---|---|
| LLM が `execute_skill` を呼ぶ | code をそのまま実行 | 引き続き動作（deprecated だが機能する） |
| LLM が `build_context()` を読む | 「execute_skill で実行せよ」と書いてある | 「参考コマンド例」として表示、動的実行を促す |
| 新規スキル作成 | `executable` 作成可能 | API が `experience` 固定で作成 |

### 6.2 マイグレーション手順

既存 `executable` スキルを手動で `experience` に移行するスクリプト（参考）：

```sql
-- code フィールドの内容を guidance に統合
UPDATE skills
SET guidance = CASE
    WHEN guidance = '' THEN 'コマンド例: ' || code
    ELSE guidance || chr(10) || 'コマンド例: ' || code
END,
skill_type = 'experience'
WHERE skill_type = 'executable' AND code IS NOT NULL;
```

このスクリプトは移行完了確認後に手動実行。自動マイグレーションには含めない。

### 6.3 `execute_skill` gateway action の廃止スケジュール

| フェーズ | 内容 | タイミング |
|---|---|---|
| Phase 1 | `build_context()` の文言変更・`deprecated` ログ追加 | 即時 |
| Phase 2 | `acquire_executable_skill()` を非推奨化 | Phase 1 完了後 |
| Phase 3 | 既存 executable スキルの手動マイグレーション | 利用状況確認後 |
| Phase 4 | `execute_skill` action 削除・`code` カラムの drop | 全移行完了後 |

---

## 7. 実装ステップ（優先順位付き）

### P0（即時対応 - コア修正）

**7.1 `build_context()` の文言変更**  
ファイル: `crates/core/src/skill.rs`

- `executable` チェックを削除
- 「実行方法: execute_skill アクションで...」の文言を削除
- `code` フィールドがある場合は「参考コマンド例：」として表示
- `execute_shell` を使った動的実行を促す文言に変更

**7.2 `add_skill` API の `skill_type` 固定化**  
ファイル: `crates/server/src/api/skills.rs`

- `AddSkillRequest` に `skill_type`/`code` フィールドを追加しない方針を明示
- 既存の `add_skill` handler が常に `skill_type: "experience"` で登録するよう確認（現状すでに "experience" 固定になっている → 変更不要）

### P1（近期 - 廃止処理）

**7.3 `acquire_executable_skill()` に deprecated 警告追加**  
ファイル: `crates/core/src/skill.rs`

- `#[deprecated]` アトリビュート付与
- 内部で `tracing::warn!` を出す
- 呼び出し箇所をすべて `acquire_skill()` + guidance への説明記述に置き換える

**7.4 `execute_skill` gateway action に deprecation notice**  
ファイル: `crates/discord/src/gateway_actions.rs`

- description に「非推奨: execute_shell を使って直接コマンドを実行すること」と追記
- 実行時に `tracing::warn!` を出力

### P2（中期 - クリーンアップ）

**7.5 既存 `executable` スキルのマイグレーション**  

- マイグレーション SQL の作成とテスト
- ステージング環境で検証後、本番適用

**7.6 `execute_skill` action の削除**  
移行完了を確認後、`execute_skill` を `gateway_actions.rs` から完全削除。

**7.7 `skill_type`/`code` カラムの整理**  

- `skill_type` カラムは残す（`experience` のみになる）か、段階的に drop
- `code` カラムは `guidance` に統合後に drop

### P3（長期 - 構造整理）

**7.8 `Skill` 構造体の整理**  
`skill_type` フィールドと `code` フィールドを削除。`SkillSource` の `Standard`/`Acquired` 区分も見直し。

**7.9 `acquire_executable_skill()` の完全削除**

---

## 8. まとめ

| 変更点 | 優先度 | 工数感 |
|---|---|---|
| `build_context()` 文言変更 | P0 | 小（数行） |
| API の skill_type 固定確認 | P0 | 最小（確認のみ） |
| `acquire_executable_skill()` deprecated | P1 | 小 |
| `execute_skill` deprecated | P1 | 小 |
| 既存スキル SQL マイグレーション | P2 | 中 |
| `execute_skill` 削除 | P2 | 中 |
| スキーマ整理 | P3 | 小〜中 |

**最大のインパクト変更は `build_context()` の数行修正。** これだけで LLM の動作が「固定ボタンを押す」から「コマンドを動的に組み立てる」に変わる。残りは cleanup として順次対応できる。

---

*このドキュメントは設計の議論を促進するための Draft です。実装前に `PLAN.md` に反映してから作業を開始してください。*
