# LLMログUI 設計書

**作成日**: 2026-03-24  
**ステータス**: Draft  
**対象ファイル**: `web/src/pages/AgentLlmLogs.tsx`

---

## 1. 現状の問題点

| # | 問題 | 影響 |
|---|------|------|
| 1 | フィルタ機能なし（時間・モデル・全文検索） | 大量ログから目的のログを見つけられない |
| 2 | カードが展開可能であることが視覚的に不明確 | ユーザーが操作方法を理解できない |
| 3 | 展開後に最新部分（レスポンス）へジャンプできない | 長いrequest（systemプロンプト等）でスクロール地獄 |
| 4 | レイアウト崩れ（横スクロール発生） | JSON表示やモデル名等が画面からはみ出す |
| 5 | ページネーションなし（limit選択のみ） | 大量ログ時のパフォーマンス問題 |

---

## 2. ページ全体のレイアウト

```
┌─────────────────────────────────────────────────────────┐
│ [📋 LLMログ] ページタイトル          [🔄更新] [設定]    │  ← ヘッダー行
├─────────────────────────────────────────────────────────┤
│ 📊 統計サマリー (折りたたみ可)                           │  ← StatsSection（既存）
├─────────────────────────────────────────────────────────┤
│ 🔍 フィルターバー                                         │  ← FilterBar（新規）
│ [時間範囲▼] [モデル▼] [エラー▼] [🔍 全文検索____] [クリア] │
├─────────────────────────────────────────────────────────┤
│ ログ件数: 42件 / 全100件                                  │  ← ResultsInfo（新規）
├─────────────────────────────────────────────────────────┤
│ ┌───────────────────────────────────────────────────┐   │
│ │ ▶ [claude-3-5-sonnet] [session:abc123] [tool calls]│   │  ← LogCard（改善）
│ │   2026-03-24 21:37  1,234↑ / 456↓ / 1,690 ⚡230ms │   │
│ │   "ユーザーのリクエストを処理して..."                │   │
│ └───────────────────────────────────────────────────┘   │
│ ┌─ 展開状態 ─────────────────────────────────────────┐   │
│ │ ▼ [claude-3-5-sonnet]  ...                          │   │
│ │  ┌─ LLMリクエスト ─────────────────────────────┐   │   │
│ │  │  messages (5件) / tools (12件)              │   │   │
│ │  └─────────────────────────────────────────────┘   │   │
│ │  ┌─ LLMレスポンス ─────────────────────────────┐   │   │  ← 「ここへジャンプ」ボタン
│ │  │  finish_reason: tool_calls / 1,690 tokens  │   │   │
│ │  └─────────────────────────────────────────────┘   │   │
│ └─────────────────────────────────────────────────────┘   │
│                                                           │
│           [もっと読み込む (次の20件)]                      │  ← LoadMoreButton
└─────────────────────────────────────────────────────────┘
                               ↑
                   [⬇ 最新へジャンプ] ← 画面下部固定ボタン（ScrollToBottomFab）
```

---

## 3. コンポーネント構成

### ファイル構成

```
web/src/pages/
  AgentLlmLogs.tsx          ← メインページ（既存、リファクタ対象）

web/src/components/
  llm-logs/                 ← 新規ディレクトリ
    LlmLogFilterBar.tsx     ← フィルターバー
    LlmLogCard.tsx          ← ログカード（コンパクト表示）
    LlmLogDetail.tsx        ← 展開後の詳細表示
    LlmLogStats.tsx         ← 統計セクション（既存StatsSection切り出し）
    ScrollToBottomFab.tsx   ← 最後尾ジャンプFAB
    types.ts                ← 型定義（既存をここに移動）
```

### コンポーネント責務

