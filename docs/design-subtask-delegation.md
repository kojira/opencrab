# 設計ドキュメント: サブタスク委譲アーキテクチャ

> TODO #19 — 一次応答 + バックグラウンド処理分離
> ステータス: **設計確定**

---

## 1. 現状分析（問題定義）

### 1.1 現在の処理フロー

```
message_loop.rs::run_discord_loop()
  ├─ Discord メッセージ受信
  ├─ gateway.start_typing(channel_id)
  ├─ run_agent_response(...).await   ← 全処理ブロック（数十秒〜数分）
  └─ gateway.send_to_channel(...)    ← 完了後にやっと送信
```

### 1.2 問題点

- エンジン完了まで Discord に一切応答なし（処理ブロック状態）
- タイピングインジケーターは最大10秒で自動消える（Discord仕様）
- 画像生成・複数ステップのシェル処理は特に長時間化する
- ユーザーには処理中なのかフリーズしたのか区別がつかない

### 1.3 エンジンのループ構造（engine.rs）

```rust
loop {
    iterations += 1;
    if iterations > self.max_iterations { return stopped_by_limit; }

    let response = self.llm.chat(request).await?;

    if !response.tool_calls.is_empty() {
        // ツール実行 → メッセージ追加 → continue
        continue;
    }
    // ツールコールなし → 最終応答として return
    return Ok(EngineResult { response: final_text, ... });
}
```

- 1ループ = 1 LLM呼び出し + N ツール実行
- max_iterations=20（process.rsでハードコード）

---

## 2. 確定設計（案A+D統合: LLM第1ターン活用方式）

### 2.1 全体フロー

```
メッセージ受信
  ↓
メインエンジン LLM第1ターン呼び出し（depth=0）
  ├─ テキストのみ → 即Discord送信、終了（短い返答）
  └─ テキスト+tool_calls → テキスト即Discord送信、tool_callsはバックグラウンドへ

[バックグラウンド]
サブエンジン（depth=1）でタスク処理
  ↓
subtask_result → セッション履歴に追加
  ↓
メインエンジン再呼び出し（depth=0）
  ↓
最終応答をDiscordに送信
```

### 2.2 一次応答の設計

- **ハードコード固定文言は禁止**
- LLM第1ターンのテキストをそのまま使う（自然なack）
- `engine.rs`の`iterations==1`時にコールバック経由で即送信

### 2.3 spawn_subtask（gateway action）

新規ファイル `crates/actions/src/spawn_subtask.rs` に実装。

引数:
- `task`: タスク説明（必須）
- `timeout_secs: Option<u32>`: タイムアウト秒数（省略時はデフォルト1800秒）
- `max_iterations: Option<u32>`: LLMループの最大イテレーション数（省略時はデフォルト）

LLMがtool_callとして`spawn_subtask`を発行すると:

1. 新しいセッションIDを生成
2. metadata_jsonに`parent_session_id` + `depth`を記録
3. `tokio::spawn`でサブエンジン（depth+1）を起動
4. `DashMap<subtask_id, (JoinHandle, session_id)>`で管理
5. `tokio::select!`でタイムアウトと処理を競争

### 2.4 depth制限（MAX_DEPTH=2）

| depth | 役割 | spawn_subtask | Discord系アクション |
|-------|------|--------------|-------------------|
| 0 | メインエンジン | 使用可 | 使用可 |
| 1 | サブエンジン | 使用可（MAX_DEPTH未満） | **ブロック** |
| 2 | サブのサブ | **使用不可**（MAX_DEPTH到達） | **ブロック** |

- `depth >= MAX_DEPTH(=2)` で`spawn_subtask`をアクション一覧から除外
- `depth >= 1` で`discord_send`等のDiscord系アクションを全ブロック

### 2.5 セッション設計

- サブは**別セッション**（新しいセッションIDを生成）
- `metadata_json`に記録する情報:
  - `parent_session_id`: 親のセッションID
  - `depth`: 現在の深さ
