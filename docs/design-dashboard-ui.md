# Dashboard UI 全体設計書

**作成日**: 2026-03-24  
**ステータス**: Draft  
**スコープ**: opencrab Webフロントエンド全画面

---

## 0. 設計の方針

### なぜ全体設計が必要か

現状は画面ごとに場当たり的に作られており、以下の問題がある：

1. **AgentLayoutのタブが10個** → 横スクロール必須、重要度の差がない
2. **AgentOverview が「リンクボタンの一覧」** → 情報ゼロ、タブと役割重複
3. **Home が薄い** → 数値3個とクイックリンクだけ。ここで何が見たいか明確でない
4. **グローバルLLMログ画面がない** → エージェントをまたいだログ確認が不可能
5. **セッション一覧にフィルタなし** → 大量セッションから目的のものを探せない
6. **レイアウト崩れ** → overflow未対処、長いJSON/モデル名が横はみ出し

この設計書で全体のUXを統一し、各画面を作り直しても再実装にならないようにする。

---

## 1. デザインガイドライン

### 1.1 デザインシステム

既存のMaterial Design 3 + Tailwind構成を維持する。  
ただし現状の実装から以下の規則を明示・統一する。

### 1.2 タイポグラフィ

既存の `tailwind.config.js` のスケールをそのまま使う。使用箇所を統一：

| 役割 | クラス | 使用箇所 |
|------|--------|---------|
| ページタイトル | `text-title-lg font-bold` | 各ページのh1 |
| セクション見出し | `text-title-md font-semibold` | カード内のh2 |
| カード見出し | `text-title-sm font-semibold` | リスト項目のタイトル |
| 本文 | `text-body-md` | 説明文・詳細テキスト |
| 補助テキスト | `text-body-sm text-on-surface-variant` | メタ情報・タイムスタンプ |
| ラベル | `text-label-lg` | バッジ・チップ・タブ |
| コード | `font-mono text-body-sm` | ID・JSON・コマンド |

### 1.3 スペーシング

```
コンテナ内パディング:
  card-elevated: p-3 (12px) — コンパクトリスト用
  card-outlined: p-4 (16px) — 詳細表示用
  card-section: p-5 (20px) — 設定フォーム用

アイテム間隔:
  リスト項目: space-y-2 (8px)
  セクション間: space-y-4 (16px)  
  ページ内セクション間: space-y-6 (24px)

ページ外側パディング:
  モバイル: p-4 (16px)
  デスクトップ: p-6 (24px)
```

### 1.4 カード密度

| タイプ | 用途 | 基本クラス |
|--------|------|-----------|
| Compact | 一覧リスト（多数表示） | `card-elevated p-3` |
| Standard | 詳細表示・設定 | `card-outlined p-4` |
| Spacious | ヒーローカード・フォーム | `card-outlined p-5` |

**原則**: 一覧画面はCompact、詳細画面はStandard/Spacious。

### 1.5 カラーパレット（既存を整理）

```
Primary (#4F46E5):   主要アクション、アクティブ状態
Secondary (#475569): サブナビ、補助情報
Tertiary (#0D9488):  セッション関連、成功系アクセント
Error (#DC2626):     エラー、削除アクション
Success (#16A34A):   稼働中、正常状態
Warning (#D97706):   注意、一時停止
```

### 1.6 レイアウト崩れ防止（必須ルール）

**すべてのコンポーネントで以下を守る:**

```tsx
// ❌ NG: overflow未指定でflex要素が崩れる
<div className="flex">
  <span className="text-very-long-content">...</span>
</div>

// ✅ OK: min-w-0 + truncate/break-wordで制御
<div className="flex min-w-0">
  <span className="truncate flex-1 min-w-0">...</span>
</div>

// コードブロック・JSON表示
<pre className="whitespace-pre-wrap break-all overflow-x-auto max-w-full font-mono text-body-sm">

// カードコンテナ
<div className="card-elevated overflow-hidden min-w-0">
```

---

## 2. ナビゲーション構造

### 2.1 現状の問題

- サイドバー: Dashboard / Agents / Sessions（3項目）
- モバイルボトムナブ: 同じ3項目
- AgentLayout: 10タブが横スクロール（overview / skills / memory / sessions / co-agents / trusted-users / channels / allowed-commands / llm-logs / analytics）