| コンポーネント | 責務 |
|--------------|------|
| `AgentLlmLogs` | 状態管理・API呼び出し・全体レイアウト |
| `LlmLogFilterBar` | フィルター入力UI・クエリパラメータ管理 |
| `LlmLogCard` | 1件のログのコンパクト表示・展開トグル |
| `LlmLogDetail` | 展開後の詳細（リクエスト/レスポンス） |
| `LlmLogStats` | 統計サマリー・棒グラフ |
| `ScrollToBottomFab` | 画面下部固定の「最新へジャンプ」ボタン |

---

## 4. フィルター仕様

### 4.1 フィルター項目

| フィルター | UI部品 | クエリパラメータ | 備考 |
|-----------|--------|---------------|------|
| 時間範囲（開始） | `<input type="datetime-local">` | `from` | ISO8601形式 |
| 時間範囲（終了） | `<input type="datetime-local">` | `to` | ISO8601形式 |
| プリセット時間 | `<select>` | `from` + `to` を計算 | 直近1時間/6時間/24時間/7日 |
| モデル | `<select>` (動的: ログから一覧取得) | `model` | 複数選択非対応（v1） |
| エラーのみ | `<input type="checkbox">` | `errors_only=true` | |
| Bot iteration除外 | `<input type="checkbox">` | `exclude_bot_iter=true` | デフォルトOFF |
| 全文検索 | `<input type="text">` | `q` | prompt/response JSON内を検索 |

### 4.2 フィルターバー状態管理

```typescript
// LlmLogFilterBar の props / state
interface LlmLogFilters {
  from?: string;       // ISO8601
  to?: string;         // ISO8601
  model?: string;
  errorsOnly: boolean;
  excludeBotIter: boolean;
  q?: string;          // 全文検索テキスト
}
```

- フィルター変更時は **デバウンス 400ms** でAPIを再呼び出し
- URLクエリパラメータに同期（ページリロード後も状態維持）
- 「クリア」ボタンで全フィルター初期化

### 4.3 全文検索の仕様

- 検索対象: `prompt`（JSON文字列）と `response`（JSON文字列）
- バックエンドで `LIKE '%q%'` 検索（SQLite）
- 最小文字数: 2文字以上で発動
- ハイライト表示はv1では非対応（v2で検討）

---

## 5. ログカード設計（LlmLogCard）

### 5.1 コンパクト表示（未展開）

```
┌─────────────────────────────────────────────────────────────┐
│ ▶  [claude-3-5-sonnet-20241022] [session:abc123…]           │
│    2026-03-24 21:37:45    ↑1,234  ↓456  ∑1,690  ⚡ 230ms   │
│    "ユーザーのリクエストを受け取り、以下のツールを..."         │
└─────────────────────────────────────────────────────────────┘
```

**設計ポイント:**
- 左端に `chevron_right` アイコン（展開中は `expand_more`）を **常時表示** し、クリック可能であることを明示
- カード全体がクリック可能な `<button>` タグ
- タイムスタンプ: `requested_at` を優先（なければ `created_at`）
- トークン数は `↑(prompt) ↓(completion) ∑(total)` の3点セット
- レスポンスのテキストプレビューを1行表示（80文字でトランケート）

### 5.2 展開後の表示（LlmLogDetail）

展開後は `LogDetail` コンポーネントをそのまま使用。  
**改善点:**

1. **展開直後に「レスポンスへジャンプ」ボタン**を表示
   - 展開エリアのトップに固定表示
   - クリックでレスポンスセクションまで `scrollIntoView()` する
   - `ref` を `Collapsible[title="LLMレスポンス"]` の先頭要素に付与

2. **シンタックスハイライト**
   - 現在は `<pre>` の plain text 表示
   - `JSON.stringify(parsed, null, 2)` の出力に対してキーを `text-blue-600`、文字列値を `text-green-600`、数値を `text-orange-500` でカラーリング（軽量な自前実装）
   - ライブラリ追加は不要（`highlightJson()` ユーティリティ関数を実装）

### 5.3 展開の視覚的手がかり強化

現状の問題: chevronが小さく、カード全体が「クリックできる」とわかりにくい

