# 設計: スリープ時スキル統合（自己強化ループ）

> Status: 設計（未実装）
> 関連: `docs/design-skill-system-v2.md`, `docs/design-memory-rollup-v2.md`
> 着想元: Behrouz et al. "Language Models Need Sleep: Learning to Self-Modify and Consolidate Memories" (arXiv:2606.03979, 2026)

## 1. なぜ（背景と目的）

OpenCrab は今、スキルを **作りっぱなし** にしている:

- `create_my_skill` / `learn_from_experience` で獲得したスキルは `effectiveness = None` のまま放置される
- `find_unused_skills` は「7日使われていない」= **古さ** でしか刈れず、「効いているか」で取捨選択しない
- つまり「獲得 → 測定 → 採否」の閉ループが無く、スキルは溜まる一方で自浄しない

これを、エージェントが **アイドル時（スリープ）に自分のスキルを振り返り、自分にとって効いたものを強化し、効かないものを（可逆的に）棚上げする** 自己強化ループとして閉じる。

### 1.1 最上位の設計思想（これがすべての判断軸）

**目的はエージェント毎に「強いベクトル（個性）」を育てることであり、正しさ・平均点ではない。**

- スキルX を極端に細分化して溜め込むエージェントがいてもよい。それがそのエージェントの個性なら正解
- **平均点のエージェントなら、わざわざ人格を持たせる意味がない**
- ゆえに、全エージェントを単一の「正しさ」へ均質化する仕組み（第三者の客観採点で keep/prune を決める等）は **採らない**
- 判定軸は常に **そのエージェント自身の人格** であり、基準は **そのエージェント自身の平均** である（他エージェントとの相対比較で正規化しない）

この思想は、以下すべての設計判断（採点者・効果の定義・剪定の基準・ピアの扱い）の理由になっている。

### 1.2 着想元の論文との対応と、あえて外すもの

論文 "Language Models Need Sleep" は継続学習を Wake/Sleep の周期で捉え、Sleep 中に
①メモリ統合（上書き前に抽象化して安定層へ蒸留）と ②Dreaming（自己生成データで自己改善し、
**効いたものだけ報酬で採用・残りは synaptic pruning で剪定**）を行う。

- **借りる概念**: Wake/Sleep のライフサイクル、オフライン統合、報酬による keep/prune、剪定（可逆棚上げ）、蓄積量ベースのトリガ
- **あえて外す（移植不可）**: パラメータ拡張 / Generalized Knowledge Distillation / LoRAエキスパート / RL(ReSTEM) による**重み更新**。OpenCrab は API/CLI 経由で LLM を消費する側であり、モデルの重みは触れない。本設計は「重みの継続学習」を「**メモリ＆スキルの継続運用**」に読み替えた部分だけを使う
- **論文と一致する重要点**: 論文の Dreaming 報酬は frozen(外部) reward model や客観距離で付ける。ただし OpenCrab は思想 1.1 の通り **意図的に本人採点** を採る。「自己改善を自分で採点すると reward hacking に陥る」というリスクは、客観判定ではなく **可逆・低感度・自分基準** の安全弁（§6）で抑える

## 2. スコープ

### やること
- スリープ（既存 `memory_maintenance` ループ）に **スキル統合パス** を1つ追加する
- スリープ時に **本人の人格で直近セッションを自己採点** する（採点タイミングを会話中に依存させない）
- スキル利用とセッション採点を紐付け、**本人基準の effectiveness** を実測して埋める
- 効いたスキルは維持・強化、効かないスキルは **可逆 archive**（ハード削除しない）
- （Phase 2・任意）信頼するピアのスキルを見て **助言** を生成する（非権威）

### やらないこと（非目標）
- モデルの重み学習（LoRA/蒸留/RL）——不可能
- 客観的「正しさ」の判定 / エージェント間の正規化——思想 1.1 に反する
- 第三者 evaluator（verify 段）を本ループの採点に使うこと——§4 で不採用を明記

## 3. アーキテクチャ概要

```
[Wake] エージェントが会話・行動（既存）
   └─ record_used_skills: どのスキルを使ったかを記録（既存 + 拡張）

[Sleep] memory_maintenance ループ（既存, 既定600秒ポーリング）
   ├─ ① メモリ統合（既存: 索引ビルド / キーワード補完 / 月次rollup）
   └─ ② スキル統合パス（新規, 独自の低頻度ゲート付き）
        1. 自己採点:   直近の未採点セッションを本人の人格で quality_score 付け
        2. 効果測定:   スキル毎に「使用セッションの自己スコア vs 本人ベースライン」
        3. 採否:       効いた→維持/強化 , 効かない(≥M件)→可逆archive , 未満→保留
        4. (任意)助言: 信頼ピアの効いてるスキルから助言テキスト生成（非権威）
```