- **cross_session_ref（クロスセッション参照）**: メインとサブが別セッションになるため、「どのメインメッセージに対してどのサブタスクが起動されたか」という因果関係が失われてしまう問題を解決する仕組み。あるセッションのログが「別セッションのどのメッセージ」に対応するかを記録する
  - `metadata_json`に以下の形式で保存:
    ```json
    {
      "cross_session_ref": {
        "session_id": "親セッションのID",
        "message_id": "参照元メッセージのID"
      }
    }
    ```
  - 例: サブタスクの`subtask_result`ログに、それを起動したメインセッションのmessage_idを記録することで、「このサブタスクはあのメッセージに応答して起動された」と辿れる
  - **保存先**: `session_logs`テーブルの`metadata_json`カラム
  - **Phase**: Phase 1で実装（サブセッション生成時に必ず記録する）
  - **ダッシュボード**: セッション一覧で「このセッションの親」「このセッションから起動されたサブ一覧」を表示する際に参照
- **steer（サブタスクへの追加指示）**: 実行中のサブエンジンに対して、メインから途中で指示を送り込める仕組み
  - サブは別セッションを持つため、そのセッションIDに対してメッセージを追加できる
  - サブのLLMは次のループ呼び出し時にセッション履歴からそのメッセージを受け取り、指示に従って処理を変更する
  - 例: 「方針を変えて英語で出力して」「処理を中断して結果だけ返して」等の軌道修正が可能
  - 実装: `DashMap<subtask_id, (JoinHandle, session_id)>` でサブを管理し、`steer_subtask(subtask_id, message)` gateway actionでセッションにメッセージを追加する
  - **Phase 1では受動的な実装のみ（セッションにメッセージを書くだけ）。Phase 2でサブがリアルタイム受信できる仕組みを追加**
- **サブ起動時のメインセッション履歴への自動書き込み**: サブタスクが起動されると、メインセッション履歴に以下のシステムメッセージが自動的に書き込まれる:
  ```json
  {
    "type": "subtask_spawned",
    "subtask_id": "xxx",
    "session_id": "yyy",
    "spawned_at": "2026-03-22T11:00:00Z"
  }
  ```
  - これによりメインLLMがsubtask_idを把握できる
  - steer/cancelはこのsubtask_idを使って呼ぶ

### 2.6 サブのコンテキスト

サブエンジンに渡す情報:
- **システムプロンプト**（人格・スキル定義）
- **タスク指示**（spawn_subtaskの引数）
- **直近コンテキスト**（必要最小限）

渡さないもの:
- 全セッション履歴（コンテキスト爆発防止）

RAGアクセス（記憶検索スキル）は**Phase 1から使用可能**。

### 2.7 タイムアウト

- デフォルト: **1800秒（30分）**、configで変更可能（spawn_subtaskの`timeout_secs`引数でもオーバーライド可能）
- タイムアウト時の処理:
  1. メインエンジンに通知
  2. Discordにエラー送信
  3. `JoinHandle::abort()` + プロセスグループkill

実装: `tokio::select!`で`tokio::time::sleep(timeout)`と処理を競争させる。

### 2.8 変更するファイル

| ファイル | 変更内容 |
|---------|---------|
| `crates/discord/src/message_loop.rs` | `run_discord_loop`の分岐追加（第1ターン判定・バックグラウンドspawn） |
| `crates/core/src/engine.rs` | `iterations==1`時のコールバック追加 |
| `crates/server/src/process.rs` | `run_agent_response`に`depth`引数追加 |
| `crates/server/src/bridge.rs` | `BridgedExecutor`でdepthによるアクションフィルタリング |
| `crates/actions/src/spawn_subtask.rs` | 新規作成（gateway action） |

---

## 3. Phase別実装ステップ

### Phase 1（最優先）

