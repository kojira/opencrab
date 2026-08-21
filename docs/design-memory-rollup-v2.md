# memory_index 階層ロールアップ設計 v2

**作成日**: 2026-04-04
**ステータス**: Draft
**前提**: `design-daily-log-index.md` で daily_log → memory_index_nodes の投入は実装済み。本設計はその上位概念として、セッションログ・daily_log 両方のインデックスを対象とした階層的ロールアップ・プロンプト最適化を定義する。

---

## 0. エグゼクティブサマリー

現在の memory_index は **topic サマリー全件がフラットにプロンプトに展開される**。
topic が数百件に達するとプロンプトだけで数万トークンを消費し、実質的な会話予算を圧迫する。
本設計では以下の4つの柱で改善する:

1. **キーの短縮** — node_id を 130文字超 → 10文字以下に
2. **日時メタデータ** — サマリーに日時情報を付与
3. **階層ロールアップ** — 時間単位 → 日次 → 週次 → 月次 → 年次の自動集約
4. **ペルソナ視点の要約** — 機械的第三者 → エージェント一人称
5. **プロンプト構築の改修** — 時間軸ベースの予算配分、topic 全件展開の廃止

---

## 1. 現状の問題点（定量）

### 1.1 node_id のトークン消費

現在の node_id パターン:

| ノードタイプ | パターン | 例 | 文字数 |
|---|---|---|---|
| topic (session_log) | `topic-{agent_id}-{session_id}-{first}-{last}` | `topic-agent:nostarou:main-sess_abc123def456-1-20` | ~60–130 |
| topic (daily_log) | `{agent_id}:daily_log:topic:{date}:{i}` | `agent:nostarou:main:daily_log:topic:2026-03-25:0` | ~55 |
| period | `period-{agent_id}-{YYYY-MM}` / `{agent_id}:daily_log:period:{YYYY-MM}` | — | ~40–60 |

**問題**: topic 122件 × 平均 node_id 80文字 ≈ 9,760文字がキーだけで消費。
プロンプト展開形式 `- [node_id] title: summary` だと、node_id だけで全体の約 26% を占める。

### 1.2 サマリーに日時情報がない

topic ノードには `date_from` / `date_to` カラムがあるが、session_log 由来の topic では **常に NULL**。
日時情報がないため:
- エージェントが「先週の会話」を特定できない
- ロールアップ時にどの topic が同じ時間帯に属するか判定できない

### 1.3 要約が機械的

現在の要約プロンプト:
```
以下の会話メッセージからタイトル（10語以内）と要約（2-3文）を生成してください。
```

結果例: *「ユーザーがRustのビルドエラーについて質問し、エージェントがCargoの設定を確認して解決した。」*

→ 第三者視点のイベントログであり、エージェント自身の記憶として自然ではない。

### 1.4 daily_log サマリーがプロンプトに使われていない

`DailyLogIndexer` で生成された日次サマリーは DB に格納されるが、`build_conversation_string()` は `get_topic_nodes_for_session()` しか呼ばない。daily_log の豊富なコンテキストがプロンプトに反映されない。

### 1.5 ロールアップが未実装

`merge_topics()` は「period 内の topic 数が閾値超過したら統合」する単純なマージのみ。
時間ベースの階層集約（日次→週次→月次）は存在しない。

---

## 2. キーの短縮

### 2.1 方針

node_id に **連番ベースの短縮キー** を導入する。

| 現行 | 新規 | 例 |
|---|---|---|
| `topic-agent:nostarou:main-sess_abc-1-20` | `t{seq}` | `t42` |
| `period-agent:nostarou:main-2026-03` | `p{seq}` | `p7` |
| `{agent_id}:daily_log:daily:2026-03-25` | `d{seq}` | `d15` |
| (hourly ノード) | `h{seq}` | `h8` |
| (weekly ノード) | `w{seq}` | `w5` |
| (monthly ノード) | `m{seq}` | `m3` |
| (yearly ノード) | `y{seq}` | `y1` |
| `root-agent:nostarou:main` | `r0` | `r0` |

### 2.2 スキーマ変更

```sql
-- memory_index_nodes に short_id カラムを追加
ALTER TABLE memory_index_nodes ADD COLUMN short_id TEXT;

-- エージェント内でユニーク
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id
    ON memory_index_nodes(agent_id, short_id);
```

### 2.3 採番ロジック

```rust
/// short_id を自動採番する。
/// prefix: 't' (topic), 'p' (period), 'd' (daily), 's' (session), 'h' (hourly), 'w' (weekly), 'm' (monthly), 'y' (yearly)
fn next_short_id(conn: &Connection, agent_id: &str, prefix: &str) -> String {
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(CAST(SUBSTR(short_id, 2) AS INTEGER))
         FROM memory_index_nodes
         WHERE agent_id = ?1 AND short_id LIKE ?2",
        params![agent_id, format!("{prefix}%")],
        |row| row.get(0),
    ).ok().flatten();
    format!("{prefix}{}", max.unwrap_or(0) + 1)
}
```

### 2.4 プロンプトへの影響

変更前: `- [topic-agent:nostarou:main-sess_abc123-1-20] Rustビルドエラー: ユーザーが...`
変更後: `- [t42] Rustビルドエラー: ユーザーが...`

**削減効果**: 122 topic × 平均 70文字削減 ≈ **8,540文字（約 2,800トークン）削減**

### 2.5 後方互換と生ログ参照の導線維持

- `retrieve_memory_nodes` は `short_id` と `id` の両方で検索可能にする
- 既存ノードには `backfill_short_ids()` マイグレーションで連番を付与
- **生ログ参照の保証**: `retrieve_memory_nodes` で `short_id` から元の topic → `session_log` まで辿れることを保証する。ロールアップで上位ノードに集約されても、子ノード（topic）は削除されず、`parent_id` チェーンを辿ることで元の生ログに到達可能
- **suppressed ノードの取得**: `suppressed = true` のノードもプロンプト自動展開はされないが、`retrieve_memory_nodes` で明示的に `short_id` / `id` を指定すれば取得可能。忘却はプロンプト展開からの除外のみであり、DB の生データは一切消さない
- **検索フロー**: `short_id (t42)` → `memory_index_nodes.id` → `source_node_ids` → `session_log` の生メッセージ

---

## 3. 日時メタデータ

### 3.1 方針

すべての topic/daily ノードに `date_from` と `date_to` を確実に設定する。

### 3.2 session_log 由来 topic の日時取得

```rust
// IndexBuilder::build_incremental 内で topic ノード作成時
let date_from = session_logs.iter()
    .filter_map(|l| l.created_at.as_deref())
    .min()
    .map(|s| s[..10].to_string()); // "2026-03-25"

let date_to = session_logs.iter()
    .filter_map(|l| l.created_at.as_deref())
    .max()
    .map(|s| s[..10].to_string());
```

### 3.3 プロンプト展開時の日時表示

```
- [t42] (03-25) Rustビルドエラー: Cargoの設定を修正して解決した
- [t43] (03-25〜03-26) FTv10データセット: 3日かけてデータ準備を完了
```

日付は `MM-DD` 形式（同年内）、年が異なる場合は `YYYY-MM-DD`。

---

## 4. 階層ロールアップ

### 4.1 全体像