`memory_maintenance` は「作業が要るか見に行くだけのポーリング」で、新規ログが無い tick は
LLM ゼロコール（§3.1 参照）。スキル統合パスも同様に **ゲートで空振りは即 return** させる。

### 3.1 なぜ既存ループに相乗りさせ、独自ゲートを付けるのか

`run_maintenance_tick`（`crates/server/src/memory_maintenance.rs`）は既に:
- 索引ビルドを `unindexed >= threshold || idle(IDLE_GATE_MINUTES 経過)` でゲート
- rollup を staleness でゲート（冪等）

600秒は「作業有無を見るポーリング cadence」であり、実コストはこれらのゲートが決める。
スキル統合を毎 tick 走らせるのは無駄なので、**専用の蓄積量ベースゲート**（§5）を持たせる。

## 4. 採点者 — 本人の人格（self-eval）

OpenCrab には評価経路が2つある:

| 経路 | 採点者 | 保存先 | 性質 |
|---|---|---|---|
| `evaluate_response`（`EvaluateResponseAction`） | **本人の人格**（自分の run loop / モデル） | `llm_usage_metrics.quality_score`（数値カラム） | 個性色・自己採点 |
| `evaluator` / verify段（`run_verify_stage`） | 第三者（独立context・別モデル・rubric） | `session_logs`(`log_type=evaluation`) | 客観・契約タスク限定 |

**本ループは経路1（本人採点）を採る。** 理由は思想 1.1。第三者採点で keep/prune すると全員が
単一の正しさへ寄り、個性が死ぬ。effectiveness は数値カラムで JOIN も容易。

> 経路2（verify 段）は契約タスクの検証という別目的でそのまま残す。本ループには使わない。

### 4.1 採点タイミング = スリープ時に自己採点する

会話中に `evaluate_response` を呼ぶかはエージェント任せで、**サボると採点が溜まらない**。
そこで **スリープ時に、本人の人格で直近の未採点セッションをまとめて自己採点** する:

- スリープパスの先頭で「未採点の直近セッション（自己スコア未付与）」を集め、本人のモデル＋人格
  コンテキストで `quality_score`（と任意で `task_success`）を付ける
- 保存は経路1と同じ `llm_usage_metrics.quality_score`（`session_id` 紐付き）を再利用
- これで採点はエージェントの自己評価習慣に依存しなくなり、かつ **人格で採点する** 思想も保たれる

## 5. トリガ — 蓄積量ベース ＋ 時間フロア

論文の統合トリガは wall-clock ではなく「ステップ数が C の倍数」＝**蓄積量ベース**。密度に自動追従
させるため、これに倣う。エージェント毎に `last_skill_consolidation_at` を持ち、スリープ tick で:

```
スキル統合パスを発火する条件（いずれか）:
  (A) 前回統合以降に「新しく自己採点が付いたセッション」が N件たまった   ← 密度に自動追従
  (B) 前回統合から24h経過 かつ 新規採点が1件以上ある                   ← 暇なエージェントの保険
制約:
  最短間隔 MIN_INTERVAL（例 1h）は空けない（統計ノイズ/無駄撃ち防止）
```

- 忙しいエージェント → (A) で頻繁に統合
- 暇なエージェント → (B) で最低1日1回（データがあれば）
- 「1日固定」だと密度差で破綻する問題を回避

`N` の初期値目安 = 20（採点セッション）。config 化。

## 6. 効果測定と採否

### 6.1 「効いた」の定義（本人基準・ペルソナ色）

> スキルX の効果 = **X を使った（自己採点付き）セッションの `quality_score` 平均が、
> そのエージェント自身のベースライン平均を上回るか。**

- 基準は **そのエージェント自身の平均**。他エージェントと比較しない（§1.1）
- よって「そのエージェントのペルソナにとって良かった」スキルが残る＝個性を増幅する

### 6.2 最小サンプル M でだけ判定（低感度）

```
スキルX の effectiveness を(再)算出するのは、
  X を使った自己採点付きセッションが ≥ M件（例 M=5）ある時だけ。
  M件未満は「データ不足」= effectiveness を据え置き（None のまま保留）。
```

- 1〜2回の自己採点で舞い上がらない
- 判定の重さも密度に比例（よく使うスキルほど早く評価、稀なスキルはずっと保留）

### 6.3 採否（可逆のみ）

| 条件 | 動作 |
|---|---|
| ≥M件 かつ ベースライン超 | 維持・`effectiveness` を実測値で更新（表示/並べ替え用のソフトスコア） |
| ≥M件 かつ ベースライン下位 | **可逆 `archive_skill`**（論理削除・復活可能）。ハード削除しない |
| <M件 | 保留（effectiveness=None のまま） |

**細分化・溜め込みを罰しない:** 基準が本人平均なので、あるエージェントがスキルX を50個に細分化し、
そのどれも自分で高く採点していれば50個すべて残る。それがそのエージェントの強いベクトル（§1.1）。