### 2.2 新しいナビゲーション構造

#### グローバルナビゲーション（サイドバー & モバイルボトムナブ）

```
┌─ opencrab ────────────────────┐
│  🏠 ホーム         [Dashboard]  │  ← / 
│  🤖 エージェント   [Agents]     │  ← /agents
│  💬 セッション     [Sessions]   │  ← /sessions
│  📋 LLMログ        [LLM Logs]   │  ← /llm-logs  ★新規追加
└───────────────────────────────┘
       [バージョン情報]
```

**モバイルボトムナブ（廃止ではなく維持、4項目に拡張）**:
```
🏠ホーム  🤖エージェント  💬セッション  📋ログ
```

LLMログをグローバルに昇格させる理由:
- 現状「/agents/:id/llm-logs」は単一エージェントのみ
- 複数エージェントをまたいで監視したいユースケースが多い
- 「今どのエージェントが何回LLM呼び出してるか」はホーム的な情報

#### AgentLayoutのタブ構造（10 → 5タブに統廃合）

```
Before (10タブ):
  overview / skills / memory / sessions / co-agents / 
  trusted-users / channels / allowed-commands / llm-logs / analytics

After (5タブ):
  概要(Overview) / 設定(Settings) / スキル(Skills) / セッション(Sessions) / ログ(Logs)
```

**統廃合の詳細:**

| 旧タブ | 新タブ | 理由 |
|--------|--------|------|
| overview | 概要 | そのまま（ただし内容を刷新） |
| skills | スキル | そのまま |
| memory | 設定 | memory + channelsをまとめる（ともに設定系） |
| sessions | セッション | そのまま |
| analytics + llm-logs | ログ | 統合（stats + 生ログを1画面に） |
| co-agents | 設定 | 設定タブ内のサブセクションに |
| trusted-users | 設定 | 設定タブ内のサブセクションに |
| channels | 設定 | 設定タブ内のサブセクションに |
| allowed-commands | 設定 | 設定タブ内のサブセクションに |
| persona / edit | 概要 | 概要タブ内でインライン編集 |

### 2.3 画面遷移マップ

```
/ (ホーム)
├── /agents (エージェント一覧)
│   ├── /agents/new (エージェント作成)
│   └── /agents/:id (エージェント詳細)
│       ├── /agents/:id (概要タブ)
│       ├── /agents/:id/settings (設定タブ)
│       │   ├── #identity (アイデンティティ)
│       │   ├── #persona (ペルソナ)
│       │   ├── #channels (チャンネル)
│       │   ├── #co-agents (コエージェント)
│       │   ├── #trusted-users (信頼ユーザー)
│       │   └── #allowed-commands (許可コマンド)
│       ├── /agents/:id/skills (スキルタブ)
│       ├── /agents/:id/sessions (セッションタブ)
│       └── /agents/:id/logs (ログタブ = Analytics + LLM logs)
├── /sessions (セッション一覧)
│   └── /sessions/:id (セッション詳細)
├── /llm-logs (グローバルLLMログ) ★新規
└── /workspace/:agentId (ワークスペース)
```

---

## 3. ホーム画面（First View）

### 3.1 設計方針

ホームは「今何が起きているか」を一目で把握するダッシュボード。  
現状の問題: 数値3個 + クイックリンクだけで情報密度が低い。

### 3.2 情報の優先順位

1. **🔴 最優先**: アクティブなセッション一覧（今動いてるものを見たい）
2. **🟡 重要**: エラーのあるエージェント・LLM呼び出し異常
3. **🟢 参考**: 総計統計（エージェント数、セッション数）
4. **📎 補助**: クイックリンク（新規作成など）

### 3.3 ホーム画面レイアウト