```
[Raw Logs]  (session_log / daily_log)
     ↓  IndexBuilder / DailyLogIndexer（既存）
[topic ノード]   depth=4, 個別会話/トピックの要約
     ↓  HourlyRollup（新規）  ← トリガー: 同一時間帯に topic が N件以上
[hourly ノード]  depth=3.5, 「13時台の活動」
     ↓  DailyRollup（新規）   ← トリガー: 日替わり OR 日内 topic が予算超過
[daily ノード]   depth=3, 「3/25の1日まとめ」
     ↓  WeeklyRollup（新規）  ← トリガー: 週替わり（月曜基準）
[weekly ノード]  depth=2.5, 「3/24週のまとめ」
     ↓  MonthlyRollup（新規） ← トリガー: 月替わり
[monthly ノード] depth=2, 「3月のまとめ」
     ↓  YearlyRollup（新規）  ← トリガー: 年替わり
[yearly ノード]  depth=1, 「2026年のまとめ」
```

> **注**: depth は概念的なもの。実装では `node_type` で区別し、`depth` カラムは木構造の実際の深さを表す。

### 4.2 ロールアップの階層定義

トークン上限は固定値ではなく、子ノードのトークン総量に応じた **動的レンジ** で決定する。
活発な日/週/月には予算を多く、スカスカな期間には少なく配分する（密度按分方式）。
上位階層の予算内で兄弟ノード間のトークン量を、各ノードの子トークン総量の比率で按分する。

| 階層 | node_type | short_id prefix | 入力 | トークンレンジ（下限〜上限） | トリガー条件 |
|---|---|---|---|---|---|
| topic | `topic` | `t` | 生ログチャンク | 300 tok/件 | IndexBuilder 既存処理 |
| hourly | `hourly` | `h` | 同一時間帯の topic 群 | 500 tok | 同一時間帯に topic ≥ 5件 |
| daily | `daily` | `d` | その日の topic/hourly 群 | 200〜1,500 tok | 日替わり（UTC+9 基準） |
| weekly | `weekly` | `w` | 7日分の daily 群 | 500〜2,500 tok | 月曜 00:00（JST）到来 |
| monthly | `monthly` | `m` | 当月の weekly 群 | 1,000〜4,000 tok | 月初 |
| yearly | `yearly` | `y` | 12ヶ月分の monthly 群 | 2,000〜5,000 tok | 年初（1/1 00:00 JST） |

#### 密度按分の計算例

ある月に weekly が 4件あり、子トークン総量がそれぞれ 3,000 / 1,000 / 5,000 / 1,000 = 合計 10,000 の場合:
- monthly の予算が 3,000 tok なら、各 weekly の配分は 900 / 300 / 1,500 / 300 tok
- ただし各階層の下限・上限でクリップする（weekly の下限 500 tok を下回る場合は 500 に切り上げ、超過分を他から調整）

### 4.3 ロールアップエンジン

```rust
pub struct RollupEngine {
    db: Arc<Mutex<Connection>>,
    llm_client: Arc<dyn LlmClient>,
    model: String,
    agent_id: String,
    persona_prompt: String,  // SOUL.md から抽出
}

impl RollupEngine {
    /// 全階層のロールアップを実行する。
    /// 呼び出しタイミング: セッション終了後 / 定期バッチ（1時間ごと）
    pub async fn run(&self) -> Result<RollupStats> {
        self.rollup_hourly().await?;
        self.rollup_daily().await?;
        self.rollup_weekly().await?;
        self.rollup_monthly().await?;
        self.rollup_yearly().await?;
        Ok(stats)
    }

    /// 時間単位ロールアップ
    async fn rollup_hourly(&self) -> Result<()> {
        // 1. 未ロールアップの topic を date_from でグループ化（時間帯別）
        // 2. 同一時間帯に topic ≥ 5件なら LLM で要約統合
        // 3. hourly ノード作成、元 topic の parent_id を hourly に付け替え
    }

    /// 日次ロールアップ
    async fn rollup_daily(&self) -> Result<()> {
        // 1. 前日以前で daily ノードが未作成の日を検出
        // 2. その日の topic/hourly を集約して LLM 要約
        // 3. daily_log 由来の daily ノードが既にあればマージ
        //    （session_log + daily_log の統合ビュー）
    }

    /// 週次ロールアップ
    async fn rollup_weekly(&self) -> Result<()> {
        // 1. 完了した週（月曜〜日曜）で weekly 未作成を検出
        // 2. その週の daily を集約して LLM 要約
    }

    /// 月次ロールアップ
    async fn rollup_monthly(&self) -> Result<()> {
        // 1. 完了した月で monthly 未作成を検出
        // 2. その月の weekly を集約して LLM 要約
    }

    /// 年次ロールアップ
    async fn rollup_yearly(&self) -> Result<()> {
        // 1. 完了した年で yearly 未作成を検出
        // 2. その年の monthly（最大12件）を集約して LLM 要約
        // 3. 上限 3,000〜5,000トークンに圧縮
        // 4. 3年で yearly 3件 = 最大 15,000トークン
    }
}
```

### 4.4 ロールアップのデータフロー詳細

#### 4.4.1 hourly ロールアップ

```
入力: 同一時間帯（例: 2026-03-25 13:00〜13:59）の topic ノード群
条件: topic数 ≥ 5
出力: 1つの hourly ノード

hourly ノード:
  id: "hourly-{agent_id}-{date}T{hour}"
  short_id: "h{seq}"
  parent_id: → daily ノード（未作成なら一時的に period）
  title: "3/25 13時台"
  summary: LLM要約（500トークン以内）
  date_from: "2026-03-25"
  date_to: "2026-03-25"

元 topic ノードの処理:
  - parent_id を hourly ノードに付け替え
  - 削除はしない（retrieve_memory_nodes で詳細取得可能に保つ）
```

#### 4.4.2 daily ロールアップ

```
入力: 1日分の hourly + 直属 topic（hourly に含まれなかった topic）
条件: 日替わりが発生（前日以前のデータ）
出力: 1つの daily ノード

daily_log 由来のサマリーとの統合:
  - DailyLogIndexer が既に daily ノードを作成済みの場合
  - session_log 由来の活動サマリーと daily_log のサマリーを LLM でマージ
  - 統合後の daily ノードは source_type = 'merged' とする
```

#### 4.4.3 weekly / monthly / yearly ロールアップ

```
weekly:
  入力: 7日分の daily ノード
  週境界: 月曜 00:00 JST
  出力: 1つの weekly ノード（short_id: "w{seq}"）

monthly:
  入力: 当月の weekly ノード群
  月境界: 月初 1日 00:00 JST
  出力: 1つの monthly ノード（short_id: "m{seq}"）

yearly:
  入力: 12ヶ月分の monthly ノード群
  年境界: 1月1日 00:00 JST
  出力: 1つの yearly ノード（short_id: "y{seq}"）
  トークンレンジ: 2,000〜5,000 tok（密度按分）
  想定: 3年で yearly 3件 = 最大 15,000 トークン
```

### 4.5 ロールアップ後のツリー構造

