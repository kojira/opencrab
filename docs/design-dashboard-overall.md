# ダッシュボード全体UI設計書

**作成日**: 2026-03-24  
**ステータス**: Draft  
**対象**: `web/src/` 全体

---

## 0. 設計背景と問題点

| # | 問題 | 影響 |
|---|------|------|
| 1 | 文字・ボタンが大きすぎる | 情報密度が低く、スクロールが多い |
| 2 | ボトムタブに "Dashboard" ボタンがある | 現在地から別画面へのナビゲーションがおかしい |
| 3 | 未実装の検索バーがヘッダーを占領 | 画面面積の無駄、混乱を招く |
| 4 | i18n未対応箇所が多い | Co-Agents/Trusted Users/Channels等が英語 |
| 5 | Botトークンが部分表示 | セキュリティリスク |
| 6 | セッション数・ターン数等が0のまま | APIバグ |
| 7 | 全セッションがアクティブ表示 | APIバグ |

---

## 1. デザインガイドライン

### 1.1 フォントサイズ基準

| クラス | 使用場所 |
|--------|---------|
| `text-xs` / `text-label-sm` | メタデータ（timestamp, ID, バッジのラベル） |
| `text-sm` / `text-body-sm` | セカンダリ情報（説明文、補助テキスト） |
| `text-base` / `text-body-md` | 通常の本文テキスト |
| `text-lg` / `text-title-sm` | カードタイトル、セクションヘッド |
| `text-xl` / `text-title-md` | ページタイトル（最大） |

**原則**: ページタイトルは `text-xl` を上限とする。`text-2xl` 以上は使わない。

### 1.2 パディング・スペーシング規則

| 要素 | 規則 |
|------|------|
| ページ外側パディング | `p-4`（モバイル）/ `p-6`（デスクトップ）— 既存通り |
| カード内パディング | `p-3`（コンパクト）/ `p-4`（標準） |
| セクション間スペーシング | `space-y-3`（密）/ `space-y-4`（標準） |
| カードグリッドギャップ | `gap-3`（コンパクト）/ `gap-4`（標準） |
| ボタン内パディング | `py-1.5 px-3`（小）/ `py-2 px-4`（標準） |

**原則**: カードには `p-4` より大きいパディングは使わない。

### 1.3 カードの情報密度

**1カードに乗せる情報量の目安:**
- タイトル（1行）
- サブテキスト（1行、truncate）
- メタ情報（2〜3項目、text-xs/text-label-sm）
- アクション（最大2ボタン、または右矢印1つ）

**アンチパターン:**
- カードに大きなアイコン（w-12 h-12以上）を置いて余白を作る
- description句を2行以上書く
- `card-elevated` と `card-outlined` を混在させる（同一画面では統一）

### 1.4 ボタンサイズ規則

| ボタン種別 | クラス | 用途 |
|-----------|--------|------|
| 主要アクション | `btn-filled` | ページ内の最重要操作（1つのみ） |
| 副次アクション | `btn-tonal` | 中程度の重要度 |
| 境界線ボタン | `btn-outlined` | キャンセル・戻る |
| テキストボタン | `btn-text` | インライン・破壊的操作以外の補助 |
| 危険 | `btn-danger` | 削除・取り消し不可能な操作 |

**原則**: 1画面に `btn-filled` は1つまで。アイコンボタン単体（`p-1.5`）は一覧行内のみ使用。

---

## 2. ナビゲーション構造（ボトムタブ廃止）

### 2.1 廃止理由

`AppLayout.tsx` の `bottomNavItems` に "Dashboard" が含まれているが、これは現在地（ホーム）からの遷移ナビとして機能しない。モバイルナビゲーションとしての情報設計が壊れている。

### 2.2 新しいナビゲーション構造

**モバイル**: ボトムタブ廃止 → ハンバーガーメニュー（現在のサイドバー）のみ  
**デスクトップ**: 現在のサイドバー（変更なし）