目標: 一次応答 + バックグラウンド処理の基本動作

#### Step 1: engine.rsにコールバック追加

- `SkillEngine`に`on_first_response: Option<Box<dyn FnOnce(String) + Send>>`を追加
- `iterations==1`かつテキストありの場合にコールバックを呼ぶ
- コールバックはDiscord送信関数のクロージャ

#### Step 2: process.rsにdepth引数追加

- `run_agent_response`のシグネチャに`depth: u32`を追加
- depthを`BridgedExecutor`に渡す

#### Step 3: bridge.rsでアクションフィルタリング

- `BridgedExecutor`に`depth`フィールドを追加
- `depth >= 1`でDiscord系アクション（`discord_send`等）をブロック
- `depth >= MAX_DEPTH`で`spawn_subtask`を除外

#### Step 4: spawn_subtask.rs新規作成

- gateway actionとして実装
- 引数:
  - `task`: タスク説明（必須）
  - `timeout_secs: Option<u32>`: タイムアウト秒数（省略時はデフォルト1800秒）
  - `max_iterations: Option<u32>`: LLMループの最大イテレーション数（省略時はデフォルト）
- 処理:
  1. 新セッションID生成
  2. metadata_json設定（parent_session_id, depth）
  3. `tokio::spawn`でサブエンジン起動（depth+1）
  4. `DashMap`に`(JoinHandle, session_id)`を登録
  5. `tokio::select!`でタイムアウト管理

#### Step 5: message_loop.rsの分岐実装

- 第1ターンの結果で2択で分岐:
  - テキストのみ → 即送信、終了
  - テキスト+tool_calls → テキスト即送信、tool_callsはバックグラウンド
- バックグラウンド完了後:
  1. subtask_resultをセッション履歴に追加
  2. メインエンジン再呼び出し（depth=0）
  3. 最終応答をDiscord送信

#### Step 6: セッション・ログ設計

- 別セッション生成の実装
- metadata_jsonにparent_session_id + depth記録
- cross_session_refによるトレース確認

#### Step 7: RAGアクセス有効化

- サブエンジンのコンテキストにRAG（記憶検索スキル）を含める
- メインと同じRAG設定を引き継ぐ

#### Step 8: cancel_subtask gateway action

- `cancel_subtask(subtask_id)` を実装
- DashMapからJoinHandleを取得して`abort()`
- メインセッション履歴に`{type: "subtask_cancelled", subtask_id}`を記録

#### Step 9: report_progress gateway action

- `report_progress(message)` を実装
- depth >= 1のサブのみ呼び出し可能（depth=0は呼べない）
- メインセッション履歴に`{type: "subtask_progress", subtask_id, message}`を書き込む
- メインエンジンは次の呼び出し時にこれを受け取りDiscordへ送信するかどうかを判断する

#### Step 10: spawn_coding_agent gateway action

- `spawn_coding_agent(agent_type: "claude|codex", task, timeout_secs?)` を実装
- spawn_subtaskの特化版（通常タスクとは別の専用アクション）
- 起動時に以下を自動実行:
  1. `progress_report.sh` をサブのワークスペースに自動生成・配置
     ```bash
     #!/bin/bash
     MESSAGE="$1"
     curl -s -X POST http://localhost:8080/api/agents/AGENT_ID/subtasks/SUBTASK_ID/progress \
       -H "Content-Type: application/json" \
       -d "{\"message\": \"$MESSAGE\"}"
     ```
  2. タイムアウトを長めに設定（デフォルト1800秒、引数でオーバーライド可）
  3. サブのシステムプロンプトに「ステップ完了時は `./progress_report.sh 'メッセージ'` を呼ぶこと」を追加
- progress APIエンドポイント `POST /api/agents/{id}/subtasks/{subtask_id}/progress` を新規追加

### Phase 2

目標: サブ↔メイン双方向通信と安全性向上

#### Step 1: ask_parent実装