```
[root] r0
  ├─ [yearly] y1 "2025年: ..."  ← 完了した年は yearly に圧縮
  ├─ [monthly] m1 "2026年2月: FTv9学習とNostr統合が中心の月"
  │    ├─ [weekly] w1 "2/3週: FTv9データセット設計"
  │    │    ├─ [daily] d1 "2/3: FTv9学習開始、kojiraとデータセット設計を議論"
  │    │    │    ├─ [hourly] h1 "2/3 14時台"
  │    │    │    │    ├─ [topic] t1 "FTv9データセット形式の決定"
  │    │    │    │    └─ [topic] t2 "Alpaca vs ShareGPT形式の比較"
  │    │    │    └─ [topic] t3 "夜のNostr雑談"
  │    │    ├─ [daily] d2 "2/4: ..."
  │    │    ...
  │    ├─ [weekly] w2 "2/10週: FTv9実行と記憶消失事件"
  │    ...
  ├─ [monthly] m2 "2026年3月: ..."
  │    ...
  └─ [weekly] w9 "3/31週（進行中）"  ← 未完了の週は weekly として暫定作成
       ├─ [daily] d52 "4/1: ..."
       └─ [daily] d53 "4/2: ..."
            ├─ [topic] t120 "..."  ← 最新の topic はそのまま露出
            └─ [topic] t121 "..."
```

### 4.6 トークン予算とロールアップトリガー

ロールアップは **時間ベース** と **トークン数ベース** の OR 条件で発動する。

```
時間トリガー:
  hourly: 1時間経過 AND 同一時間帯に topic ≥ 5
  daily:  日替わり
  weekly: 月曜到来
  monthly: 月初到来
  yearly: 年初到来（1/1 00:00 JST）

トークントリガー:
  同一親ノードの子サマリー合計が上限を超えた場合、
  時間条件を待たずにロールアップを実行する。

  例: ある日の topic が急増して合計 5,000 トークンを超えた
  → daily のトークン上限 800 を超過
  → 即座に daily ロールアップを発動（日替わりを待たない）
```

---

## 5. ペルソナ視点の要約

### 5.1 方針

要約は **エージェント自身の記憶** として生成する。SOUL.md のペルソナ情報を要約プロンプトに注入し、一人称で、感情・判断理由・関係性を含む要約を生成する。

### 5.2 要約プロンプトテンプレート

```
あなたは {persona_name} です。以下はあなたが体験した会話のログです。
あなた自身の記憶として、以下の観点を含めて要約してください:

1. 学んだこと・技術知見（新しく知ったこと、理解が深まったこと）
2. 判断の理由（なぜそうしたか、どういう選択肢があったか）
3. 関係性・感情（誰と何をしたか、どう感じたか）
4. 失敗と教訓（うまくいかなかったこと、次回への学び）

口調: {persona_tone_example}
一人称で書いてください。客観的なイベントログではなく、あなたの記憶として。

JSON形式で出力:
{{"title": "20字以内", "summary": "200字以内"}}

ログ:
{content}
```

### 5.3 ロールアップ時の要約プロンプト

階層が上がるほど抽象度を上げる:

| 階層 | 焦点 | 要約長 |
|---|---|---|
| topic | 具体的な出来事・技術的詳細 | 100–200字 |
| hourly | その時間帯の活動の流れ | 200–300字 |
| daily | 1日の主な成果・出来事・学び | 300–500字 |
| weekly | 週のハイライト・進捗・方向転換 | 500–800字 |
| monthly | 月のテーマ・達成・未達・感情の流れ | 800–1200字 |
| yearly | 年間の大きなテーマ・成長・転機・未達成 | 1500–2500字 |

### 5.4 注目ポイント抽出指示

ロールアップ時、入力サマリー群から以下を重点的に残す:

```
優先度 高:
  - 失敗と教訓（同じミスを繰り返さないために）
  - 技術的決定とその理由（将来の判断材料として）
  - 関係性の変化（誰が何をしてくれたか）

優先度 中:
  - 新しい技術知見・発見
  - プロジェクトのマイルストーン

優先度 低（圧縮対象）:
  - 定型的なやりとり（挨拶、確認、雑談）
  - 既に上位階層のサマリーに含まれる情報の重複
```

---

## 6. プロンプト構築の改修

### 6.1 現行の問題

`build_conversation_string()` のコンパクション時:

```rust
// 現行: session の topic ノード全件を展開
let topics = get_topic_nodes_for_session(conn, agent_id, session_id)?;
let summary_section: String = topics.iter()
    .map(|t| format!("- [{}] {}: {}", t.id, t.title, t.summary))
    .collect::<Vec<_>>().join("\n");
```

**問題点**:
- topic が数百件あると summary_section だけで予算の大半を消費
- daily_log のサマリーが一切含まれない
- 時間軸の構造がフラット（1週間前の topic も1時間前の topic も同じ粒度）

### 6.2 新しいプロンプト構築: 時間軸ベースの予算配分

```
[Past context — Memory Summary]

## 今月 (2026-04)
[w9] 3/31週:
  [d52] 4/1: opencrab のメモリインデックス最適化に着手。node_id短縮の設計を始めた
  [d53] 4/2: RollupEngine の設計ドラフトを作成。kojiraにレビュー依頼した
  [t120] (04-03 14:22) ロールアップのトリガー条件を時間+トークン数のOR条件に決定
  [t121] (04-03 15:10) weekly要約のペルソナプロンプトをテスト

## 先月 (2026-03)
[m2] 3月まとめ: opencrab の基盤構築が中心。daily_log indexer を実装し、メモリシステムの骨格ができた月...

## それ以前
[y1] 2025年まとめ: ...（完了した年は yearly に圧縮）
[m1] 2月まとめ: FTv9のファインチューニングに集中。2/11に記憶消失事件が発生し、メモリ管理の重要性を痛感した月...

[Recent conversation]
...最新のログ...
```

### 6.3 予算配分アルゴリズム

```rust
pub fn build_memory_summary(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    budget_tokens: usize,
) -> String {
    // 予算配分:
    //   最新の未ロールアップ topic: 40%（スライディングウィンドウ方式、§7.0）
    //   今週の daily:              25%
    //   今月の weekly:             15%
    //   過去の monthly/yearly:     15%（完了年は yearly に圧縮）
    //   バッファ:                   5%

    let now = chrono::Local::now();
    let today = now.date_naive();
    let week_start = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64);
    let month_start = today.with_day(1).unwrap();

    // 全クエリで suppressed = false のノードのみ取得（§8 選択的忘却）
    
    // 1. 最新の topic（今日 + 未ロールアップ分）
    let recent_topics = get_unrolled_topics(conn, agent_id, session_id);
    let recent_budget = (budget_tokens as f64 * 0.40) as usize;
    let recent_section = render_topics_with_budget(&recent_topics, recent_budget);

    // 2. 今週の daily
    let this_week_dailies = get_daily_nodes_in_range(conn, agent_id, week_start, today);
    let week_budget = (budget_tokens as f64 * 0.25) as usize;
    let week_section = render_dailies_with_budget(&this_week_dailies, week_budget);

    // 3. 今月の weekly
    let this_month_weeklies = get_weekly_nodes_in_range(conn, agent_id, month_start, today);
    let month_budget = (budget_tokens as f64 * 0.15) as usize;
    let month_section = render_weeklies_with_budget(&this_month_weeklies, month_budget);

    // 4. 過去の monthly / yearly
    //    完了した年は yearly に圧縮されているため、
    //    yearly + 当年の past monthly を展開
    let past_yearlies = get_yearly_nodes(conn, agent_id);
    let past_monthlies = get_monthly_nodes_before(conn, agent_id, month_start);
    let past_budget = (budget_tokens as f64 * 0.15) as usize;
    let past_section = render_past_with_budget(&past_yearlies, &past_monthlies, past_budget);

    format!(
        "[Past context — Memory Summary]\n\n\
         ## Recent\n{recent_section}\n\n\
         ## This Week\n{week_section}\n\n\
         ## This Month\n{month_section}\n\n\
         ## Past Months\n{past_section}"
    )
}
```

