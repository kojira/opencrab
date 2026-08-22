# 設計書: コンパクション失敗検知の改善

**作成日:** 2026-03-25  
**対象ファイル:** `crates/server/src/process.rs`  
**関連調査:** `docs/compaction-failure-investigation-2026-03-25.md`  
**優先度:** HIGH  
**コード変更なし（設計書のみ）**

---

## 概要

エージェントAのメモリインデックスビルド（コンパクション）が44時間失敗し続けたにもかかわらず検知されなかった。  
根本原因は `let _ = build_incremental(...).await` による**エラー結果の完全廃棄**。  
本設計書では、最小コード変更で最大の検知能力を得る修正方針を定義する。

---

## 修正対象箇所

### ファイル

```
crates/server/src/process.rs
```

### 現在のコード（699行目付近）

```rust
tracing::info!(
    agent_id = %index_agent_id,
    unindexed = unindexed,
    threshold = config.threshold,
    batch_size = config.batch_size,
    "Starting background memory index build"
);
let llm_adapter = LlmRouterAdapter::new(index_llm_router);
let _ = opencrab_core::memory_index::IndexBuilder::build_incremental(
    &index_db,
    &index_agent_id,
    &llm_adapter,
    &index_model,
    config.batch_size as usize,
)
.await;
```

**問題:** `let _ = ...` によりビルド結果（`Result<IndexBuildResult>`）が完全廃棄される。  
`Err(...)` が返っても、ログ・通知・メトリクス何も残らない。

---

## 修正設計

### 変更後のコード

```rust
tracing::info!(
    agent_id = %index_agent_id,
    unindexed = unindexed,
    threshold = config.threshold,
    batch_size = config.batch_size,
    "Starting background memory index build"
);
let llm_adapter = LlmRouterAdapter::new(index_llm_router);
match opencrab_core::memory_index::IndexBuilder::build_incremental(
    &index_db,
    &index_agent_id,
    &llm_adapter,
    &index_model,
    config.batch_size as usize,
)
.await
{
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

### 変更の要点

| 項目 | 変更前 | 変更後 |
|------|--------|--------|
| 成功時ログ | なし | `tracing::info!` で `nodes_created` / `logs_indexed` を記録 |
| 失敗時ログ | なし | `tracing::error!` で `error` 内容を記録 |
| ビルド結果 | `let _` で廃棄 | `match` で分岐し適切に処理 |
| インポート追加 | 不要 | 不要（`tracing` は既存使用） |

---

## 期待される効果

### 即時効果（この変更のみで得られる）

1. **44時間 → 数分以内の検知**  
   `tracing::error!` が出力されると既存のログ監視（`tracing_subscriber` + ログ集約）が反応する。  
   ハートビートが数分おきに発火するため、次回ハートビート時に必ずエラーログが出る。

2. **エラー原因の特定が可能になる**  
   現在は「なぜ失敗したか」がまったく分からない。  
   `error = %e` でエラー内容（DBロック競合・LLM API障害・タイムアウト等）が記録される。

3. **成功時の可視性向上**  
   `nodes_created` / `logs_indexed` が記録されるため、インデックス進捗の追跡が可能。

### 検知フロー（修正後）

```
[ハートビート発火]
  ↓
build_incremental が Err を返す
  ↓
tracing::error!("Background memory index build FAILED", error = %e)
  ↓
ログファイル / ログ集約システムにエラーレコードが記録される
  ↓
ログ監視アラートが発火（数分以内）
```

---

## 修正の差分（git diff イメージ）

```diff
-            let _ = opencrab_core::memory_index::IndexBuilder::build_incremental(
+            match opencrab_core::memory_index::IndexBuilder::build_incremental(
                 &index_db,
                 &index_agent_id,
                 &llm_adapter,
                 &index_model,
                 config.batch_size as usize,
             )
-            .await;
+            .await
+            {
+                Ok(result) => {
+                    tracing::info!(
+                        agent_id = %index_agent_id,
+                        nodes_created = result.nodes_created,
+                        logs_indexed = result.logs_indexed,
+                        "Background memory index build completed"
+                    );
+                }
+                Err(e) => {
+                    tracing::error!(
+                        agent_id = %index_agent_id,
+                        error = %e,
+                        "Background memory index build FAILED"
+                    );
+                }
+            }
```

---

## 追加修正の候補（今回スコープ外）

調査レポートで指摘された残課題。今回の実装スコープには含めないが、将来的に対処を検討する。

### 優先度HIGH: 連続失敗時のDiscord通知

`tracing::error!` だけではログを見ていない限り気づかない。  
連続失敗カウンター（`Arc<AtomicU32>`）を管理し、N回連続失敗でDiscord Webhookに通知する仕組みを追加する。

```rust
// 概念コード（実装は別タスク）
static CONSECUTIVE_FAILURES: AtomicU32 = AtomicU32::new(0);

Err(e) => {
    let count = CONSECUTIVE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::error!(...);
    if count >= MAX_CONSECUTIVE_FAILURES {
        notify_discord_webhook(&format!("index build failed {} times: {e}", count)).await;
    }
}
Ok(_) => {
    CONSECUTIVE_FAILURES.store(0, Ordering::Relaxed);
    tracing::info!(...);
}
```

### 優先度MEDIUM: メトリクスコンテキストの追加

バックグラウンドビルド時の LLM 呼び出しを `llm_logs` テーブルに記録する。  
`LlmRouterAdapter::new(...)` を `LlmRouterAdapter::new(...).with_metrics(ctx)` に変更。

### 優先度MEDIUM: 並行ビルドの防止

`Arc<AtomicBool>` による実行中フラグで重複 `tokio::spawn` を防ぐ。  
複数ハートビートが競合してDBロックを取り合うシナリオを排除する。

### 優先度LOW: ダッシュボードへの最終成功時刻表示

`GET /api/agents/{id}/memory/index` レスポンスに `last_success_at` / `consecutive_failures` を追加。

---

## 実装チェックリスト

- [ ] `crates/server/src/process.rs` の `let _ = build_incremental(...)` を `match` に置き換える
- [ ] `Ok(result)` アームで `tracing::info!` （`nodes_created`, `logs_indexed`）を追加する
- [ ] `Err(e)` アームで `tracing::error!` （`error = %e`）を追加する
- [ ] `cargo build` でコンパイルエラーがないことを確認する
- [ ] `cargo test -p opencrab-server` でリグレッションがないことを確認する
- [ ] ローカル動作確認: ビルド失敗を意図的に起こしてログが出ることを確認する

---

## 参考: IndexBuildResult の定義

```rust
// crates/core/src/memory_index/index_builder.rs:18
pub struct IndexBuildResult {
    pub nodes_created: usize,
    pub logs_indexed: usize,
}
```

`nodes_created`: 今回のビルドで新規作成されたトピックノード数  
`logs_indexed`: 今回のビルドで処理されたログエントリ数

---

## まとめ

**最小変更・最大効果の原則に従い、`let _ = ...` を `match` に置き換えるだけで、44時間未検知の問題が数分以内の検知に改善される。**

この修正は:
- コンパイルエラーリスクがほぼゼロ（`tracing` マクロの追加のみ）
- 既存の動作を変えない（ビルド失敗してもフォールバックは継続される）
- ログ監視インフラが既存のまま使える（新しいアラートルールを追加するだけでよい）

追加候補の Discord 通知・並行防止・メトリクス記録は、この最小修正の後に段階的に実装する。