```
┌─────────────────────────────────────────────────────┐
│ opencrab Dashboard            [● DB接続中]           │  ← ヘッダー
├─────────────────────────────────────────────────────┤
│ ┌──────────┐ ┌──────────┐ ┌──────────┐             │
│ │🤖 エージェント│ │💬 セッション│ │▶ アクティブ│             │
│ │    3      │ │   12    │ │    2     │             │
│ └──────────┘ └──────────┘ └──────────┘             │  ← StatCards（コンパクト）
├─────────────────────────────────────────────────────┤
│ ▶ アクティブセッション (2件)                [全て見る→] │  ← セクションヘッダー
│ ┌─────────────────────────────────────────────────┐ │
│ │ 🟢 エージェントC  Discord #general  Turn 47        │ │  ← アクティブセッションカード
│ │ 最終活動: 2分前  claude-3-5-sonnet              │ │
│ └─────────────────────────────────────────────────┘ │
│ ┌─────────────────────────────────────────────────┐ │
│ │ 🟢 test-bot    Discord DM     Turn 3            │ │
│ │ 最終活動: 15分前  claude-3-haiku                │ │
│ └─────────────────────────────────────────────────┘ │
├─────────────────────────────────────────────────────┤
│ ⚡ LLM 直近の活動 (過去1時間)              [ログ→]   │
│ ┌─────────────────────────────────────────────────┐ │
│ │ エージェントC  claude-3-5-sonnet  1,234tok  230ms   │ │  ← ミニログカード（5件）
│ │ 2分前 ・ tool_calls                              │ │
│ └─────────────────────────────────────────────────┘ │
│ ┌─────────────────────────────────────────────────┐ │
│ │ test-bot  claude-3-haiku  456tok  120ms         │ │
│ │ 15分前 ・ stop                                   │ │
│ └─────────────────────────────────────────────────┘ │
├─────────────────────────────────────────────────────┤
│ 🤖 エージェント                           [一覧→]   │
│ ┌──────────┐ ┌──────────┐ ┌──────────┐            │
│ │🟢 エージェントC│ │🔴 test-bot│ │⚫ agent3  │            │  ← エージェントカード(小)
│ │ 3 skills │ │ ERROR    │ │ 0 active │            │
│ └──────────┘ └──────────┘ └──────────┘            │
└─────────────────────────────────────────────────────┘
```

### 3.4 コンポーネント

| コンポーネント | ファイル | 表示内容 |
|-------------|--------|---------|
| `StatCards` | `Home.tsx` 内 | エージェント数・セッション数・アクティブ数（既存刷新） |
| `ActiveSessionsMini` | `components/home/ActiveSessionsMini.tsx` | アクティブセッション最大3件 |
| `RecentLlmActivity` | `components/home/RecentLlmActivity.tsx` | 過去1時間のLLMログ最新5件 |
| `AgentsMiniGrid` | `components/home/AgentsMiniGrid.tsx` | エージェント一覧（最大6件）+「全て見る」 |

---

## 4. エージェント一覧（/agents）

### 4.1 現状

カードグリッド表示。各カードに名前・ペルソナ・スキル数・セッション数・ステータス。  
検索・フィルタなし。

### 4.2 改善点

- **検索バー**を追加（エージェント名でfilter、クライアントサイドで即時）
- **ステータスフィルタ**（active / error / all）
- カードのagentアイコンをより大きく（48px → そのまま）
- 「最終LLM呼び出し」を表示（いつ最後に動いたか）

### 4.3 レイアウト

```
┌─────────────────────────────────────────────────┐
│ エージェント             [+ 新規エージェント]     │
├─────────────────────────────────────────────────┤
│ [🔍 名前で検索...]  [🟢 稼働中▼]                │  ← フィルターバー（新規）
├─────────────────────────────────────────────────┤
│ ┌─────────┐ ┌─────────┐ ┌─────────┐           │
│ │AgentCard│ │AgentCard│ │AgentCard│           │  ← grid-cols-1 md:2 lg:3
│ └─────────┘ └─────────┘ └─────────┘           │
└─────────────────────────────────────────────────┘
```

### 4.4 AgentCard 改善

```tsx
// AgentCard の表示内容（改善後）
┌─────────────────────────────────────────┐
│ [AVATAR]  エージェントC          [🟢 active]  │
│           persona_name                  │
├─────────────────────────────────────────┤
│ 🧠 3スキル  💬 12セッション              │
│ 最終活動: 2分前 (claude-3-5-sonnet)      │  ← ★新規追加
└─────────────────────────────────────────┘
```