```
AppLayout（AppLayout.tsx）
├── Sidebar（左サイドバー、モバイルはドロワー）
│   ├── ロゴ・ブランド名
│   ├── ダッシュボード（/）
│   ├── エージェント（/agents）
│   └── セッション（/sessions）
├── Header（ページ上部、幅縮小）
│   ├── [モバイルのみ] ハンバーガーボタン
│   ├── DB接続ステータス（dot + ラベル）
│   └── （検索バー・言語切り替えは削除）
└── main（コンテンツ領域）
    └── <Outlet />
```

**廃止する要素:**
- `AppLayout.tsx`: `bottomNavItems` 配列と `<nav>` 要素（ボトムナビ全体）
- `Header.tsx`: 検索バー（`<input>`）、言語切り替えトグル（EN/JA）

**移動先:**
- 言語設定: 設定ページ（将来実装）に移動
- 検索: 実装時にセッション一覧・エージェント一覧の画面内フィルタとして実装

### 2.3 画面遷移の階層構造

```
/ （ホーム）
├── /agents （エージェント一覧）
│   ├── /agents/new （エージェント作成）
│   └── /agents/:id （エージェント詳細）
│       ├── [tab] overview
│       ├── /agents/:id/skills
│       ├── /agents/:id/memory
│       ├── /agents/:id/sessions
│       ├── /agents/:id/co-agents
│       ├── /agents/:id/trusted-users
│       ├── /agents/:id/channels
│       ├── /agents/:id/allowed-commands
│       ├── /agents/:id/llm-logs
│       └── /agents/:id/analytics
├── /sessions （セッション一覧）
│   └── /sessions/:id （セッション詳細）
└── /workspace/:agentId （ワークスペース）
```

### 2.4 「戻る」ボタンの配置

| 画面 | 戻り先 | 実装 |
|------|--------|------|
| `/agents/new` | `/agents` | `<Link to="/agents">` でBreadcrumb最左 |
| `/agents/:id` | `/agents` | `AgentLayout.tsx` の既存Breadcrumb（変更なし） |
| `/sessions/:id` | `/sessions` | `SessionDetail.tsx` に `<Link to="/sessions">` の戻るボタン追加 |
| `/workspace/:agentId` | `/agents/:agentId` | 既存の戻るボタン（変更なし） |

---

## 3. ホーム画面（First View）

### 3.1 現状の問題

- StatCardとQuickLinkが2グリッド×2行で冗長
- QuickLinkの「メモリ」「分析」が `/agents` に遷移するだけで意味がない
- `home.subtitle` のフォールバックが英語: `"Manage your AI agents and sessions"`

### 3.2 新しいホーム画面レイアウト

```
┌─────────────────────────────────────────────────────────┐
│ ダッシュボード                                           │  ← ページタイトル（text-xl）
├─────────────────────────────────────────────────────────┤
│ [🤖 エージェント数: N] [💬 セッション: N] [🟢 アクティブ: N] │  ← 統計バー（超コンパクト）
├─────────────────────────────────────────────────────────┤
│ エージェント                            [+ 新規エージェント] │  ← セクションヘッド
│ ┌─────────────────────────────────────────────────────┐ │
│ │ [Avatar] エージェント名              🟢 active      │ │  ← AgentCard（コンパクト）
│ │          persona名   3スキル  2セッション  →       │ │
│ └─────────────────────────────────────────────────────┘ │
│ ...（最大5件表示）                 [全エージェントを見る→] │
└─────────────────────────────────────────────────────────┘
```

**変更点:**
- 統計をStatsCardグリッドから1行の `StatBar` コンポーネントに変更
- QuickLinkグリッドを廃止（ナビゲーションはサイドバーで行う）
- エージェントカード一覧を最大5件プレビュー表示
- 「全エージェントを見る →」リンクを追加

### 3.3 削除する要素

- `StatCard` コンポーネント（→ StatBarに置換）
- `QuickLink` コンポーネント（完全削除）
- `home.subtitle` の英語フォールバック文字列

### 3.4 コンポーネント変更

