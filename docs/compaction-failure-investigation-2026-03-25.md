# エージェントA コンパクション44時間失敗 未検知調査レポート

**調査日:** 2026-03-25  
**対象:** opencrab プロジェクト — メモリインデックスビルド失敗の検知不能問題  
**コード変更:** なし（調査のみ）

---

## TL;DR（要約）

エージェントAのコンパクション（メモリインデックスビルド）が44時間失敗し続けたにもかかわらず検知されなかった原因は、**バックグラウンドビルドの結果を `let _ = ...` で完全廃棄している**ことが主因。フォールバック機構がサイレントにマスキングし、アラート機構が存在しないため、誰にも通知されなかった。

---

## コンパクションの仕組み（概要）

```
ユーザーメッセージ受信
  ↓
build_conversation_string() で会話履歴を構築
  ↓
全文が token_budget を超えるか？
  ├─ NO → そのまま全文を使用（コンパクション不要）
  └─ YES → コンパクション発動
              ↓
        get_topic_nodes_for_session() でトピック要約取得
              ↓
        トピック要約が存在するか？
              ├─ YES → [Past context summary] + 最近ログで構築（品質良好）
              └─ NO  → build_truncated_conversation() でフォールバック（単純切り捨て）

run_agent_response() の後でバックグラウンドでインデックスビルドを実行
  ↓
tokio::spawn(async move {
    if unindexed >= threshold {
        IndexBuilder::build_incremental(...).await
    }
})
```

**重要：コンパクションは「トピック要約なし→単純切り捨て」でも動作し続ける。失敗してもエージェントAは返答できる。**

---

## 根本原因（6つの設計上の問題）

### 原因1（主因）: エラー結果のサイレント廃棄

**ファイル:** `crates/server/src/process.rs:699`

```rust
let _ = opencrab_core::memory_index::IndexBuilder::build_incremental(
    &index_db,
    &index_agent_id,
    &llm_adapter,
    &index_model,
    config.batch_size as usize,
)
.await;
```

`let _ = ...` によりビルド結果が完全に廃棄される。  
ビルドが `Err(...)` を返しても:
- `tracing::error!()` が出ない
- Discord通知が飛ばない
- メトリクスが増加しない
- ログに何も残らない

ビルド**開始前**には `tracing::info!("Starting background memory index build")` があるが、**完了・失敗の結果は何もログされない**。

### 原因2: LLMエラーがメトリクスに記録されない

**ファイル:** `crates/server/src/process.rs:697`

```rust
let llm_adapter = LlmRouterAdapter::new(index_llm_router);  // ← メトリクスコンテキストなし
```

通常の会話では `.with_metrics(ctx)` を付けてLLMログをDBに保存するが、  
バックグラウンドインデックスビルドでは `LlmRouterAdapter::new()` のみ。

→ LLM API障害（rate limit / auth error / timeout等）があっても `llm_logs` テーブルに**何も残らない**。

### 原因3: index_builder内のLLMエラーはフォールバック要約で隠蔽される

**ファイル:** `crates/core/src/memory_index/index_builder.rs:240-247`

```rust
let summary = match llm.chat(request).await {
    Ok(resp) => { /* 正常処理 */ }
    Err(_) => LlmSummary {
        title: format!("Topic (logs {first_log_id}-{last_log_id})"),
        summary: "Summary generation failed".to_string(),  // ← エラーを隠蔽
    },
};
```

LLM呼び出しが失敗しても `build_incremental` は `Ok(...)` を返す。  
つまり原因1の `let _ = ...` による廃棄を使わなくても、LLMエラー単体では検知できない。  
（ただし、この場合はフォールバック要約つきでtopicノードが作成される）

### 原因4: フォールバック機構がサイレントに失敗をマスクする

**ファイル:** `crates/server/src/process.rs:176-181`

```rust
if topics.is_empty() {
    // フォールバック: 要約がない場合は最新ログを予算内で切り詰め
    return build_truncated_conversation(conn, session_id, context_budget_tokens);
}
```

トピックノードが存在しない場合（インデックスビルドが一度も成功していない場合）、  
`[Note: Earlier messages were omitted due to context length...]` というヘッダー付きで切り捨て会話を返す。

→ エージェントAは**正常に返答できる**（品質は低下するが動作継続）  
→ ユーザー・モニターには「コンパクション失敗」が見えない

### 原因5: 並行ビルドの重複実行リスク

**ファイル:** `crates/server/src/process.rs:668-710`

```rust
// run_agent_response() の最後で毎回 tokio::spawn で起動
tokio::spawn(async move {
    if unindexed >= config.threshold {
        let _ = IndexBuilder::build_incremental(...).await;
    }
});
```

ハートビートや複数メッセージが重なると複数の `tokio::spawn` が起動する。  
各タスクは独立して「unindexed >= threshold → ビルド開始」と判断するため、  
**同じタイミングで複数の並行ビルドが起動する可能性**がある。