---

## 5. エージェント詳細（/agents/:id）

### 5.1 タブ構成（5タブ）

```
[概要] [設定] [スキル] [セッション] [ログ]
```

AgentLayoutのヘッダーカードは維持するが、タブ数を減らしてスクロール不要にする。

### 5.2 概要タブ（AgentOverview）

**現状の問題**: ActionCardの羅列でリンク集になっている。  
**改善**: エージェントの状態・統計・最近の活動を表示するダッシュボードにする。

```
┌─────────────────────────────────────────────────┐
│ Discord Bot設定          [🟢 稼働中] [停止][編集] │  ← Discordステータス（コンパクト化）
├─────────────────────────────────────────────────┤
│ 今日の活動                                        │
│   LLM呼び出し: 47回  |  トークン: 123,456  |  エラー: 0  │
├─────────────────────────────────────────────────┤
│ アクティブセッション (2件)                [→]     │
│  ┌────────────────────────────────────────────┐ │
│  │ Discord #general  Turn 47  ⚡ 2分前         │ │
│  └────────────────────────────────────────────┘ │
├─────────────────────────────────────────────────┤
│ エージェント情報                                  │
│   ID: xxxxxxxx-xxxx-xxxx  名前: エージェントC        │
└─────────────────────────────────────────────────┘
```

### 5.3 設定タブ（AgentSettings）

現在の以下のページを統合:
- AgentIdentityEdit（名前・アイコン・ID）
- PersonaEdit（ペルソナ設定）
- AgentChannels（チャンネル設定）
- AgentCoAgents（コエージェント）
- AgentTrustedUsers（信頼ユーザー）
- AgentAllowedCommands（許可コマンド）

**レイアウト**: アコーディオン形式のセクション一覧

```
┌─────────────────────────────────────────────────┐
│ ▼ アイデンティティ                               │  ← 展開中
│   名前: [________________]                       │
│   アイコンURL: [________________]                │
│   [保存]                                        │
├─────────────────────────────────────────────────┤
│ ▶ ペルソナ                                       │  ← 折りたたみ中
├─────────────────────────────────────────────────┤
│ ▶ Discord Bot設定                                │
├─────────────────────────────────────────────────┤
│ ▶ チャンネル設定                                 │
├─────────────────────────────────────────────────┤
│ ▶ コエージェント                                 │
├─────────────────────────────────────────────────┤
│ ▶ 信頼ユーザー                                   │
├─────────────────────────────────────────────────┤
│ ▶ 許可コマンド                                   │
├─────────────────────────────────────────────────┤
│ ─ 危険な操作 ─                                  │
│   [🗑️ エージェントを削除]                        │
└─────────────────────────────────────────────────┘
```

**ファイル**: `pages/AgentSettings.tsx`（新規作成）  
各セクションはコンポーネント化:
- `components/agent-settings/IdentitySection.tsx`
- `components/agent-settings/PersonaSection.tsx`
- `components/agent-settings/DiscordBotSection.tsx`（AgentOverviewから移動）
- `components/agent-settings/ChannelsSection.tsx`
- `components/agent-settings/CoAgentsSection.tsx`
- `components/agent-settings/TrustedUsersSection.tsx`
- `components/agent-settings/AllowedCommandsSection.tsx`

### 5.4 スキルタブ（AgentSkills）

現状ほぼ問題なし。以下のみ追加:
- 上部に「インストール済み: N件 / 有効: M件」のサマリーバッジ
- 検索バー（クライアントサイド即時フィルタ）

```
┌─────────────────────────────────────────────────┐
│ スキル  [🧠 12件 / ✅ 8件有効]   [🔍 検索...]   │
├─────────────────────────────────────────────────┤
│ ┌──────────────────────────────────────────────┐│
│ │ [toggle] skill-name   ✅有効                  ││
│ │ 説明テキスト...                               ││
│ └──────────────────────────────────────────────┘│
└─────────────────────────────────────────────────┘
```

### 5.5 セッションタブ（AgentSessions）

現状: フィルタなし、SessionCard一覧のみ