改善策:
```tsx
// カード全体にhoverスタイルを追加
<button
  onClick={() => setExpanded(!expanded)}
  className="w-full text-left group"  // groupを追加
>
  {/* chevronを大きく、prominentに */}
  <span className="material-symbols-outlined text-xl text-primary 
                   group-hover:text-primary transition-colors">
    {expanded ? "expand_less" : "expand_more"}
  </span>
  ...
</button>
```

加えて、未展開カードの右端に `「クリックして展開」` のツールチップ（`title` 属性）を設定。

---

## 6. 最後尾ジャンプボタン（ScrollToBottomFab）

### 6.1 表示条件

- ページが十分にスクロール可能な場合（スクロール位置が最下部から 200px 以上離れている）に表示
- スクロールが最下部に達したら非表示

### 6.2 実装

```tsx
// ScrollToBottomFab.tsx
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
      onClick={() => window.scrollTo({ top: document.body.scrollHeight, behavior: "smooth" })}
      className="fixed bottom-6 right-6 z-50 
                 flex items-center gap-2 px-4 py-2 rounded-full shadow-lg
                 bg-primary text-on-primary
                 hover:bg-primary/90 transition-all"
      aria-label="最新ログへジャンプ"
    >
      <span className="material-symbols-outlined text-lg">arrow_downward</span>
      <span className="text-label-sm font-medium">最新へ</span>
    </button>
  );
}
```

### 6.3 配置

`AgentLlmLogs` の最下部（ポータルで `document.body` に `createPortal`）

---

## 7. レイアウト崩れ防止

### 7.1 横スクロール発生箇所

| 場所 | 原因 | 修正方法 |
|------|------|---------|
| ログカードのバッジ行 | flex-wrap未設定 | `flex-wrap gap-2` を追加 |
| JSON pre要素 | `whitespace-pre` + 長い行 | `whitespace-pre-wrap break-all` |
| モデル名バッジ | 長いモデル名 | `max-w-[200px] truncate` |
| カード全体 | 親コンテナの `overflow-x: auto` | `overflow-hidden` に変更 |

### 7.2 レイアウト原則（必須クラス）

```tsx
// カードコンテナ
<div className="card-elevated overflow-hidden min-w-0">

// テキストを含む要素
<p className="truncate min-w-0">  {/* 1行トランケート */}
<pre className="whitespace-pre-wrap break-all overflow-x-auto max-w-full">  {/* コードブロック */}

// フレックスアイテム
<div className="flex min-w-0">
  <span className="truncate flex-1 min-w-0">...</span>
</div>
```

### 7.3 コードブロックの最大幅

```css
/* Tailwindカスタム（既存スタイルに追加） */
.llm-code-block {
  max-width: 100%;
  overflow-x: auto;
  word-break: break-all;
  white-space: pre-wrap;
}
```

---

## 8. API仕様（拡張）

### 8.1 既存エンドポイント

```
GET /api/agents/:id/llm-logs
  既存パラメータ: ?limit=N
```

### 8.2 追加クエリパラメータ

```
GET /api/agents/:id/llm-logs
  ?limit=N           件数上限（既存、デフォルト20）
  &offset=N          ページネーション用オフセット（新規）
  &from=ISO8601      開始日時フィルタ（新規）
  &to=ISO8601        終了日時フィルタ（新規）
  &model=NAME        モデル名フィルタ（新規）
  &errors_only=true  エラーログのみ（新規）
  &exclude_bot_iter=true  bot_iteration除外（新規）
  &q=TEXT            全文検索（prompt/responseを対象）（新規）
```

### 8.3 レスポンス形式変更

ページネーション対応のためレスポンス形式を変更:

```typescript
// 現在: LlmLog[]
// 変更後:
interface LlmLogsResponse {
  logs: LlmLog[];
  total: number;    // フィルタ後の総件数
  limit: number;
  offset: number;
}
```