→ 実際の障害時、並行ビルドが競合してDBロックを取り合い、`map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?` が連鎖エラーとなりビルドが全失敗するシナリオが考えられる。  
→ すべてのエラーは原因1の `let _ = ...` で廃棄されるため検知不能。

### 原因6: 監視・アラート機構の不在

- `GET /api/agents/{id}/memory/index` エンドポイントでインデックス状態は確認できる（`unindexed_logs`, `watermark` を返す）
- しかし**このエンドポイントを定期的にポーリングする仕組みがない**
- **ウォーターマークが長時間更新されていない場合にアラートを飛ばす仕組みがない**
- `heartbeat_log` テーブルにインデックスビルドの成否が記録されない
- ダッシュボードにインデックスビルドの最終成功時刻や連続失敗回数が表示されない

---

## 失敗が44時間検知されなかったシナリオ

```
[t=0h]     なんらかのトリガーで IndexBuilder::build_incremental が失敗
             ↓ let _ = ... で廃棄 → 誰も気づかない

[t=0h~44h] ハートビートが数分〜数十分おきに発火するたびに
             run_agent_response → tokio::spawn → build_incremental → 失敗 → 廃棄
             ↓
           コンパクション時は topics.is_empty() → build_truncated_conversation でフォールバック
             ↓
           エージェントAは返答継続（品質低下しているが動作する）
             ↓
           tracing::info!("Starting background memory index build") は出続けるが
           エラーログがないためログ監視に引っかからない

[t=44h]    人間が「おかしい」と気づいてコードを確認 → 初めて発覚
```

---

## 考えられる失敗の直接原因

（コードから特定できる候補。実際の原因は runtime ログの確認が必要）

| 候補 | 説明 |
|------|------|
| **LLM API障害** | Anthropic等のAPI障害/レートリミット。原因3のフォールバックで隠蔽される場合とならない場合がある |
| **DBロック競合** | 並行ビルドによる `std::sync::Mutex` 取得失敗。`map_err(...)? ` → `Err` → `let _ =` で廃棄 |
| **ウォーターマーク未進行** | ビルドが成功しているがウォーターマーク更新前に失敗、次回も同じログを再ビルド → 無限ループ |
| **threshold超過なし** | ログが threshold (デフォルト20件) に達していないためビルドが一度も起動していない |

---

## 修正提案

### 優先度HIGH: エラーログの追加

**`crates/server/src/process.rs`** のバックグラウンドビルドに結果ログを追加:

```rust
match opencrab_core::memory_index::IndexBuilder::build_incremental(
    &index_db,
    &index_agent_id,
    &llm_adapter,
    &index_model,
    config.batch_size as usize,
).await {
    Ok(result) => {
        tracing::info!(
            agent_id = %index_agent_id,
            nodes_created = result.nodes_created,
            logs_indexed = result.logs_indexed,
            "Background memory index build completed"
        );
    }
    Err(e) => {
        tracing::error!(
            agent_id = %index_agent_id,
            error = %e,
            "Background memory index build FAILED"
        );
    }
}
```

### 優先度HIGH: 連続失敗時のDiscord通知

失敗カウンターをDBまたはインメモリで管理し、N回連続失敗したらDiscordに通知:

```rust
// 失敗カウンターをメモリ上で管理（Arc<AtomicU32>）
if consecutive_failures >= MAX_FAILURES {
    // Discord通知を飛ばす
}
```

### 優先度MEDIUM: メトリクスコンテキストの追加

バックグラウンドビルド時もLLMログを記録:

```rust
let llm_adapter = LlmRouterAdapter::new(index_llm_router)
    .with_metrics(MetricsContext {
        session_id: Some(format!("index-build-{index_agent_id}")),
        // ...
    });
```

### 優先度MEDIUM: 並行ビルドの防止

ビルド実行中フラグ（`Arc<AtomicBool>`）を使って重複起動を防止:

```rust
if !is_building.compare_exchange(false, true, ...).is_ok() {
    return; // すでにビルド中
}
// ビルド完了後に is_building.store(false)
```

### 優先度LOW: ダッシュボードへの最終成功時刻表示

`GET /api/agents/{id}/memory/index` のレスポンスに「最終成功時刻」と「連続失敗回数」を追加し、ダッシュボードで可視化。

---

## 結論

**エージェントAのコンパクション44時間失敗が検知されなかった核心理由は3つ:**

1. **`let _ = build_incremental(...).await`** — 失敗を完全廃棄（エラーログ/通知なし）
2. **フォールバック機構** — コンパクションは単純切り捨てで継続動作し、失敗を隠蔽
3. **監視機構の不在** — ウォーターマーク停止を検知するアラートが存在しない

設計として「フォールバックで動作継続」は正しいが、「エラーを完全廃棄」は問題。  
最低限 `tracing::error!()` を追加するだけで、ログ監視で44分以内に検知できるようになる。