改善:
- **ステータスフィルタ**: active / completed / all
- 各SessionCardに「最終メッセージ時刻」を追加
- SessionCardのデザインをコンパクト化（高さを抑える）

### 5.6 ログタブ（AgentLogs）

現在のAgentAnalytics + AgentLlmLogsを統合。

```
┌─────────────────────────────────────────────────┐
│ [📊 統計] [📋 生ログ]    ← タブ切り替え           │
├─────────────────────────────────────────────────┤
│ [統計タブ: 既存StatsSection そのまま]             │
│  または                                          │
│ [生ログタブ: 詳細は「6. LLMログ画面」を参照]      │
└─────────────────────────────────────────────────┘
```

---

## 6. LLMログ画面

### 6.1 グローバルLLMログ（/llm-logs）[新規]

全エージェントのLLMログを横断的に見る画面。

```
┌─────────────────────────────────────────────────┐
│ LLMログ                                          │
├─────────────────────────────────────────────────┤
│ [フィルターバー — 下記参照]                        │
├─────────────────────────────────────────────────┤
│ 42件 / 全100件                                   │
├─────────────────────────────────────────────────┤
│ [LogCard一覧 — エージェント名列を追加]             │
└─────────────────────────────────────────────────┘
                            [⬇ 最新へ] ← FAB固定
```

### 6.2 エージェントLLMログ（/agents/:id/logs）

エージェント詳細のログタブ。グローバルと同じUIだがエージェントでフィルタ済み。

### 6.3 フィルターバー仕様

```
┌────────────────────────────────────────────────────────────┐
│ [直近1時間▼] [エージェント▼] [モデル▼] [エラーのみ□]       │
│ [🔍 全文検索（request/response内を検索）___________] [クリア]│
└────────────────────────────────────────────────────────────┘
```

#### フィルター項目一覧

| フィルター | UI部品 | APIパラメータ | 備考 |
|-----------|--------|-------------|------|
| 時間範囲プリセット | `<select>` | `from` + `to` | 1h/6h/24h/7d/カスタム |
| 時間範囲カスタム | `<input datetime-local>` × 2 | `from`, `to` | プリセット=カスタム時に展開 |
| エージェント | `<select>` (動的) | `agent_id` | グローバルログ画面のみ表示 |
| モデル | `<select>` (動的) | `model` | ログから一覧を動的取得 |
| エラーのみ | `<checkbox>` | `errors_only=true` | |
| Bot iteration除外 | `<checkbox>` | `exclude_bot_iter=true` | デフォルトOFF |
| 全文検索 | `<input text>` | `q` | prompt/response JSON内を検索、400msデバウンス、2文字以上 |

#### 状態管理

```typescript
interface LlmLogFilters {
  timePreset: "1h" | "6h" | "24h" | "7d" | "custom";
  from?: string;           // ISO8601（カスタム時）
  to?: string;             // ISO8601（カスタム時）
  agentId?: string;        // グローバルログ画面のみ
  model?: string;
  errorsOnly: boolean;
  excludeBotIter: boolean;
  q?: string;
}
```

- URL queryパラメータに同期（リロード後も状態維持）
- フィルター変更時は 400ms デバウンスで API 再フェッチ

### 6.4 ログカード設計（LogCard）

#### コンパクト表示（未展開）

```
┌────────────────────────────────────────────────────────────┐
│ ▶  [claude-3-5-sonnet]  [session:abc123]  [🔧 tool calls]  │  ← 1行目: バッジ群
│    2026-03-24 21:37   ↑1,234  ↓456  ∑1,690  ⚡ 230ms      │  ← 2行目: メタ情報
│    "ユーザーのリクエストを処理して..."                       │  ← 3行目: テキストプレビュー
└────────────────────────────────────────────────────────────┘
```

**chevronアイコンの強調（現状の問題を修正）:**
```tsx
<button className="w-full text-left group">
  {/* 左端に大きめchevronを常時表示 */}
  <span className="material-symbols-outlined text-xl text-on-surface-variant 
                   group-hover:text-primary transition-colors shrink-0">
    {expanded ? "keyboard_arrow_down" : "keyboard_arrow_right"}
  </span>
  {/* カード全体のhoverスタイル */}
  <div className="group-hover:bg-surface-container-high/50 ...">
```