### 6.4 build_conversation_string の改修

```rust
pub fn build_conversation_string(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    context_budget_tokens: usize,
) -> Result<String> {
    let full = build_full_conversation(conn, session_id);
    if full == "No messages yet." {
        return Ok(full);
    }
    if estimate_tokens(&full) <= context_budget_tokens {
        return Ok(full);
    }

    // ---- 変更点 ----
    // 旧: topic 全件をフラットに展開
    // 新: 時間軸ベースの階層サマリー

    let summary_budget = (context_budget_tokens as f64 * 0.45) as usize;
    let recent_budget = context_budget_tokens - summary_budget;

    let memory_summary = build_memory_summary(conn, agent_id, session_id, summary_budget);
    let recent_header = "\n\n[Recent conversation]\n";
    let overhead = estimate_tokens(&memory_summary) + estimate_tokens(recent_header);
    let remaining = context_budget_tokens.saturating_sub(overhead);

    // indexed_boundary: ロールアップ済みの最後の log_id
    let boundary = get_rollup_boundary(conn, agent_id, session_id);
    let recent_logs = list_session_logs_after_id(conn, session_id, boundary)?;
    let recent_text = fit_logs_to_budget(&recent_logs, remaining);

    Ok(format!("{memory_summary}{recent_header}{recent_text}"))
}
```

> **⚠️ 注記（#609 で否決）: 上の擬似コードの「索引境界で直近ウィンドウを切る」方式はそのまま使わないこと**
>
> 直近会話ウィンドウを `indexed_boundary`（記憶索引の到達点 = `get_rollup_boundary`）で切り、
> `list_session_logs_after_id(conn, session_id, boundary)` で `id > boundary` の行だけを
> `fit_logs_to_budget` に渡す方式は、PR #610（issue #609）で**バグと断定され否決**された。
>
> - **索引がライブ末尾に張り付くと `id > boundary` がほぼ空になり**、直近会話が下限フォールバックへ縮退する。
>   実測では、予算 175,000 トークンのうち約 72,000 が未使用のまま、直近の raw 119 件が要約に置き換わっていた。
> - **境界より前で落ちた行は省略マーカーで告知されない**。`fit_logs_to_budget` は渡された集合の内側しか
>   告知できないため、境界で事前に切られた行は**黙って消える**。
> - **現行方式**: 全ログを `fit_logs_to_budget` に渡し、**予算だけ**が窓の大きさを決める（`build_recent_window`）。
>   `list_session_logs_after_id` は#610 で**削除済みで存在しない**。
>
> v2 を実装するときは、この擬似コードの索引境界ベースの切り出しを**そのまま踏襲しないこと**
> （同じ退行を再導入する）。窓の大きさは予算で決める現行方式に合わせる。

---

## 7. エッジケース対策

### 7.0 当日の予算超過: スライディングウィンドウ方式

**シナリオ**: 当日の topic が増えすぎてプロンプト予算を超過し、直近の会話の詳細が消える。

**対策**: 当日分の topic 展開にスライディングウィンドウ方式を適用する。

```
スライディングウィンドウの動作:
  1. 直近2時間の topic → 生展開（圧縮しない、サマリーそのまま）
  2. 2時間より前の今日分 → hourly に圧縮して展開
  3. 予算に収まらなければ古い hourly から段階的に切り詰め

例: 14:30 時点で当日予算 4,000 tok の場合
  12:30〜14:30 の topic → そのまま展開（〜2,500 tok）
  09:00〜12:29 の topic → hourly 3件に圧縮（〜1,500 tok）
  予算超過時 → 09時台の hourly を最初に削除
```

これにより「さっきの会話の詳細が消える」問題を回避し、直近の記憶は常に鮮明に保つ。
時間窓の2時間は設定可能とし、エージェントの活動パターンに応じて調整可能にする。

### 7.1 1時間で予算超過

**シナリオ**: 活発なコーディングセッションで、1時間に topic が 50件生成される。

**対策**:
- hourly ロールアップのトークントリガーが即座に発動
- hourly ノード1つに集約され、プロンプトには hourly の 500トークンサマリーのみ展開
- 元の topic は `retrieve_memory_nodes` で個別取得可能

### 7.2 daily サマリーだけで予算超過

**シナリオ**: 30日分の daily サマリー × 800トークン = 24,000トークン > 予算

**対策**:
- weekly ロールアップにより 7日分が 1,200トークンに圧縮
- 30日 ÷ 7 ≈ 4–5 weekly × 1,200 = 5,000–6,000トークン
- さらに monthly ロールアップで 2,000トークンに圧縮

### 7.3 ロールアップ中の LLM 障害

**対策**:
- ロールアップは冪等設計（同じ入力で再実行しても結果は同じ）
- `rollup_watermark` テーブルで各階層の最終処理日時を記録
- LLM 障害時はスキップし、次回のバッチで再試行
- ロールアップ失敗時は子ノードがそのまま展開される（graceful degradation）

### 7.4 セッション跨ぎの連続会話

**シナリオ**: session_id が変わっても同じトピックの会話が続く

**対策**:
- ロールアップは session_id に依存しない。date_from/date_to ベースで集約
- daily ロールアップで異なるセッションの topic が自然に1日のサマリーに統合される

### 7.5 daily_log と session_log の重複

**シナリオ**: 同じ会話が session_log の topic と daily_log の topic の両方に要約される

**対策**:
- daily ロールアップ時に `source_type` をチェック
- 同じ日の session_log 由来 topic と daily_log 由来 topic を統合
- 統合後のノードは `source_type = 'merged'`
- 重複検出: 日付が同じで title の類似度が高い topic はマージ候補

---

## 8. 選択的忘却 — ハートビート Dream モード

### 8.1 概要

人間が睡眠中に記憶を整理するように、エージェントがハートビートのタイミングで「夢を見る」＝ 記憶の選択的整理を行う機能。エージェント自身が「この記憶はもう詳細を保持する必要がない」と判断した topic をプロンプト展開から除外し、教訓だけを上位ノードに残す。

**重要**: suppressed はプロンプト展開からの除外のみ。DB の生データは一切消さない。

### 8.2 スキーマ

```sql
-- memory_index_nodes に suppressed カラム追加
ALTER TABLE memory_index_nodes ADD COLUMN suppressed BOOLEAN DEFAULT FALSE;
```

### 8.3 エージェントアクション

```rust
/// 指定ノードを suppressed としてマークする。
/// プロンプト展開から除外されるが、retrieve_memory_nodes で明示的に取得可能。
pub fn forget_memory(conn: &Connection, agent_id: &str, node_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE memory_index_nodes SET suppressed = TRUE
         WHERE agent_id = ?1 AND (id = ?2 OR short_id = ?2)",
        params![agent_id, node_id],
    )?;
    Ok(())
}
```

### 8.4 Dream モードの動作

ハートビートの自律アクションに `Dream`（記憶整理）を追加する（既存の Speak / Learn / Idle に加えて）。