**`web/src/pages/Home.tsx`**:
```
削除: StatCard 関数
削除: QuickLink 関数
新規: StatBar コンポーネント（インライン実装）
変更: エージェントカード一覧を最大5件プレビュー表示
変更: home.subtitle を i18n キーのみ（フォールバックなし）
```

**`web/src/i18n/locales/ja.json`** / **`en.json`**:
```json
"home.subtitle": "AIエージェントとセッションを管理"
```

---

## 4. 各画面の情報設計

### 4.1 エージェント一覧（`Agents.tsx`）

**現状**: `grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6` で大きめカード

**改善:**
- ギャップを `gap-4` に縮小
- `AgentCard.tsx`: アバターを `w-10 h-10`（現在 `w-12 h-12`）に縮小
- ステータスバッジのラベルを日本語化（`active`→`稼働中`, `inactive`→`停止中`, `error`→`エラー`）

**`web/src/components/ui/AgentCard.tsx`** 変更点:
- `w-12 h-12` → `w-10 h-10`
- `mb-4` → `mb-3`
- `gap-3` → `gap-2.5`
- ステータスバッジ: `agent.status` をそのまま表示 → `t('agentStatus.' + agent.status)` に変更

### 4.2 エージェント詳細（`AgentLayout.tsx` + `AgentOverview.tsx`）

**現状**: タブが10個あり横スクロールが必要

**タブの整理案（10→8タブ）:**

| 旧 | 新 | 変更理由 |
|----|----|---------|
| overview | 概要 | 変更なし |
| skills | スキル | 変更なし |
| memory | メモリ | 変更なし |
| sessions | セッション | 変更なし |
| co-agents | Co-Agent | i18n改善 |
| trusted-users | 信頼ユーザー | 変更なし |
| channels | チャンネル | 変更なし |
| allowed-commands | コマンド | 変更なし |
| llm-logs | LLMログ | 変更なし |
| analytics | 分析 | 変更なし |

**`AgentLayout.tsx` の変更点:**
- タブラベルをすべて `t()` 経由に統一（既存）
- タブアイコンは維持
- モバイルでのタブスクロールインジケーター（フェードグラデーション）は維持

**`AgentOverview.tsx` の変更点:**
- `ActionCard` グリッドを `grid-cols-2 md:grid-cols-3` に変更（現在は常時 `grid-cols-1 md:grid-cols-3`）
- `text-3xl` アイコン → `text-2xl` に縮小
- `h2.section-title` のテキストをi18nキーで日本語に
- Bot トークン: `token_masked` の表示を `●●●...●●●` 形式に変更（後述セキュリティ要件参照）

### 4.3 セッション一覧（`Sessions.tsx`）

**現状**: `space-y-3` でカード一覧表示のみ

**改善:**
- ページタイトル横にフィルタセレクト（ステータス: 全て / アクティブ / 完了）
- セッション数バッジ（`[42件]`）
- `SessionCard` のコンパクト化:
  - アイコンを `w-10 h-10` → `w-8 h-8`
  - ディスコードメタ情報（guild名 + チャンネル名）を1行に圧縮
  - turn_number を右端のバッジとして表示

**`web/src/pages/Sessions.tsx`** 変更点:
```
追加: statusFilter state（'all' | 'active' | 'completed'）
追加: フィルタされたセッション一覧（filteredSessions）
追加: フィルタUIセレクト
変更: セッション件数表示（`${sessions.length}件`）
```

**`web/src/components/ui/SessionCard.tsx`** 変更点:
- `w-10 h-10` → `w-8 h-8`（アイコン）
- `text-title-md` → `text-title-sm`（テーマテキスト）
- ステータスバッジのラベルをi18n化（`active`→`アクティブ`等）

### 4.4 セッション詳細（`SessionDetail.tsx`）

**現状**: `sessionDetail.mode`, `sessionDetail.phase`, `sessionDetail.turn` はi18n対応済み

**改善:**
- ページ上部に「← セッション一覧に戻る」ボタン追加
- ログアイテムの `SessionLogItem` コンポーネントのタイムスタンプ表示を確認・追加