## 7. ピアの扱い — 助言まで（Phase 2・任意・既定 off）

「他のエージェントのスキルを見てアドバイスくらいはしてもよい」を、**非権威**の助言レイヤーとして
Phase 2 で追加する:

- 消費する既存資産: `co_agents`（信頼関係・権限 owner/agent/co-agent）＋ ピアの `list_skills`
  ＋ `learn_from_peer` アクション
- スリープパスで、自分の低スコア領域/gap に対し、**信頼するピアの効いてるスキル** を探し、
  **本人の人格の声で助言テキストを生成**（例:「Xはこの状況をスキルYで捌いている。君のペルソナなら
  …と翻案できるかも」）
- **採否は本人の人格が決める。自動採用しない。** 助言はダッシュボード表示 or 次ターンの反省材料に置くだけ
- config トグルで on/off。「してもよい “かも”」の位置づけなので **既定 off**
- これは論文の `learn_from_peer` / 「random expert で無関係知識を混ぜ novel synthesis」の
  **権威を持たせない版**。他者の視点は入れるが判定軸は本人のまま（§1.1 を壊さない）

## 8. データモデルの変更

### 新規テーブル: `skill_usage_log`
スキル利用をセッション単位で残す（現状 `increment_skill_usage` はグローバル `usage_count` を
増やすだけで、どのセッションで使ったかが残らない）。

```sql
CREATE TABLE skill_usage_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id   TEXT NOT NULL,
    skill_id   TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_skill_usage_log_skill   ON skill_usage_log(skill_id);
CREATE INDEX idx_skill_usage_log_session ON skill_usage_log(session_id);
```
`schema.rs` の `PRAGMA user_version` マイグレーションで追加（次の空き番号）。

### 新規の per-agent 状態: `last_skill_consolidation_at`
既存の memory index config 系テーブルに1カラム追加、または小さな KV（`agent_kv` 相当）で保持。

### 既存の活用
- `skills.effectiveness`: 現状ほぼ None → 本ループで実測値が入る
- `record_used_skills`（`crates/server/src/process.rs`）: `increment_skill_usage` に加えて
  `skill_usage_log` へ1行 insert する
- `archive_skill` / `find_unused_skills`: 剪定に流用（古さゲートは残しつつ、効果ゲートを追加）
- `llm_usage_metrics.quality_score` / `evaluate_response`: 自己採点シグナル
- `memory_maintenance` の人格スレッド（`persona_name`/`personality` は既に tick に渡っている）

## 9. ダッシュボード
- スキル画面に `effectiveness` を表示（現状 None で空欄の所）。「データ不足」状態も明示
- 本ループで archive されたスキルを「復活」導線付きで表示（可逆であることを可視化）
- （Phase 2）ピア助言ノートの表示

## 10. config
```toml
[skill_consolidation]
enabled = true          # ループ全体の on/off
trigger_new_sessions = 20   # N: 発火する新規採点セッション数
min_samples = 5             # M: 判定に必要な採点付き使用回数
min_interval_secs = 3600    # 最短間隔フロア
time_cap_hours = 24         # 保険トリガの時間キャップ
peer_advice = false         # Phase 2: ピア助言（既定 off）
```

## 11. 検証（実装時）

### 単体
- 効果算出: 使用セッションのスコア平均 vs ベースラインの比較ロジック
- トリガゲート: (A)量 / (B)24h / MIN_INTERVAL フロアの各分岐
- 最小サンプルゲート: M未満は None 据え置き

### E2E
- 自己スコアを散らしたセッションを仕込む → 統合パス実行 →
  高スコアで使われたスキルは維持、低スコア(≥M)は archive、データ不足(<M)は None、を assert
- 冪等性: 2回目の統合は新規データ無ければ no-op

### 思想の回帰テスト（最重要）
- **同一の生スキル集合** を持つ、**人格の異なる2エージェント** に、各人格が別々に採点した
  セッションを与える → 統合後の keep/prune 結果が **エージェント毎に異なる** ことを assert。
  （＝均質化していない＝§1.1 が守られている証明）

## 12. 段階リリース
1. **Phase 1（本設計の中核）**: `skill_usage_log` + スリープ時自己採点 + 効果測定 + 可逆archive
   + ダッシュボード表示。ピアなし
2. **Phase 2（任意）**: ピア助言レイヤー（非権威・既定 off）

## 13. 既知の割り切り（正直な前提）
- 本人採点は思想上の選択であり、客観的正しさは保証しない（それが狙い）
- 相関ベース（使用と自己スコアの相関）であって因果ではない。API 消費側の限界
- 効果測定の鮮度は自己採点データの蓄積速度に依存する。データ不足のスキルは安全側に保留される
- Phase 2 のピア助言は多様性を壊さないよう **助言まで**。権威化・自動採用は思想違反として禁止