#### 展開後の表示（LogDetail）

```
┌────────────────────────────────────────────────────────────┐
│ ▼  [claude-3-5-sonnet]  ... (コンパクト表示と同じ)          │
├────────────────────────────────────────────────────────────┤
│ [📤 レスポンスへジャンプ] ← 展開直後に表示するショートカット  │  ← ★新規追加
├─ LLMリクエスト ────────────────────────────────────────────│
│  model / temp / max_tokens サマリー                         │
│  Messages (5件):                                            │
│   [SYSTEM #1] ▶ (展開で全文表示)                           │
│   [USER #2]   メッセージ内容...                             │
│   [ASSISTANT #3] レスポンス内容...                          │
│  Tools (12件): [折りたたみ]                                 │
├─ LLMレスポンス ────────────────────────────────────────────│  ← ★レスポンスへジャンプ先
│  finish_reason: tool_calls                                 │
│  ↑1,234 ↓456 ∑1,690 (キャッシュヒット: 800)                │
│  Tool Calls:                                               │
│   build exec  (引数JSON)                                   │
└────────────────────────────────────────────────────────────┘
```

**「レスポンスへジャンプ」ボタン:**
```tsx
// 展開エリアの先頭に表示
<div className="py-2 px-3 flex items-center gap-2 bg-surface-container-high/50 
                border-b border-outline-variant">
  <button
    onClick={() => responseRef.current?.scrollIntoView({ behavior: "smooth" })}
    className="btn-text text-label-sm py-1 px-3"
  >
    <span className="material-symbols-outlined text-sm">arrow_downward</span>
    レスポンスへジャンプ
  </button>
</div>
```

### 6.5 最後尾ジャンプFAB（ScrollToBottomFab）

```tsx
// 画面右下に固定表示
// スクロールが最下部200px以上離れている時のみ表示

function ScrollToBottomFab() {
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    const handleScroll = () => {
      const fromBottom =
        document.documentElement.scrollHeight -
        window.scrollY -
        window.innerHeight;
      setVisible(fromBottom > 200);
    };
    window.addEventListener("scroll", handleScroll, { passive: true });
    return () => window.removeEventListener("scroll", handleScroll);
  }, []);

  if (!visible) return null;

  return (
    <button
      onClick={() =>
        window.scrollTo({ top: document.body.scrollHeight, behavior: "smooth" })
      }
      className="fixed bottom-20 right-4 md:bottom-6 md:right-6 z-50
                 flex items-center gap-2 px-4 py-2.5 rounded-full shadow-elevation-3
                 bg-primary text-primary-on
                 hover:shadow-elevation-4 transition-all"
      aria-label="最新ログへジャンプ"
    >
      <span className="material-symbols-outlined text-lg">arrow_downward</span>
      <span className="text-label-md font-medium hidden sm:inline">最新へ</span>
    </button>
  );
}
```

※ モバイルはボトムナブ（56px）の上に表示するため `bottom-20`、デスクトップは `bottom-6`。

### 6.6 JSON シンタックスハイライト（軽量実装）

外部ライブラリ不要。`highlightJson()` ユーティリティ関数で対応:

```typescript
// web/src/utils/highlightJson.ts
export function highlightJson(jsonStr: string): string {
  return jsonStr
    .replace(/("(\\u[a-zA-Z0-9]{4}|\\[^u]|[^\\"])*"(\s*:)?|\b(true|false|null)\b|-?\d+(?:\.\d*)?(?:[eE][+\-]?\d+)?)/g,
      (match) => {
        let cls = 'text-orange-500 dark:text-orange-400'; // number
        if (/^"/.test(match)) {
          if (/:$/.test(match)) {
            cls = 'text-blue-600 dark:text-blue-400';    // key
          } else {
            cls = 'text-green-600 dark:text-green-400';  // string
          }
        } else if (/true|false/.test(match)) {
          cls = 'text-purple-600 dark:text-purple-400';  // boolean
        } else if (/null/.test(match)) {
          cls = 'text-gray-400';                          // null
        }
        return `<span class="${cls}">${match}</span>`;
      }
    );
}
```