**`web/src/pages/SessionDetail.tsx`** 変更点:
```
追加: <Link to="/sessions" className="btn-text">← セッションに戻る</Link>
```

### 4.5 スキル一覧（`AgentSkills.tsx`）

**現状**: `SkillEditor` コンポーネントを並べているが情報が多い

**改善:**
- 一覧ではスキル名・アクティブ切り替えのみ表示（コンパクト行）
- 詳細編集は展開またはサイドパネルへ
- `SkillEditor` の `guidance` テキストエリアの高さを `h-24`（現在 `h-32` 以上の可能性）に制限

**将来対応（v2）:**
- `/agents/:id/skills/:skillId` の詳細ページを別途作成

### 4.6 LLMログ（`AgentLlmLogs.tsx`）

**別途詳細設計書**: `docs/design-llm-log-ui.md` を参照。

本書での追加方針:
- 本文のデザインガイドライン（Section 1）に従う
- ページタイトルは `text-xl` を上限
- フィルタバーは `bg-surface-container-high` の1行コンテナ

### 4.7 Co-Agents（`AgentCoAgents.tsx`）

**現状**: ほぼ英語のまま

**i18n対応が必要なハードコード英語テキスト:**

```
"Co-Agents"                          → t('coAgents.title')         ※ ja.jsonに追加済み
"Trusted co-agents that can act..."  → t('coAgents.description')   ※ ja.jsonに追加済み
"Add Co-Agent"（ボタン）              → t('coAgents.addButton')
"Co-Agent ID is required."           → t('coAgents.idRequired')
"Co-Agent ID *"                      → t('coAgents.idLabel')
"Allowed Actions (...)"              → t('coAgents.actionsLabel')
"All actions"                        → t('coAgents.allActions')
"Loading..."                         → t('common.loading')（既存）
"Error: "                            → t('common.error')（既存）
"No trusted co-agents."              → t('coAgents.noCoAgents')
"Add co-agents to allow them..."     → t('coAgents.emptyDesc')
"Cancel" / "Add" / "Adding..."       → t('common.cancel') / t('common.add') / t('common.adding')
"Remove" / "Remove Co-Agent?"        → t('common.remove') / t('coAgents.confirmRemoveTitle')
"Remove ... from trusted co-agents?" → t('coAgents.confirmRemoveMsg')
"Co-Agent ID" / "Allowed Actions" / "Added By" / "Added At" → テーブルヘッダーのi18nキー追加
```

**レイアウト改善:**
- テーブル形式からカードリスト形式に変更（モバイルで横スクロール発生を回避）
- 各エントリを1行コンパクト表示（co_agent_id / allowed_actions / 追加日 / 削除ボタン）

### 4.8 Trusted Users（`AgentTrustedUsers.tsx`）

**現状**: ほぼ英語のまま

**i18n対応が必要なハードコード英語テキスト:**

```
"Trusted Users"                          → t('trustedUsers.title')      ※ ja.jsonに追加済み
"Discord users who can send DMs..."      → t('trustedUsers.description') ※ ja.jsonに追加済み
"Add User"（ボタン）                      → t('trustedUsers.addButton')
"Discord User ID is required."           → t('trustedUsers.idRequired')
"Discord User ID *"                      → t('trustedUsers.idLabel')
"Permission"                             → t('trustedUsers.permissionLabel')
"Loading..." / "Error: "                 → t('common.loading') / t('common.error')
"No trusted users."                      → t('trustedUsers.noUsers')
"Add Discord user IDs to allow..."       → t('trustedUsers.emptyDesc')
"Save" / "Cancel" / "Add" / "Adding..."  → t('common.save') / t('common.cancel') / t('common.add') / t('common.adding')
"Remove" / "Remove User?"                → t('common.remove') / t('trustedUsers.confirmRemoveTitle')
"Remove this user from trusted users?"   → t('trustedUsers.confirmRemoveMsg')
"Discord User ID" / "Permission" / "Added By" / "Added At" → テーブルヘッダーのi18nキー追加
```