- サブがメインに質問するためのtool_call方式
- サブ側: `ask_parent`アクションを追加
- メイン側: 質問を受けてLLM呼び出し → 回答をサブに返す

#### Step 2: 出力トランケーション

- サブの出力が長い場合に要約/切り詰めてメインに渡す
- メインのコンテキスト爆発を防止

#### Step 3: チャンネルごとの競合管理

- 同一チャンネルで複数サブタスクが走った場合の排他制御
- 応答順序の保証

### Phase 3

目標: マルチエージェント連携

#### Step 1: co-agent呼び出し

- 同一マシン上の別エージェントをサブとして呼び出す仕組み

#### Step 2: 相互評価システム

- エージェント間の得意分野発見

#### Step 3: spawn_subtaskのスマートなプロンプト設計

- タスク種別に応じた最適なプロンプトテンプレート

---

## 4. 補足事項

### 4.1 depth設計の根拠

MAX_DEPTH=2とした理由:
- depth=0（メイン）→ depth=1（サブ）で大半のユースケースをカバー
- depth=2（サブのサブ）は複雑なタスク分割時のみ
- 無制限にするとリソース消費・デバッグ困難が指数的に増加
- 2段で十分な実用性と安全性のバランス

### 4.2 セッション分離の根拠

別セッションにする理由:
- メインの会話履歴がサブのツール実行結果で汚れない
- サブが失敗しても、メインのセッション状態に影響しない
- metadata_jsonのparent_session_id + cross_session_refで追跡可能性は担保

### 4.3 ログ・トレース設計

```
[depth=0] session=abc123 → spawn_subtask(task="画像生成")
[depth=1] session=def456 (parent=abc123) → 処理開始
[depth=1] session=def456 → ツール実行: generate_image
[depth=1] session=def456 → 処理完了 (elapsed: 45s)
[depth=0] session=abc123 → subtask_result受信 → メインエンジン再呼び出し
[depth=0] session=abc123 → 最終応答Discord送信
```

### 4.4 エラーハンドリング

| 状況 | 対応 |
|------|------|
| サブがタイムアウト（デフォルト30分） | メインに通知 → Discordにエラー送信 → abort + kill |
| サブがパニック | JoinHandleのエラーをキャッチ → メインに通知 |
| サブがmax_iterations到達 | stopped_by_limitとして結果を返す → メインが判断 |
| depth超過でspawn_subtask呼び出し | アクション一覧に含まれないため、LLMが呼べない |

---

## 5. システムプロンプト要件

### 5.1 テキスト先行応答の強制

フロー分岐を2択に保つため、実装時のシステムプロンプトに以下を必ず含めること：

```
必ずテキスト応答を先に返してからツールを使うこと。
いきなりツールコールのみの応答（テキストなし）は禁止。
```

**理由:**
- これにより「tool_callsのみ」ケースが発生しなくなる
- message_loop.rsのコード分岐が2択に簡潔になる
- LLMが必ずユーザーへの意思表示（ack）をしてからツール実行に入る

### 5.2 一次応答の指針

第1ターンのテキストはユーザーへの自然なackとなるよう、以下も推奨：

```
ツールを使う前に、何をするかを1〜2文で簡潔に述べること。
長い説明は不要。「〜を調べます」「〜を実行します」程度で十分。
```

### 5.3 spawn_subtaskの非同期動作の周知

spawn_subtaskの動作仕様をエージェントが正しく理解できるよう、以下をシステムプロンプトに含めること：

```
spawn_subtaskを呼び出した後の動作について：
- tool_resultは即座に返るが、これはサブタスクの起動確認（status: spawned）にすぎない
- 実際の処理結果は非同期にセッション履歴に追加される（後から返ってくる）
- spawn_subtask呼び出し後は、ackテキスト（「〜を開始しました」等）を返して一旦終了すること
- サブタスク完了後、メインエンジンが自動的に再呼び出しされ、結果を受け取る
- spawn_subtask後に結果を待ち続けるループに入ってはいけない
```