使用方法:
```tsx
<pre
  className="text-xs font-mono whitespace-pre-wrap break-all overflow-x-auto"
  dangerouslySetInnerHTML={{ __html: highlightJson(JSON.stringify(parsed, null, 2)) }}
/>
```

---

## 7. セッション一覧（/sessions）

### 7.1 現状の問題

- フィルタなし
- セッション数が増えると探しにくい
- `SessionCard` の情報が見づらい

### 7.2 改善

```
┌─────────────────────────────────────────────────┐
│ セッション                                        │
├─────────────────────────────────────────────────┤
│ [🟢 アクティブ▼]  [エージェント▼]  [🔍 検索...]  │  ← フィルターバー
├─────────────────────────────────────────────────┤
│ アクティブ (2件)                                  │  ← グループヘッダー
│ ┌────────────────────────────────────────────┐  │
│ │ [ICON] セッションテーマ           [🟢 active]│  │
│ │         エージェントC  Discord #general        │  │
│ │         Turn 47  2分前                      │  │
│ └────────────────────────────────────────────┘  │
│ 完了済み (10件)                                  │
│ ┌────────────────────────────────────────────┐  │
│ │ ...                                         │  │
└─────────────────────────────────────────────────┘
```

### 7.3 SessionCard 改善点

```tsx
// 追加表示項目
- agent_name（どのエージェントのセッションか）
- last_activity_at（最終活動時刻 → relative time: "2分前"）
- turn_number は維持
- Discord情報（guild/channel）は維持

// 削除（過剰）
- mode（discordかどうかはアイコンで判断可能）
- phase（詳細ページで見れば十分）
- participant_count（詳細ページで見れば十分）
```

---

## 8. ルーティング変更まとめ

### 8.1 新規ルート

```tsx
// App.tsx に追加
<Route path="/llm-logs" element={<LlmLogsGlobal />} />
```

### 8.2 変更ルート

```tsx
// 旧: /agents/:id/analytics → 削除（/agents/:id/logs に統合）
// 旧: /agents/:id/edit → /agents/:id/settings にリダイレクト
// 旧: /agents/:id/persona → /agents/:id/settings にリダイレクト
// 旧: /agents/:id/co-agents → /agents/:id/settings にリダイレクト
// 旧: /agents/:id/trusted-users → /agents/:id/settings にリダイレクト
// 旧: /agents/:id/channels → /agents/:id/settings にリダイレクト
// 旧: /agents/:id/allowed-commands → /agents/:id/settings にリダイレクト

// 新: /agents/:id/settings → AgentSettings（アコーディオン統合設定）
// 新: /agents/:id/logs → AgentLogs（stats + 生ログ統合）
```

---

## 9. 新規コンポーネント一覧

### 9.1 pages/

| ファイル | 説明 |
|---------|------|
| `pages/AgentSettings.tsx` | 設定タブ（アコーディオン形式） |
| `pages/AgentLogs.tsx` | ログタブ（stats + 生ログ統合） |
| `pages/LlmLogsGlobal.tsx` | グローバルLLMログ（/llm-logs） |

### 9.2 components/home/

| ファイル | 説明 |
|---------|------|
| `ActiveSessionsMini.tsx` | アクティブセッション（最大3件） |
| `RecentLlmActivity.tsx` | 最近のLLM活動（最新5件） |
| `AgentsMiniGrid.tsx` | エージェントミニグリッド |

### 9.3 components/llm-logs/

| ファイル | 説明 |
|---------|------|
| `LlmLogFilterBar.tsx` | フィルターバー |
| `LlmLogCard.tsx` | ログカード（コンパクト・展開） |
| `LlmLogDetail.tsx` | 展開後の詳細 |
| `LlmLogStats.tsx` | 統計セクション |
| `ScrollToBottomFab.tsx` | 最下部ジャンプFAB |
| `types.ts` | 型定義 |

### 9.4 components/agent-settings/