**レイアウト改善:**
- Co-Agentsと同様にカードリスト形式に変更

### 4.9 チャンネルホワイトリスト（`AgentChannels.tsx`）

**現状**: Guild IDを入力してLoadするUIで、英語のまま。各カラムの意味が不明。

**i18n対応が必要なハードコード英語テキスト:**

```
"Guild ID"                           → t('channels.guildIdLabel')
"Enter guild ID"                     → t('channels.guildIdPlaceholder')
"Load"                               → t('channels.loadButton')
"Loading..."                         → t('common.loading')
"Channel" / "Readable" / "Writable" → テーブルヘッダー（i18nキー追加）
"Whitelisted" / "Heartbeat"          → テーブルヘッダー（i18nキー追加）
"Interval (sec)"                     → t('channels.intervalLabel')
"Global"（placeholder）               → t('channels.globalPlaceholder')
"Save" / "Delete"                    → t('common.save') / t('common.delete')
"No channel configs found..."        → t('channels.noConfigs')
```

**説明テキストの追加（i18nキー）:**

各カラムにツールチップまたは説明テキストを追加:
- Readable: 「エージェントがこのチャンネルのメッセージを読める」
- Writable: 「エージェントがこのチャンネルに書き込める」
- Whitelisted: 「ホワイトリスト登録済みチャンネル（エージェントが応答する）」
- Heartbeat: 「定期的なハートビートメッセージを送信する」

### 4.10 ワークスペース（`Workspace.tsx`）

**現状**: ファイル一覧+ビューアの2カラムレイアウト。`agentId` をフル表示している。

**改善:**
- ページ上部のエージェントID表示を `font-mono text-xs` でコンパクトに（現在 `text-body-md`）
- `agentId` の全文表示 → 最初8文字だけ表示 + ホバーでfull表示（`title` 属性）
- ファイルエントリの `size` 表示をバイト単位から自動フォーマット（1KB以上は KB 表示）

**`web/src/pages/Workspace.tsx`** 変更点:
```
変更: "エージェント:" + agentId フルパス → agentId.slice(0, 8) + "..." (title=agentId)
変更: <span className="text-body-md"> → <span className="text-xs font-mono">
```

---

## 5. セキュリティ要件

### 5.1 Botトークンの完全マスキング

**現状（`AgentOverview.tsx`）:**
```tsx
<DetailRow label={t('agentDetail.botToken')} value={config.token_masked || '***'} />
```

`token_masked` がバックエンドから `"MTk5MDYz...（途中省略）...8abc"` のような部分表示で返ってくる場合、それがそのまま画面に表示される。

**修正方針:**
- フロントエンド側で常に `●●●...●●●` 形式で表示する（トークン値を一切表示しない）
- バックエンドの `token_masked` フィールドは「設定済みか否かの確認」にのみ使用
- 「コピー」ボタンは設けない

**`web/src/pages/AgentOverview.tsx`** 変更点:
```tsx
// 変更前
<DetailRow label={t('agentDetail.botToken')} value={config.token_masked || '***'} />

// 変更後
<DetailRow 
  label={t('agentDetail.botToken')} 
  value={config.configured ? '●●●●●●●●●●●●●●●●●●●●' : t('agentDetail.notConfigured')}
/>
```

**追加i18nキー:**
```json
"agentDetail.notConfigured": "未設定"
```

### 5.2 言語設定の移動

**現状**: `Header.tsx` に言語切り替えトグル（EN/JA ボタン）がある。

**修正**: ヘッダーから削除し、将来の設定ページに移動。  
暫定対応: サイドバーのフッター部分に小さく配置。

---

## 6. i18n方針

### 6.1 基本方針

- **全テキストを日本語に統一**（ユーザー向け表示テキストはすべて `t()` を経由）
- ハードコードされた英語テキストを段階的にi18n化
- `ja.json` と `en.json` は常に同期（キーが片方にしかない状態にしない）

### 6.2 未対応キー一覧（優先度付き）

#### 高優先度（ユーザーが常に見る）