**この仕様を明記する理由:**
- エージェントがtool_resultを「完了通知」と誤解して結果を待ち続けるループに入るのを防ぐ
- spawn後に即座にackを返す動作が設計通りであることをエージェントに伝える
- サブタスク結果は別のターンで届くため、現在のターンでは待機不要

### 5.4 全てのツールコールは非同期（サブ経由）

ツールコールは全て（`execute_shell`も含む）バックグラウンドのサブエンジンに委譲されて実行される。メインエンジンから見ると全て非同期であることをシステムプロンプトに明記すること：

```
ツールコールについて：
- テキストを返しながらツールを使うと、ツール処理はバックグラウンドのサブエンジンに委譲される
- execute_shellを含む全てのツールはサブエンジン内で実行される（メインからは非同期）
- 処理が完了すると、メインエンジンが自動的に再呼び出しされて結果を受け取る
```

`spawn_subtask`はその中でも特殊で、**サブエンジンがさらに別のサブエンジンを起動する**アクション（サブのサブ）。

> **注意**: サブエンジン内で`execute_shell`等の通常ツールを呼んでも、さらに別のサブは起動しない。サブ内のツールはそのサブエンジン内で直接（同期的に）実行される。サブを起動するのは`spawn_subtask`のみ。

**この仕様を明記する理由:**
- エージェントが「自分がツールを同期的に実行している」と誤解するのを防ぐ
- 再呼び出し時に「サブタスクの結果を受け取った続き」として正しくコンテキストを解釈させるため

### 5.5 subtask_idを使ったキャンセル・制御

サブ起動時に `{subtask_id, session_id}` がメインセッション履歴に記録される。ユーザーがキャンセルや進捗確認を求めてきた場合、セッション履歴からsubtask_idを読み取って`cancel_subtask` / `steer_subtask`を呼ぶこと：

```
サブタスクの制御について：
- spawn_subtask実行後、メインセッション履歴にsubtask_idが記録される
- ユーザーから「キャンセルして」「止めて」と言われた場合: cancel_subtask(subtask_id)を呼ぶ
- ユーザーから「方針を変えて」等の軌道修正を求められた場合: steer_subtask(subtask_id, message)を呼ぶ
- subtask_idはセッション履歴のtype="subtask_spawned"エントリから取得する
```

---

## Appendix: 採用しなかった案の比較

### 案B: システムプロンプト指示方式

**概要:** LLMにシステムプロンプトで「先に短い応答を返してからツールを使え」と指示する。

**不採用理由:**
- LLMが指示に従う保証がない（プロンプトの信頼性に依存）
- 従ったとしても、テキスト→ツール実行→最終応答の全体が同期ブロック
- 単体では「処理ブロック問題」を解決しない
- 案A+Dとの組み合わせとして採用済み（5.1参照）

### 案C: ツールコール結果の段階的Discord送信

**概要:** engine.rsのループ内で、各ツール実行結果をリアルタイムにDiscordに送信する。

**不採用理由:**
- engine.rsはDiscordの存在を知らないべき（レイヤー違反）
- コールバック方式で解決しようとしても、engine→Discord方向の依存が生まれる
- ツール実行の中間結果はユーザーにとって有用とは限らない（ノイズになる）
- 実装難易度が高い割に、ユーザー体験の改善が限定的

### 案比較表

| 観点 | 案A+D（採用） | 案B | 案C |
|------|-------------|-----|-----|
| 一次応答の確実性 | 高（コールバック制御） | 低（LLM依存） | 中（中間結果） |
| 実装難易度 | 中 | 低 | 高 |
| アーキテクチャ整合性 | 良好 | 良好 | 悪い（レイヤー違反） |
| ユーザー体験 | 自然な応答 | 不確実 | ノイズが多い |