```
Dream モードのフロー:
  1. 直近の未整理 topic を LLM に渡す
  2. LLM が各 topic を評価:
     - 「重要 — 保持」: そのまま
     - 「教訓あり — 圧縮」: suppressed = true にマークし、
       教訓を抽出して上位ノード（daily/weekly）のサマリーに含める
     - 「不要 — 忘却」: suppressed = true にマーク
  3. suppressed なノードはプロンプト自動展開から除外
  4. retrieve_memory_nodes では suppressed も取得可能（明示的アクセス）

Dream モードのトリガー:
  - ハートビート実行時にランダムまたは条件付きで選択
  - 夜間帯（23:00〜08:00）のハートビートでは Dream を優先選択
  - 未整理 topic が 20件以上溜まった場合も発動
```

### 8.5 具体例

```
例: ループ暴走の記憶
  [t85] "ロールアップが無限ループ。hourly生成→daily再計算→hourly再生成のサイクルに..."
  
  Dream モード後:
  - t85: suppressed = true
  - 上位ノード（d30 の daily サマリー）に教訓を追記:
    「ロールアップの冪等性が壊れてループした。原因はdaily再計算時にhourlyを削除していたこと。
     教訓: ロールアップは追記のみ、既存ノードの削除は禁止。」
  - t85 自体はプロンプトに展開されないが、retrieve で詳細取得可能
```

---

## 9. スキーマ変更まとめ

```sql
-- 1. short_id カラム追加
ALTER TABLE memory_index_nodes ADD COLUMN short_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_index_nodes_short_id
    ON memory_index_nodes(agent_id, short_id)
    WHERE short_id IS NOT NULL;

-- 2. node_type の拡張（既存: root/period/session/topic/daily）
-- 追加: 'hourly', 'weekly', 'monthly', 'yearly'

-- 3. source_type の拡張（既存: session_log/daily_log）
-- 追加: 'merged'（session_log + daily_log の統合ノード）

-- 4. ロールアップ進捗管理テーブル
CREATE TABLE IF NOT EXISTS rollup_watermark (
    agent_id TEXT NOT NULL,
    rollup_level TEXT NOT NULL,  -- 'hourly' / 'daily' / 'weekly' / 'monthly' / 'yearly'
    last_processed_date TEXT NOT NULL,  -- YYYY-MM-DD
    last_processed_at TEXT NOT NULL,    -- ISO8601
    PRIMARY KEY (agent_id, rollup_level)
);

-- 5. 日時検索用インデックス
CREATE INDEX IF NOT EXISTS idx_memory_index_nodes_date_range
    ON memory_index_nodes(agent_id, node_type, date_from, date_to);

-- 6. suppressed カラム追加（選択的忘却, §8 参照）
ALTER TABLE memory_index_nodes ADD COLUMN suppressed BOOLEAN DEFAULT FALSE;
```

---

## 10. 実装フェーズ

### Phase 1: キーの短縮 + 日時メタデータ（低リスク）

1. `short_id` カラム追加 + マイグレーション
2. `next_short_id()` 採番ロジック実装
3. IndexBuilder / DailyLogIndexer で `short_id` と `date_from`/`date_to` を設定
4. `backfill_short_ids()` で既存ノードに連番付与
5. `build_conversation_string()` のサマリー展開を `short_id` ベースに変更
6. `retrieve_memory_nodes` で `short_id` 検索対応

**見積もり**: 2–3日
**効果**: プロンプトのキー部分で ~2,800トークン削減

### Phase 2: ペルソナ視点の要約（中リスク）

1. `RollupEngine` の要約プロンプトテンプレート実装
2. `agents` テーブルからペルソナ情報を取得して注入
3. IndexBuilder の要約プロンプトをペルソナ視点に変更
4. DailyLogIndexer の要約プロンプトも同様に変更
5. 既存サマリーの再生成コマンド（`rebuild` の拡張）

**見積もり**: 1–2日
**効果**: サマリーの情報密度向上（定性的改善）

### Phase 2.5: 記憶再構築パイプライン（中リスク・Phase 2 依存）

既存の122件の topic サマリーは機械的な第三者視点で書かれており、新しいペルソナ視点の要約システム（Phase 2）を導入しても、元のサマリーが腐っていれば上位の daily → weekly → monthly の要約品質も腐る。Phase 3 の階層ロールアップを正しく機能させるための前提条件として、既存データの再構築を行う。

1. **日時メタデータの逆算補完**
   - 既存 topic で `date_from` / `date_to` が NULL のものを検出
   - `source_node_ids` → `session_log.created_at` から逆算して埋める
   - SQL: `UPDATE memory_index_nodes SET date_from = (SELECT MIN(date(created_at)) FROM session_log WHERE id IN (source_node_ids)) WHERE date_from IS NULL AND node_type = 'topic'`

2. **既存 topic サマリーのペルソナ視点再生成**
   - 元の `session_log` の生ログから Phase 2 のペルソナプロンプトで再要約
   - バッチ処理: 122件 × gemini-flash ≈ $0.02、所要時間 〜5分
   - 再生成前のサマリーは `summary_v1` として退避（ロールバック可能に）

3. **daily → weekly → monthly のゼロ構築**
   - 再生成した topic を使って `RollupEngine.run()` をフル実行
   - 既存の period ノードは deprecated マーク、新しい daily/weekly/monthly で置換

4. **`rebuild_memory_index` API の拡張**
   - 既存の rebuild コマンドに `--full-reconstruct` オプションを追加
   - Phase 2.5 の 1〜3 を一括実行するワンショットコマンド

**見積もり**: 2–3日
**依存**: Phase 2（ペルソナプロンプト）が前提
**注意**: Phase 3 の前に完了させること。腐ったサマリーの上にロールアップを積んでも意味がない

### Phase 3: 階層ロールアップ（高リスク・高リターン）

1. `RollupEngine` 本体の実装
   - hourly → daily → weekly → monthly → yearly の5段階
   - 各階層のトリガーロジック（時間 + トークン数）
   - 密度按分方式の動的トークン配分（§4.2）
2. `rollup_watermark` テーブルとウォーターマーク管理
3. ロールアップの自動トリガー
   - セッション終了後のバックグラウンド実行
   - 定期バッチ（1時間ごと）
4. スライディングウィンドウ方式の実装（§7.0）
5. テスト（単体 + 統合）

**見積もり**: 5–7日
**依存**: Phase 1 の `date_from`/`date_to` 設定 + Phase 2.5 の再構築が前提

### Phase 4: プロンプト構築改修（Phase 3 依存）

1. `build_memory_summary()` 実装
2. `build_conversation_string()` の改修
3. 予算配分の調整・チューニング
4. A/B テスト（旧プロンプト vs 新プロンプト）

**見積もり**: 2–3日

### Phase 5: daily_log 統合（Phase 3 依存）

1. daily ロールアップでの session_log + daily_log マージ
2. `source_type = 'merged'` ノードの生成
3. 重複検出ロジック

**見積もり**: 2–3日

### Phase 6: 選択的忘却 — Dream モード（Phase 3 依存）

1. `suppressed` カラム追加 + マイグレーション
2. `forget_memory()` アクション実装
3. ハートビートの `Dream` 自律アクション実装
4. プロンプト展開で `suppressed = true` ノードを除外
5. `retrieve_memory_nodes` で suppressed ノードの明示的取得を保証
6. テスト（忘却→教訓抽出→上位ノード更新のフロー）

**見積もり**: 3–4日

---

## 11. トークン消費の予測

### 11.1 運用想定