| ファイル | テキスト | 追加すべきキー |
|---------|---------|--------------|
| `AppLayout.tsx` | `'Dashboard'`, `'Agents'`, `'Sessions'` | ボトムタブ廃止で不要になる |
| `AgentCoAgents.tsx` | 全ラベル（Section 4.7参照） | `coAgents.*` |
| `AgentTrustedUsers.tsx` | 全ラベル（Section 4.8参照） | `trustedUsers.*` |
| `AgentChannels.tsx` | 全ラベル（Section 4.9参照） | `channels.*` |
| `Home.tsx` | `"Manage your AI agents and sessions"` | `home.subtitle` |

#### 中優先度（設定変更時に見る）

| ファイル | テキスト | 追加すべきキー |
|---------|---------|--------------|
| `AgentOverview.tsx` | `"Owner Discord ID を更新しました。"` | `agentDetail.ownerUpdated` |
| `AgentOverview.tsx` | タブラベル `"Owner ID のみ変更"`, `"Bot トークンを変更"` | `agentDetail.editModeOwnerOnly`, `agentDetail.editModeFullToken` |
| `AgentCoAgents.tsx` | モーダルタイトル等 | 上記参照 |
| `AgentTrustedUsers.tsx` | モーダルタイトル等 | 上記参照 |

#### 追加が必要な `ja.json` キー（全量）

```json
{
  "home.subtitle": "AIエージェントとセッションを管理",

  "agentDetail.notConfigured": "未設定",
  "agentDetail.ownerUpdated": "Owner Discord ID を更新しました。",
  "agentDetail.editModeOwnerOnly": "Owner ID のみ変更",
  "agentDetail.editModeFullToken": "Bot トークンを変更",

  "agentStatus.active": "稼働中",
  "agentStatus.inactive": "停止中",
  "agentStatus.error": "エラー",

  "sessionStatus.active": "アクティブ",
  "sessionStatus.completed": "完了",
  "sessionStatus.paused": "一時停止",

  "common.add": "追加",
  "common.adding": "追加中...",
  "common.remove": "削除",
  "common.filter": "フィルタ",
  "common.all": "すべて",

  "coAgents.title": "Co-エージェント",
  "coAgents.description": "このエージェントの代わりに操作できる信頼済みCo-エージェントの管理。",
  "coAgents.addButton": "Co-エージェントを追加",
  "coAgents.idRequired": "Co-エージェントIDは必須です。",
  "coAgents.idLabel": "Co-エージェントID *",
  "coAgents.actionsLabel": "許可アクション（カンマ区切り、空 = 全許可）",
  "coAgents.allActions": "全アクション",
  "coAgents.noCoAgents": "Co-エージェントが登録されていません。",
  "coAgents.emptyDesc": "Co-エージェントを追加して、このエージェントの代理操作を許可できます。",
  "coAgents.confirmRemoveTitle": "Co-エージェントを削除しますか？",
  "coAgents.confirmRemoveMsg": "\"{{id}}\" をCo-エージェントから削除しますか？",
  "coAgents.tableId": "Co-エージェントID",
  "coAgents.tableActions": "許可アクション",
  "coAgents.tableAddedBy": "追加者",
  "coAgents.tableAddedAt": "追加日時",

  "trustedUsers.title": "信頼ユーザー",
  "trustedUsers.description": "このエージェントにDMで話しかけられるDiscordユーザーの管理。空の場合はオーナーのみが操作できます。",
  "trustedUsers.addButton": "ユーザーを追加",
  "trustedUsers.idRequired": "Discord User IDは必須です。",
  "trustedUsers.idLabel": "Discord User ID *",
  "trustedUsers.permissionLabel": "権限",
  "trustedUsers.noUsers": "信頼ユーザーが登録されていません。",
  "trustedUsers.emptyDesc": "Discord User IDを追加して、このエージェントとのDMを許可できます。",
  "trustedUsers.confirmRemoveTitle": "ユーザーを削除しますか？",
  "trustedUsers.confirmRemoveMsg": "このユーザーを信頼ユーザーから削除しますか？",
  "trustedUsers.tableId": "Discord User ID",
  "trustedUsers.tablePermission": "権限",
  "trustedUsers.tableAddedBy": "追加者",
  "trustedUsers.tableAddedAt": "追加日時",

  "channels.guildIdLabel": "Guild ID",
  "channels.guildIdPlaceholder": "Guild IDを入力",
  "channels.loadButton": "読み込み",
  "channels.noConfigs": "このGuildのチャンネル設定が見つかりません。",
  "channels.globalPlaceholder": "グローバル設定",
  "channels.intervalLabel": "間隔（秒）",
  "channels.tableChannel": "チャンネル",
  "channels.tableReadable": "読み取り",
  "channels.tableWritable": "書き込み",
  "channels.tableWhitelisted": "ホワイトリスト",
  "channels.tableHeartbeat": "ハートビート",
  "channels.tableInterval": "間隔（秒）",
  "channels.tableActions": "操作",
  "channels.tooltipReadable": "エージェントがこのチャンネルのメッセージを読める",
  "channels.tooltipWritable": "エージェントがこのチャンネルに書き込める",
  "channels.tooltipWhitelisted": "エージェントが応答するチャンネル",
  "channels.tooltipHeartbeat": "定期的なハートビートメッセージを送信する"
}
```