### 8.4 追加エンドポイント

```
GET /api/agents/:id/llm-logs/models
  → string[]  （そのエージェントで使用されたモデル名一覧）
  ※ フィルターのモデル選択肢のために追加
```

### 8.5 バックエンド変更箇所

```
crates/server/src/api/llm_logs.rs
  - LlmLogsQuery に offset, from, to, model, errors_only, exclude_bot_iter, q を追加

crates/db/src/queries.rs
  - list_llm_logs() のシグネチャを LlmLogsFilter を受け取るよう変更
  - WHERE句にフィルタ条件を動的追加
  - LIMIT/OFFSET対応
  - COUNT(*) サブクエリで total を返す
  - list_llm_log_models() を追加（DISTINCT model SELECT）
```

---

## 9. フロントエンドのAPI型定義

```typescript
// web/src/components/llm-logs/types.ts

export interface LlmLog {
  id: string;
  agent_id: string;
  session_id: string | null;
  model: string | null;
  prompt: string;
  response: string;
  tool_calls: string | null;
  latency_ms: number | null;
  prompt_tokens: number | null;
  completion_tokens: number | null;
  total_tokens: number | null;
  error_code: string | null;
  error_body: string | null;
  requested_at: string | null;
  trigger_message_id: string | null;
  cache_read_tokens: number | null;
  cache_creation_tokens: number | null;
  is_bot_iteration: boolean;
  created_at: string;
}

export interface LlmLogsResponse {
  logs: LlmLog[];
  total: number;
  limit: number;
  offset: number;
}

export interface LlmLogFilters {
  from?: string;
  to?: string;
  model?: string;
  errorsOnly: boolean;
  excludeBotIter: boolean;
  q?: string;
}

export type TimePreset = "1h" | "6h" | "24h" | "7d" | "custom";
```

---

## 10. 実装優先順位

### Phase 1（最優先・すぐやる）
1. **レイアウト崩れ修正** — `overflow-hidden`, `min-w-0`, `break-all` の追加
2. **展開の視覚化強化** — chevronアイコンをprominentに、カードhoverスタイル
3. **最後尾ジャンプFAB** — `ScrollToBottomFab` 追加

### Phase 2（フィルタ機能）
4. **フィルターバー（UI）** — `LlmLogFilterBar` コンポーネント作成
5. **バックエンドAPI拡張** — クエリパラメータ追加
6. **フィルターバー接続** — フロント ↔ API連携

### Phase 3（UX改善）
7. **展開後の「レスポンスへジャンプ」ボタン**
8. **シンタックスハイライト**（軽量自前実装）
9. **ページネーション**（offset対応）
10. **URLクエリパラメータ同期**（フィルター状態の永続化）

---

## 11. 影響範囲

| 変更対象 | 変更種別 | 備考 |
|---------|---------|------|
| `web/src/pages/AgentLlmLogs.tsx` | 改修 | コンポーネント分割、フィルター追加 |
| `web/src/components/llm-logs/` | 新規 | 5ファイル新規作成 |
| `crates/server/src/api/llm_logs.rs` | 改修 | クエリパラメータ拡張 |
| `crates/db/src/queries.rs` | 改修 | フィルタ対応クエリ |
| `crates/server/src/api/mod.rs` | 改修 | `/llm-logs/models` ルート追加 |

---

## 12. 参考: 既存コンポーネント活用

現在の `AgentLlmLogs.tsx` に含まれる以下のコンポーネントは品質が高く、リファクタ後も維持:

- `Collapsible` — セクション折りたたみ（`crates/components/llm-logs/Collapsible.tsx` へ移動）
- `MessageCard` — チャットメッセージ1件表示
- `ToolsSection` — ツール定義一覧
- `UsageBar` — トークン使用量バー
- `FinishBadge` — finish_reason バッジ
- `CollapsibleText` — 長テキストの展開表示
- `StatsSection` — 統計グラフ