| ファイル | 説明 |
|---------|------|
| `IdentitySection.tsx` | アイデンティティ設定 |
| `PersonaSection.tsx` | ペルソナ設定 |
| `DiscordBotSection.tsx` | Discord Bot設定（AgentOverviewから移動） |
| `ChannelsSection.tsx` | チャンネル設定 |
| `CoAgentsSection.tsx` | コエージェント |
| `TrustedUsersSection.tsx` | 信頼ユーザー |
| `AllowedCommandsSection.tsx` | 許可コマンド |

### 9.5 utils/

| ファイル | 説明 |
|---------|------|
| `utils/highlightJson.ts` | JSONシンタックスハイライト |
| `utils/relativeTime.ts` | 相対時刻表示（"2分前"） |

---

## 10. API拡張仕様

### 10.1 既存エンドポイントの拡張

#### LLMログ
```
GET /api/agents/:id/llm-logs
  追加パラメータ:
    &offset=N           ページネーション
    &from=ISO8601       開始日時
    &to=ISO8601         終了日時
    &model=NAME         モデルフィルタ
    &errors_only=true   エラーのみ
    &exclude_bot_iter=true
    &q=TEXT             全文検索

  レスポンス変更:
    旧: LlmLog[]
    新: { logs: LlmLog[], total: number, limit: number, offset: number }
```

### 10.2 新規エンドポイント

```
# グローバルLLMログ（全エージェント横断）
GET /api/llm-logs
  パラメータ: 上記と同じ + &agent_id=ID

# モデル一覧（フィルターのドロップダウン用）
GET /api/agents/:id/llm-logs/models
  → string[]

# グローバルモデル一覧
GET /api/llm-logs/models
  → string[]

# セッション一覧（エージェント情報付き）
GET /api/sessions
  追加レスポンスフィールド:
    + agent_name: string
    + last_activity_at: string | null
```

---

## 11. 実装優先順位

### Phase 1 — 即効性（既存UIの修正）

1. **レイアウト崩れ修正** — `overflow-hidden`, `min-w-0`, `break-all` 全コンポーネントに適用
2. **AgentLayoutのタブ削減** — 10タブ → 5タブ（既存タブは新タブにリダイレクト）
3. **LLMログのchevron強調** — `keyboard_arrow_right` を大きく・常時表示
4. **ScrollToBottomFab** — LLMログ画面に追加

### Phase 2 — 情報設計

5. **AgentSettings統合ページ** — アコーディオン形式で全設定を1ページに
6. **AgentLogs統合** — stats + 生ログを1ページに（タブ切り替え）
7. **AgentOverview刷新** — アクティブセッション・今日の統計を表示

### Phase 3 — フィルタ機能

8. **LLMログフィルターバー** — バックエンドAPI拡張 + フロント接続
9. **セッション一覧フィルター** — ステータス・エージェント
10. **エージェント一覧検索** — クライアントサイド

### Phase 4 — 拡張

11. **グローバルLLMログ（/llm-logs）** — 新規ページ + バックエンドAPI
12. **Home画面刷新** — アクティブセッション + 最近のLLM活動
13. **JSONシンタックスハイライト**

---

## 12. 既存コンポーネントの扱い

| コンポーネント | 判定 | 対応 |
|-------------|------|------|
| `AgentCard` | ✅ 維持・小改善 | 「最終活動」追加 |
| `SessionCard` | 🔧 改善 | 表示項目整理 |
| `Collapsible` | ✅ 維持 | llm-logsディレクトリへ移動 |
| `MessageCard` | ✅ 維持 | そのまま |
| `ToolsSection` | ✅ 維持 | そのまま |
| `UsageBar` | ✅ 維持 | そのまま |
| `FinishBadge` | ✅ 維持 | そのまま |
| `StatsSection` | ✅ 維持 | `LlmLogStats.tsx` へ移動 |
| `AgentOverview` (ActionCards) | ❌ 廃止 | 概要ダッシュボードに刷新 |
| `AgentAnalytics` | 🔧 統合 | AgentLogsに統合 |
| `AgentIdentityEdit` | 🔧 統合 | AgentSettings.IdentitySection |
| `PersonaEdit` | 🔧 統合 | AgentSettings.PersonaSection |

---

*この設計書に基づいて各画面を実装することで、設計変更による手戻りを防ぐ。*  
*実装順は Phase 1 から順番に。各フェーズで動作確認後、次フェーズへ。*