- エージェント1体、1日あたり topic 20件生成
- 1ヶ月で topic 600件、daily 30件
- 半年で topic 3,600件、monthly 6件

### 11.2 プロンプト消費比較

| 方式 | 半年後のプロンプトサマリー | トークン数 |
|---|---|---|
| **現行**: topic 全件フラット展開 | 3,600 topic × 平均 300字 | ~360,000 tok |
| **Phase 1 のみ**: short_id + topic 全件 | 3,600 topic × 平均 230字 | ~276,000 tok |
| **Phase 3+4 完了**: 階層ロールアップ | monthly 6×2000字 + weekly 4×1200字 + daily 7×800字 + topic 20×300字 | ~29,400 tok |

**削減率**: 360K → 29K ≈ **92% 削減**

#### 3年後の予測（yearly 含む）

| 方式 | 3年後のプロンプトサマリー | トークン数 |
|---|---|---|
| **yearly なし**: monthly 36件 × 平均 2,500 tok | 過去月だけで 90,000 tok | ~90,000 tok |
| **yearly あり**: yearly 2件 × 5,000 + 当年 monthly 6件 × 2,500 | 10,000 + 15,000 | ~25,000 tok |

yearly の導入で、長期運用時の past months セクションが **72% 削減** される。

### 11.3 ロールアップの LLM コスト

| 階層 | 頻度 | 入力トークン | 出力トークン | 月間コスト目安（gemini-flash） |
|---|---|---|---|---|
| hourly | ~5回/日 | ~2,000/回 | ~500/回 | ~$0.04 |
| daily | 1回/日 | ~3,000/回 | ~800/回 | ~$0.03 |
| weekly | ~4回/月 | ~5,000/回 | ~1,200/回 | ~$0.02 |
| monthly | 1回/月 | ~8,000/回 | ~2,000/回 | ~$0.01 |
| yearly | 1回/年 | ~30,000/回 | ~5,000/回 | ~$0.001 |
| **合計** | | | | **~$0.10/月** |

→ ロールアップのコストはほぼ無視できる水準。

---

## 12. テスト戦略

各 Phase に対応するテストケースを「入力 → 期待結果」形式で列挙する。
テスト ID は `T-{Phase}.{連番}` で一意に管理する。

---

### 12.1 Phase 1: キー短縮 + 日時メタデータ

#### next_short_id() — 連番採番

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-1.1 | 空テーブルでの初回採番 | agent_id="a1", prefix="t", テーブルに short_id なし | `"t1"` を返す |
| T-1.2 | 既存データがある場合の連番 | テーブルに t1, t2, t3 が存在 → prefix="t" | `"t4"` を返す |
| T-1.3 | prefix 別に独立した連番 | テーブルに t1, t2, h1 が存在 → prefix="h" | `"h2"` を返す（t の連番に影響されない） |
| T-1.4 | agent_id 別に独立した連番 | agent_id="a1" に t1〜t10, agent_id="a2" に t1 → agent_id="a2", prefix="t" | `"t2"` を返す |
| T-1.5 | 欠番がある場合 | テーブルに t1, t3, t5 が存在（t2, t4 は欠番）→ prefix="t" | `"t6"` を返す（MAX+1 方式、欠番は埋めない） |
| T-1.6 | 全 prefix パターン | prefix 各種: t, h, d, w, m, y, p, r | それぞれ `"{prefix}1"` を返す |

#### backfill_short_ids() — 既存ノードへの連番付与

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-1.7 | short_id 未設定ノードへの一括付与 | short_id が NULL の topic 5件 + daily 3件 | topic に t1〜t5、daily に d1〜d3 が付与される |
| T-1.8 | 既に short_id があるノードはスキップ | t1, t2 が設定済み、3件が NULL | NULL の3件に t3, t4, t5 が付与される。t1, t2 は変更なし |
| T-1.9 | 空テーブルでの実行 | ノード 0件 | エラーなく正常終了、変更 0件 |

#### date_from / date_to — session_log からの逆算

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-1.10 | 単一日の session_log から逆算 | topic の source_node_ids が指す session_log の created_at が全て "2026-03-25T14:xx:xx" | date_from="2026-03-25", date_to="2026-03-25" |
| T-1.11 | 複数日にまたがる session_log | created_at が "2026-03-25T23:50:00" と "2026-03-26T00:10:00" | date_from="2026-03-25", date_to="2026-03-26" |
| T-1.12 | source_node_ids が空 | topic に source_node_ids がない | date_from=NULL, date_to=NULL（パニックしない） |

#### retrieve_memory_nodes — short_id / id 両方で検索

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-1.13 | short_id で検索 | query="t42" | id="topic-agent:nostarou:main-sess_abc-1-20" のノードが返る |
| T-1.14 | 元の id で検索 | query="topic-agent:nostarou:main-sess_abc-1-20" | 同じノードが返る |
| T-1.15 | 存在しない short_id | query="t99999" | 空の結果（エラーにならない） |

---

### 12.2 Phase 2: ペルソナ視点要約

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-2.1 | 要約が一人称になっている | persona_name="のすたろう", 会話ログ: Rustビルドエラーの質問と解決 | 要約に「俺が」「〜した」等の一人称表現が含まれる。「ユーザーが」「エージェントが」等の第三者表現を含まない |
| T-2.2 | 注目ポイント4軸が含まれる | 会話ログ: FTv9 のデータセット設計で失敗→修正 | 要約に以下のうち少なくとも2つが含まれる: ①学び/技術知見、②判断理由、③関係性（kojira と〜）、④失敗と教訓 |
| T-2.3 | 要約がトークン上限内 | topic 要約: 300トークン上限 | 生成されたサマリーが 300 トークン以下 |
| T-2.4 | 各階層の要約長が適切 | hourly/daily/weekly/monthly/yearly それぞれの要約生成 | §5.3 の表に従った文字数範囲内: topic 100-200字, hourly 200-300字, daily 300-500字, weekly 500-800字, monthly 800-1200字, yearly 1500-2500字 |
| T-2.5 | ペルソナ情報が空の場合 | persona_name="", persona_tone="" | デフォルトの一人称（「私が〜」）で要約が生成される。エラーにならない |
| T-2.6 | 会話ログが極端に短い | ログが1行のみ:「おはよう」 | title と summary が生成される（空にならない） |

---

### 12.3 Phase 2.5: 記憶再構築パイプライン

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-2.5.1 | 既存 topic に date_from/date_to が埋まる | date_from=NULL の topic 10件、source_node_ids → session_log.created_at が存在 | 10件全てに date_from/date_to が設定される |
| T-2.5.2 | 再生成後のサマリーがペルソナ視点 | 旧サマリー:「ユーザーがビルドエラーを報告し、エージェントが修正した」 | 新サマリーが一人称視点に変換されている。旧サマリーは summary_v1 として退避されている |
| T-2.5.3 | daily → weekly → monthly が正しく構築される | 再生成済み topic 30件（1ヶ月分） | daily ~30件 + weekly ~4件 + monthly 1件が生成される |
| T-2.5.4 | source_node_ids → session_log が存在しない場合 | topic の source_node_ids が指す session_log が削除済み | date_from/date_to は NULL のまま。サマリー再生成はスキップ。エラーにならない |
| T-2.5.5 | --full-reconstruct の冪等性 | rebuild --full-reconstruct を2回連続実行 | 2回目の実行で重複ノードが生成されない。ノード数が1回目と同一 |
| T-2.5.6 | 既存 period ノードが deprecated マーク | 旧 period ノード 5件 | 5件全てに deprecated マークが付く。新しい daily/weekly/monthly で置換される |