---

## 7. APIバグ修正が必要な箇所

### 7.1 セッション数が0のまま（`AgentSummary.session_count`）

**原因調査**: `crates/server/src/api/agents.rs` の SQL:
```sql
(SELECT COUNT(*) FROM agent_sessions WHERE agent_id = i.agent_id) as session_count
```
- テーブル名が `agent_sessions` だが、実際のテーブルが異なる可能性
- Discord経由のセッションが別テーブルに保存されている可能性

**修正箇所**: `crates/server/src/api/agents.rs`  
**調査方法**: `crates/db/src/schema.rs` または migrations でテーブル名を確認

### 7.2 ターン数が0のまま（`SessionDto.turn_number`）

**原因（確認済み）**: `crates/server/src/api/sessions.rs`:
```rust
// list_sessions ハンドラ
// SessionRowにはturn_numberが含まれているはずだが...
turn_number: None,  // ← list_sessions の SQL クエリが turn_number を取得していない
```

`agents_messages.rs`:
```rust
turn_number: 0,  // ← 新規セッション作成時にハードコード
```

**修正箇所**:  
- `crates/server/src/api/sessions.rs`: `list_sessions` SQLクエリで `turn_number` を SELECT に含める
- `crates/db/src/queries.rs`: `list_sessions()` 関数の SELECT句を確認・修正

### 7.3 全セッションがアクティブ表示

**原因（確認済み）**: `crates/server/src/api/agents_messages.rs`:
```rust
status: "active".to_string(),  // ← ハードコード
```
Discord経由でセッションが作成されるたびに `status = "active"` が固定セット。  
セッション終了時に `status` が更新されていない可能性。

**修正箇所**:
- `crates/server/src/api/agents_messages.rs`: セッション完了時に `status = "completed"` を UPDATE
- または `crates/db/src/queries.rs` に `update_session_status()` 関数を追加

### 7.4 ツール利用回数が0のまま（`SkillDto.usage_count`）

**確認が必要**: `AgentCard` に `skill_count` 表示はあるが、`SkillEditor` の `usage_count` が常に0の可能性。

**調査箇所**: `crates/db/src/queries.rs` の `list_skills()` または `toggle_skill()` で `usage_count` が更新されているか確認。  
ツール実行時に `usage_count` をインクリメントする処理が必要。

---

## 8. コンポーネント変更一覧

### 8.1 変更・削除