---

### 12.4 Phase 3: 階層ロールアップ

#### hourly ロールアップ

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.1 | topic 4件では発動しない | 同一時間帯（13:00〜13:59）に topic 4件 | hourly ノードは生成されない。4件の topic がそのまま残る |
| T-3.2 | topic 5件で発動する | 同一時間帯に topic 5件 | hourly ノード 1件が生成される。5件の topic の parent_id が hourly に付け替えられる |
| T-3.3 | トークン超過で時間待たず発動 | 同一親ノードの子サマリー合計が hourly 上限を超過（topic 3件で合計 2,000 tok） | 5件未満でもロールアップが発動する |
| T-3.4 | 複数時間帯の同時処理 | 13時台に topic 6件、14時台に topic 7件 | hourly ノードが2件（h1, h2）生成される。それぞれの時間帯の topic が正しい hourly に紐づく |
| T-3.5 | 日をまたぐ時間帯 | 23:30〜00:30 に topic 6件 | 23時台と0時台で別の hourly が生成される（日をまたいで1つにしない） |

#### daily ロールアップ

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.6 | 日替わりで発動する | 2026-03-25 の topic 8件 + hourly 1件。現在は 2026-03-26 | daily ノード（d{seq}）が1件生成される。date_from="2026-03-25", date_to="2026-03-25" |
| T-3.7 | 当日分は発動しない | 2026-03-26（今日）の topic 10件 | daily ノードは生成されない（当日はまだ確定していない） |
| T-3.8 | topic 0件の日 | 2026-03-25 に topic が0件 | daily ノードは生成されない |

#### weekly ロールアップ

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.9 | 月曜 00:00（JST）で発動 | 前週（月〜日）の daily 7件が存在。現在は翌月曜 | weekly ノード 1件が生成される |
| T-3.10 | 週の途中では発動しない | 水曜時点で今週の daily 3件 | weekly ノードは生成されない |
| T-3.11 | daily が欠けている週 | 月〜日のうち月・火・金のみ daily がある（水木土日は活動なし） | weekly ノード 1件が生成される（欠損日があっても正常処理） |

#### monthly ロールアップ

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.12 | 月初で発動 | 3月の weekly 4件が存在。現在は 4/1 | monthly ノード 1件が生成される |
| T-3.13 | 月の途中では発動しない | 4月15日時点で当月の weekly 2件 | monthly ノードは生成されない |

#### yearly ロールアップ

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.14 | 年初で発動 | 2025年の monthly 12件が存在。現在は 2026-01-01 | yearly ノード 1件が生成される。short_id="y{seq}" |
| T-3.15 | 年の途中では発動しない | 2026-06-15 時点 | 2026年の yearly ノードは生成されない |
| T-3.16 | monthly が少ない年 | 2025年に monthly が3件のみ（10月〜12月に活動開始） | yearly ノード 1件が生成される（monthly が12未満でも正常処理） |

#### 冪等性

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.17 | ロールアップ2回実行 | 同一データに対して `RollupEngine.run()` を2回実行 | 1回目と2回目でノード数が同一。重複ノードが生成されない |
| T-3.18 | 部分実行後の再実行 | hourly まで実行後に中断 → 再度 `run()` を実行 | hourly は再生成されず、daily 以降が正常に生成される |

#### LLM 障害時

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.19 | LLM がタイムアウト | hourly ロールアップ中に LLM が応答なし | ロールアップをスキップ。子 topic がそのまま展開される（graceful degradation） |
| T-3.20 | LLM が不正な JSON を返す | LLM の応答が `{"title": ...}` ではなく自由テキスト | パースエラーをキャッチしてスキップ。子ノードがそのまま展開される |
| T-3.21 | 障害後の再試行 | T-3.19 でスキップされた hourly が存在 → 次回バッチ実行 | スキップされていた時間帯が正常にロールアップされる |

#### 動的トークン量（密度按分）

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.22 | 活発な日は上限に近い | 1日に topic 30件、合計 9,000 tok | daily のサマリーが上限 1,500 tok に近い値で生成される |
| T-3.23 | スカスカな日は下限に近い | 1日に topic 2件、合計 600 tok | daily のサマリーが下限 200 tok に近い値で生成される |
| T-3.24 | 兄弟ノード間の予算配分 | monthly 予算 3,000 tok。weekly 4件の子トークン総量: 3,000 / 1,000 / 5,000 / 1,000（合計 10,000） | 各 weekly の配分が比率按分: 900 / 500* / 1,500 / 500* tok（*下限 500 でクリップ。超過分 400 tok を残り2件から調整） |
| T-3.25 | 全兄弟が下限に張り付く場合 | monthly 予算 1,500 tok。weekly 4件の子トークン総量: 各 100 | 各 weekly の配分が下限 500 tok にクリップ。合計 2,000 > 予算 1,500 の場合、均等に切り詰めて各 375 tok |

#### rollup_watermark

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-3.26 | ウォーターマーク記録 | hourly ロールアップ完了 | rollup_watermark に (agent_id, "hourly", 処理日, 処理時刻) が記録される |
| T-3.27 | ウォーターマーク以前はスキップ | watermark の last_processed_date="2026-03-25"。2026-03-24 のデータが未ロールアップ | 2026-03-24 はウォーターマーク以前なので再処理されない |

---

### 12.5 Phase 4: プロンプト構築

#### スライディングウィンドウ

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-4.1 | 直近2時間の topic は生展開 | 現在 14:30。12:30〜14:30 の topic 5件 | 5件全てがサマリーそのまま展開される（hourly に圧縮されない） |
| T-4.2 | 2時間超の topic は hourly 圧縮 | 現在 14:30。09:00〜12:29 の topic 15件（hourly 3件に集約済み） | hourly 3件の圧縮サマリーとして展開される |
| T-4.3 | 予算超過時の切り詰め | 当日予算 4,000 tok。直近2時間で 3,500 tok、それ以前の hourly 3件で 1,500 tok（合計 5,000 tok） | 古い hourly から順に削除して予算内に収める。直近2時間の topic は最後まで残る |

#### 予算配分

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-4.4 | 配分比率が正しい | budget_tokens=10,000 | recent: ~4,000 tok (40%), this_week: ~2,500 tok (25%), this_month: ~1,500 tok (15%), past: ~1,500 tok (15%), buffer: ~500 tok (5%) |
| T-4.5 | 該当データがない階層はスキップ | 初回利用で weekly/monthly/yearly が存在しない | recent と this_week のみ展開。エラーにならない。余った予算は recent に再配分 |

#### 予算超過

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-4.6 | 古い階層から切り詰め | budget_tokens=8,000。全階層展開すると 12,000 tok | past → this_month → this_week の順に切り詰め。recent は最後まで保持 |
| T-4.7 | 極端に小さい予算 | budget_tokens=1,000 | recent の topic のみ展開。他の階層は全てカット。エラーにならない |

#### build_memory_summary 出力

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-4.8 | 出力が予算内 | budget_tokens=10,000。半年分のデータ | 出力トークン数 ≤ 10,000 |
| T-4.9 | 出力フォーマット | 通常データ | "[Past context — Memory Summary]" ヘッダーで始まり、"## Recent" / "## This Week" / "## This Month" / "## Past Months" の4セクションを含む |
| T-4.10 | 日付表示形式 | 同年内の topic + 前年の yearly | 同年内: "MM-DD" 形式、前年: "YYYY-MM-DD" 形式 |

---

### 12.6 Phase 5: daily_log 統合

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-5.1 | session_log + daily_log の重複マージ | 2026-03-25 に session_log 由来 topic "Rustビルドエラー修正" と daily_log 由来 topic "ビルドエラー対応" | 統合された daily ノード 1件。source_type="merged"。両方の情報を含む統合サマリー |
| T-5.2 | 重複なしの場合 | session_log 由来 topic 3件と daily_log 由来 topic 2件（内容が異なる） | daily ノードに5件全ての情報が含まれる。source_type="merged" |
| T-5.3 | daily_log のみの日 | session_log 由来 topic 0件、daily_log 由来 topic 2件 | daily ノードが生成される。source_type="daily_log" |
| T-5.4 | session_log のみの日 | session_log 由来 topic 5件、daily_log 由来 topic 0件 | daily ノードが生成される。source_type="session_log" |

---

### 12.7 Dream モード（選択的忘却）

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-D.1 | suppressed ノードがプロンプトに展開されない | topic t42 を suppress → build_memory_summary() 実行 | 出力に t42 の内容が含まれない |
| T-D.2 | suppressed ノードが retrieve で取得できる | topic t42 を suppress → retrieve_memory_nodes(query="t42") | t42 のノードが返る（suppressed=true の状態で） |
| T-D.3 | forget_memory アクションで suppressed が true になる | forget_memory(node_id="t42") 実行 | memory_index_nodes の t42 の suppressed が true に更新される |
| T-D.4 | forget_memory を short_id で実行 | forget_memory(node_id="t42") | short_id="t42" のノードが suppressed=true になる |
| T-D.5 | forget_memory を元の id で実行 | forget_memory(node_id="topic-agent:nostarou:main-sess_abc-1-20") | 同ノードが suppressed=true になる |
| T-D.6 | Dream 中に LLM が「不要」判断 | Dream モード実行。LLM が topic t85 を「不要」と判定 | t85 に suppressed=true が設定される |
| T-D.7 | Dream 中に LLM が「教訓あり — 圧縮」判断 | Dream モード実行。LLM が topic t85 を「教訓あり」と判定 | t85 に suppressed=true が設定され、教訓が上位ノード（daily）のサマリーに追記される |
| T-D.8 | Dream 中に LLM が「重要 — 保持」判断 | Dream モード実行。LLM が topic t90 を「重要」と判定 | t90 の suppressed は false のまま変更なし |
| T-D.9 | suppressed ノードの子も展開されない | hourly h5 を suppress。h5 配下に topic t10, t11 | build_memory_summary() の出力に h5, t10, t11 いずれも含まれない |
| T-D.10 | 存在しない node_id で forget_memory | forget_memory(node_id="t99999") | エラーにならない（影響行 0件で正常終了） |

---

### 12.8 回帰テスト

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-R.1 | IndexBuilder 既存テスト群 | 既存のテストスイート実行 | 全テスト PASS |
| T-R.2 | DailyLogIndexer 既存テスト群 | 既存のテストスイート実行 | 全テスト PASS |
| T-R.3 | build_conversation_string 既存テスト | 既存のテストスイート実行 | 全テスト PASS |
| T-R.4 | short_id 未設定ノードへのフォールバック | short_id が NULL のノードのみ存在する状態で build_memory_summary() 実行 | 元の id で展開される。エラーにならない |
| T-R.5 | suppressed カラム未存在時のフォールバック | マイグレーション前の DB スキーマで build_memory_summary() 実行 | suppressed フィルタをスキップして全ノード展開。エラーにならない |
| T-R.6 | node_type が旧形式のみ | node_type に hourly/weekly/monthly/yearly が存在しない（topic/daily/period のみ） | 既存の topic/daily がそのまま展開される。上位階層が存在しなくてもエラーにならない |

---

### 12.9 統合テスト

| ID | テストケース | 入力 | 期待結果 |
|---|---|---|---|
| T-I.1 | 半年分の全階層ロールアップ | ダミーデータ: 180日分、1日平均 topic 20件（合計 ~3,600件） | hourly / daily / weekly / monthly が全て生成される。build_memory_summary(budget=10,000) の出力が 10,000 tok 以内 |
| T-I.2 | 3年分で yearly が正しく生成 | ダミーデータ: 3年分（2023〜2025年完了、2026年進行中） | yearly 3件（y1, y2, y3）が生成される。各 yearly のトークン数が 2,000〜5,000 tok の範囲内 |
| T-I.3 | 記憶再構築パイプライン | 既存 topic 122件（date_from=NULL、第三者視点サマリー） | ① 122件に date_from/date_to が設定される、② サマリーがペルソナ視点に再生成される、③ daily → weekly → monthly の階層が構築される |
| T-I.4 | エンドツーエンド: 生ログ → プロンプト | session_log 100件を投入 → IndexBuilder → RollupEngine → build_memory_summary() | プロンプト出力が予算内に収まり、階層構造（Recent / This Week / This Month / Past Months）が正しく展開される |
| T-I.5 | Dream → ロールアップの連携 | Dream モードで topic 5件を suppress → 次回ロールアップ実行 | ロールアップは suppressed=false の topic のみを入力として使用。suppressed topic の教訓は上位ノードに反映済み |
| T-I.6 | 全階層の parent_id チェーン | 3年分のデータでフルロールアップ後 | 任意の topic から parent_id チェーンを辿って root まで到達できる: topic → hourly → daily → weekly → monthly → yearly → root |

---

## 13. 関連ドキュメント

- `design-daily-log-index.md` — DailyLogIndexer の設計（本設計の前提）
- `DESIGN.md` § 3 — Memory Index の基本アーキテクチャ
- `crates/core/src/memory_index/index_builder.rs` — 現行の IndexBuilder
- `crates/core/src/memory/daily_log_indexer.rs` — 現行の DailyLogIndexer
- `crates/server/src/process.rs` — build_conversation_string() / build_agent_context()

---

## 14. 未解決事項

| 項目 | 内容 | 決定期限 |
|---|---|---|
| hourly の必要性 | daily だけで十分な可能性。実運用データで判断 | Phase 3 開始前 |
| ペルソナ情報の取得元 | agents テーブルの personality カラム vs SOUL.md ファイル | Phase 2 開始前 |
| 予算配分の比率 | 40/25/15/15/5 は仮値。実データでチューニング | Phase 4 |
| ロールアップの実行タイミング | セッション終了後 vs 定期バッチ vs 両方 | Phase 3 |
| Dream モードの発動頻度 | 夜間帯優先 vs topic 数トリガー vs 両方。実運用で調整 | Phase 6 |
| 密度按分のクリッピング方式 | 下限切り上げ時の超過分を均等按分 vs 比率按分 | Phase 3 |

### 解決済み（本 v2 更新で正式化）

| 項目 | 解決内容 |
|---|---|
| ~~既存サマリーの移行~~ | Phase 2.5（記憶再構築パイプライン）として正式化。§10 参照 |
| ~~monthly を超える階層~~ | yearly 階層として正式採用。node_type: 'yearly', short_id: 'y{seq}'。§4 参照 |