| ファイル | 変更種別 | 概要 |
|---------|---------|------|
| `web/src/components/layout/AppLayout.tsx` | 改修 | ボトムナビ（`bottomNavItems` + `<nav>`）削除 |
| `web/src/components/layout/Header.tsx` | 改修 | 検索バー削除・言語切り替え削除・DB接続表示のみに |
| `web/src/pages/Home.tsx` | 改修 | StatCard/QuickLink削除、StatBar新規、エージェントプレビュー追加 |
| `web/src/pages/Agents.tsx` | 軽微改修 | ギャップ縮小（`gap-6`→`gap-4`） |
| `web/src/pages/Sessions.tsx` | 改修 | ステータスフィルタ追加・件数表示追加 |
| `web/src/pages/AgentOverview.tsx` | 改修 | ActionCard縮小・Botトークン完全マスキング |
| `web/src/pages/AgentCoAgents.tsx` | 改修 | 全テキストi18n化・テーブル→カードリスト化 |
| `web/src/pages/AgentTrustedUsers.tsx` | 改修 | 全テキストi18n化・テーブル→カードリスト化 |
| `web/src/pages/AgentChannels.tsx` | 改修 | 全テキストi18n化・カラム説明追加 |
| `web/src/pages/Workspace.tsx` | 軽微改修 | agentID表示コンパクト化 |
| `web/src/pages/SessionDetail.tsx` | 軽微改修 | 「戻る」ボタン追加 |
| `web/src/components/ui/AgentCard.tsx` | 軽微改修 | アバターサイズ縮小・ステータスラベルi18n化 |
| `web/src/components/ui/SessionCard.tsx` | 軽微改修 | アイコンサイズ縮小・ステータスラベルi18n化 |
| `web/src/i18n/locales/ja.json` | 追加 | Section 6.2 の全キーを追加 |
| `web/src/i18n/locales/en.json` | 追加 | 同上（英語訳） |

### 8.2 新規作成

| ファイル | 概要 |
|---------|------|
| `web/src/components/ui/StatBar.tsx` | ホーム画面の統計バー（コンパクト版StatCard） |

### 8.3 変更しないもの

| ファイル | 理由 |
|---------|------|
| `web/src/components/layout/Sidebar.tsx` | 問題なし |
| `web/src/components/layout/AgentLayout.tsx` | タブ構造は維持（軽微なスタイル調整のみ） |
| `web/src/pages/AgentSkills.tsx` | 概ね問題なし（詳細ページは将来対応） |
| `web/src/pages/AgentMemory.tsx` | 問題なし |
| `web/src/pages/AgentAnalytics.tsx` | 問題なし |
| `web/src/pages/AgentLlmLogs.tsx` | 別設計書（`design-llm-log-ui.md`）で管理 |
| `web/src/App.tsx` | ルーティング変更なし |

---

## 9. 実装優先度

### Phase 1（即時・高インパクト）
1. **ボトムタブ廃止** — `AppLayout.tsx` から `bottomNavItems` + `<nav>` を削除
2. **検索バー・言語切り替え削除** — `Header.tsx` をDB接続ステータスのみに
3. **Botトークン完全マスキング** — `AgentOverview.tsx`
4. **i18n: Co-Agents / Trusted Users** — ハードコード英語を `t()` に置換

### Phase 2（UI改善）
5. **ホーム画面リファクタ** — StatBar化、エージェントプレビュー
6. **Channels i18n + 説明追加**
7. **AgentCard / SessionCard コンパクト化**
8. **セッション一覧フィルタ追加**

### Phase 3（APIバグ修正）
9. **ターン数表示修正** — `sessions.rs` のSQLクエリ修正
10. **セッションステータス修正** — `agents_messages.rs` + DB更新ロジック
11. **セッション数修正** — `agents.rs` のSQLテーブル名確認・修正
12. **ツール利用回数修正** — `queries.rs` の `usage_count` 更新ロジック追加

---

## 10. 参考: 既存i18nキーの整理

### `ja.json` に定義済みだが未使用の可能性があるキー

```
"nav.skills"   — Sidebar.tsx に `/skills` ルートなし（エージェント内タブのみ）
"nav.memory"   — 同上
"nav.analytics" — 同上
```

これらはSidebar.tsxでは使われていないが、将来のトップレベルナビ追加時のために保持。
