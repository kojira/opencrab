# 設計: スリープ時スキル棚卸し（エージェント自己 curation ループ）

> Status: 設計（未実装） / rev.2（第三者レビュー #100 の指摘を反映）
> 関連: `docs/design-skill-system-v2.md`, `docs/design-memory-rollup-v2.md`
> 着想元: Behrouz et al. "Language Models Need Sleep: Learning to Self-Modify and Consolidate Memories" (arXiv:2606.03979, 2026)

## 1. なぜ（背景と目的）

OpenCrab は今、スキルを **作りっぱなし** にしている:

- `create_my_skill` で獲得したスキルは `effectiveness = None` のまま放置される
- `find_unused_skills` は「7日使われていない」= **古さ** でしか刈れず、内容の良し悪しで整理しない
- つまり「獲得 → 振り返り → 整理」の周期が無く、スキルは溜まる一方

これを、**エージェントがアイドル時（スリープ）に、自分のスキル棚を自分の人格で棚卸しする**
周期として閉じる。残す・引退させる・作り直す・細分化/統合するを **エージェント本人が決める**。

### 1.1 最上位の設計思想（すべての判断軸）

**目的はエージェント毎に「強いベクトル（個性）」を育てることであり、正しさ・平均点ではない。**

- スキルX を極端に細分化して溜め込むエージェントがいてもよい。それが個性なら正解
- **平均点のエージェントなら、わざわざ人格を持たせる意味がない**
- ゆえに、全エージェントを単一の基準へ均質化する仕組みは採らない

### 1.2 rev.1 からの最重要変更 — 「機械が刈る」をやめ「本人が決める」へ

rev.1 は「使用セッションの自己スコアが **本人の平均を下回ったら archive**」という統計ルールで
keep/prune していた。第三者レビュー（PR #100）でこれは **数学的に §1.1 と矛盾** すると指摘された:
平均で切れば定義上ほぼ半数が必ず下回る → 毎回スキルの約半分を刈る → 均質化。「50個に細分化して
全部残る」が成立しない。

**根本原因は「機械（平均という軸）が取捨を決めた」こと。** rev.2 では判定を **エージェント本人の
人格** に完全に委ねる:

- keep / retire / refine / subdivide / merge は **本人が決める**
- 使用状況や成果シグナルなどの **数値は「判断材料（advisory）」** に留め、機械的な閾値では切らない
- 「刈る」ではなく「**効いているものを浮かせ、本人が棚を整える**」ループにする

これで均質化の矛盾（レビュー#3）、統計的誤判定（#6）、閾値・ベースライン未定義（#1,#8）が
根本から解消する。人格ごとに整え方が違うので、多様性は自動で保たれる。

### 1.3 着想元の論文との対応と、あえて外すもの

論文 "Language Models Need Sleep" は継続学習を Wake/Sleep の周期で捉え、Sleep 中に
①メモリ統合と ②Dreaming（自己生成データで自己改善）を行う。

- **借りる概念**: Wake/Sleep のライフサイクル、オフラインでの自己整理、蓄積量ベースのトリガ、
  「モデルが自分を self-modify する」を「**エージェントが自分のスキル棚を self-curate する**」に読替
- **あえて外す（移植不可）**: パラメータ拡張 / Generalized Knowledge Distillation / LoRA /
  RL(ReSTEM) による**重み更新**。OpenCrab は API/CLI 経由で LLM を消費する側で重みは触れない
- **本人採点を採る理由**: 論文の Dreaming 報酬は外部 reward で付けるが、OpenCrab は §1.1 の通り
  意図的に本人判断を採る。reward hacking のリスクは、客観判定ではなく **可逆 archive ＋ 運営者
  可視化・上書き**（§7）で抑える

## 2. スコープ

### やること
- スリープ（既存 `memory_maintenance` ループ）に **スキル棚卸しパス** を追加する（新規コンポーネント）
- 棚卸しは **本人の人格で判断**し、`create_my_skill`（既存）＋ `retire_my_skill`（**新設**）で反映
- 判断材料として、各スキルの **利用状況・成果シグナル**を集めて提示（数値は advisory）
- 効かないスキルは **可逆 archive**（ハード削除しない）。復活も本人/運営者が可能
- スリープで起きたことを **2層で監査ログ化**（構造化 + 生プロンプト/生応答）
- （Phase 2・任意）信頼するピアのスキルを見て **助言** を生成（非権威）

### やらないこと（非目標）
- モデルの重み学習（LoRA/蒸留/RL）——不可能
- 平均・閾値による**機械的な自動 prune**——§1.2 で撤回
- 客観的「正しさ」の判定 / エージェント間の正規化——§1.1 に反する
- 第三者 evaluator（verify 段）で棚卸しを決めること——均質化を招くため不採用

## 3. アーキテクチャ概要

```
[Wake] エージェントが会話・行動（既存）
   └─ スキル注入を記録: そのターンでコンテキストに載せたスキルを skill_usage_log へ（§8.1）

[Sleep] memory_maintenance ループ（既存, 既定600秒ポーリング）
   ├─ ① メモリ統合（既存: 索引ビルド / キーワード補完 / 月次rollup）
   └─ ② スキル棚卸しパス（新規, 蓄積量ベースの専用ゲート付き §5）
        1. パケット組立: 各スキル {guidance, 利用状況, 成果シグナル} を要約（§6.1）
        2. 人格判断:     本人のモデル＋人格で棚卸しを判断（LLM 1回, §6.2）
        3. 反映:         keep / retire(retire_my_skill) / refine(create_my_skill) / merge・subdivide
        4. 監査ログ:     何を・なぜ したかを2層で永続化（§9）
        5. (任意)助言:   信頼ピアの効いてるスキルから助言生成（非権威, §7-Phase2）
```

`memory_maintenance` は「作業が要るか見に行くだけのポーリング」で、新規活動が無い tick は
即 return（棚卸しゲート §5 が空振りを弾く）。棚卸しが走る時だけ LLM を1回消費する。

## 4. これは「新規コンポーネント」である（正直な明記）

レビュー指摘（#2）の通り、棚卸しパスは **既存流用では成立しない**:

- `evaluate_response`（`crates/actions/src/llm_evaluation.rs`）は agent の action ループ内で
  引数の採点値を DB に書くだけで、**LLM で判断する処理は含まない**。棚卸しの流用元にはならない
- `run_maintenance_tick`（`crates/server/src/memory_maintenance.rs:116`）には会話ロードも
  action ループも無い

したがって「パケット組立 → 人格プロンプト構築 → LLM 呼び出し → 応答パース → スキルアクション発火
→ 監査ログ」は **新規実装**。tick には `persona_name`/`personality`/`llm`(LlmRouterAdapter) が
既に渡っているので構築は可能だが、工数は「新規」として見積もる。

## 5. トリガ — 蓄積量ベース ＋ 時間フロア（循環を排除）

論文の統合トリガは wall-clock ではなく蓄積量ベース。密度に自動追従させるため踏襲する。
エージェント毎に `last_skill_consolidation_at` を持ち、スリープ tick で:

```
棚卸しパスを発火する条件（いずれか）:
  (A) 前回棚卸し以降の「新規活動」が N件たまった      ← 密度に自動追従
  (B) 前回棚卸しから time_cap_hours 経過 かつ 新規活動が1件以上ある   ← 暇なエージェントの保険
制約:
  最短間隔 min_interval_secs（例 1h）は空けない（無駄撃ち防止）
```

- **「新規活動」の定義（レビュー#7 の循環修正）**: 発火判定を「新規に採点が付いたセッション数」に
  すると、採点はパス内部で行うため常に0で発火しない。よって定義は **「前回棚卸し `_at` 以降に
  終了した（or 新規ログを持つ）セッション数」** とする。採点済み件数ではなく **未処理の活動量**
- `last_skill_consolidation_at` の**初期値 = エージェント作成時刻**（未設定なら「常に (B) の起点」
  として扱い、最初の十分な活動 or time_cap で初回発火）
- `N` 初期値目安 = 10 セッション。すべて config 化（§10）

## 6. 棚卸しの中身

### 6.1 判断材料（advisory・厳密な統計ではない）

各スキルについて、本人に渡すパケットを組む。**数値は判断材料であって閾値ではない**ので、
厳密な per-session スコア集約（レビュー#1）は不要。best-effort で十分:

- `guidance` / 説明 / 作成経緯（source）
- **利用状況**: `skill_usage_log`（§8.1, 注入ベース）から使用回数・直近使用時期
- **成果シグナル**（あれば）: そのスキルを注入したセッションに紐づく手掛かり
  — 例: verify段の評価（`session_logs.log_type=evaluation` の score）、`evaluate_response` が
  付けた `llm_usage_metrics.quality_score`、会話の帰結。**入手できたものだけ**を素材として提示し、
  無ければ「シグナル不足」と明記する（本人はそれも踏まえて判断）
- 重複・類似スキルの並び（統合/細分化の判断用）

> 数値はあくまで素材。keep/retire を数式で決めない（§1.2）。

### 6.2 判断（本人の人格・LLM 1回）

本人のモデル＋人格コンテキストで、上記パケットを見てスキル棚を棚卸しする。出力は各スキルへの
アクション指示（keep / retire / refine / subdivide / merge）＋**本人の理由**。

- 反映は既存/新設アクションで:
  - `create_my_skill`（既存, `skill_management.rs`）— 作成・改良・**復活**（archived を戻す）まで対応済み
  - `retire_my_skill`（**新設**）— 本人が自分のスキルを archive（引退）する手段。現状 `archive_skill` は
    DB関数＋運営者用RESTのみでエージェントが引退させられない。これを1アクション追加して補う
- 排他制御（レビュー#9）: 棚卸しは `create_my_skill` 等でスキル表を書き換えるため、**per-agent の
  棚卸し in-flight ガード**を設ける（既存 `index_build_inflight` は索引専用なので流用せず別途）

### 6.3 「効いているものを浮かせる」= effectiveness の位置づけ

`skills.effectiveness` は**表示・並べ替え用のソフトスコア**として、本人判断や成果シグナルから
更新してよい（例: 本人が「効いた」と評価した回数の反映）。**これで自動 archive はしない。**
用途はダッシュボードでの可視化と、次回パケットの並び順のみ。

## 7. 剪定の安全網 と ピア

### 7.1 安全網（統計ガードではなく可逆＋可視化）
- **archive は可逆**: `create_my_skill` が archived を復活させる（既存）。運営者もダッシュボードから復活可
- **本人による再検討**: 棚卸しパケットに **archived スキルも一定数含める**ことで、本人が後日
  「やはり戻す」と判断できる（レビュー#4: archive がループから片道になる問題の解消）
- **運営者可視化・上書き**: §9 の監査ログ＋ダッシュボードで、retire/refine を運営者が見て取り消せる

### 7.2 ピア（Phase 2・任意・既定 off）
- 既存資産: `trusted_co_agents`（信頼関係。**テーブル名は `co_agents` ではなく `trusted_co_agents`**,
  `schema.rs`）＋ ピアの `list_skills` ＋ `learn_from_peer` アクション
- 自分の弱い領域に対し、信頼ピアの効いてるスキルから **本人の人格の声で助言テキストを生成**
- **採否は本人が決める。自動採用しない。** 助言はダッシュボード表示 or 次ターンの反省材料
- config トグル、**既定 off**。論文の `learn_from_peer` の**権威を持たせない版**

## 8. データモデルの変更

### 8.1 新規テーブル: `skill_usage_log`（**注入時**に記録）
レビュー#5 の通り、`record_used_skills` の「応答本文にスキル名が出たか」判定（`process.rs:1089`
の `skill_mentioned`, 部分一致）は偽陰性/偽陽性が多く、利用の母数として脆い。**スキルを
コンテキストに注入した時点**（guidance をプロンプトに載せた時）で記録する:

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
`schema.rs` の `MIGRATIONS`（`PRAGMA user_version`, `schema.rs:333`）に次番号で追加。
既存 `record_used_skills` の応答一致は「言及された」補助シグナルとして残してもよいが、
**利用の主母数は注入ベース**にする。

### 8.2 新規アクション: `retire_my_skill`
エージェントが自分のスキルを archive する（§6.2）。`archive_skill`（`skills.rs`）を叩く薄い action。

### 8.3 per-agent 状態: `last_skill_consolidation_at`
memory index config 系テーブルに1カラム追加 or KV。初期値 = エージェント作成時刻（§5）。

### 8.4 既存の活用
- `create_my_skill` / `archive_skill` / `find_unused_skills`（古さゲートは残す）
- `skills.effectiveness`（ソフトスコア表示用, §6.3）
- verify段評価（`session_logs`）/ `llm_usage_metrics.quality_score` = 成果シグナル素材（§6.1）
- `memory_maintenance` の人格スレッド（`persona_name`/`personality` は tick に既渡し）

## 9. スリープ監査ログ（2層・生プロンプト/生応答まで残す）

自律的な棚卸しは silent にスキルを消す/変えるため、**「原則 VII: 後から読める」**（repo 規範,
`process.rs:798`）を満たす監査が必須。2層で残す:

### 層1: 構造化監査 → `agent_logs`（`context="sleep"`）
`agent_logs`（`schema.rs:1381`: `id/agent_id/level/context/message/created_at`）を流用。
1回のスリープ = 1エントリ（`message` に構造化JSON）:
```json
{
  "trigger": "activity>=N | time_cap",
  "memory":  {"logs_indexed":.., "keywords":.., "rolled_up_month":".."},
  "skill_curation": [
    {"skill":"..","action":"kept|retired|refined|created|merged","reason":"本人の理由(要約)"}
  ],
  "cost": {"llm_calls":1, "tokens":..},
  "llm_log_ids": ["..."],           // 層2への参照
  "errors": []
}
```

### 層2: 生プロンプト/生応答 → 既存 `llm_logs`
`llm_logs`（`schema.rs:1341`: `prompt TEXT` / `response TEXT` + tokens/latency/model/agent_id/
session_id/error）は **LLM 呼び出しの生プロンプト・生応答をフル保存**する既存機構。棚卸しの
LLM 呼び出しをここに保存し、層1 から `llm_log_ids` で参照する。

- **要件（正直な配線コスト）**: スリープ経路（`LlmRouterAdapter`）が現状 `llm_logs` に書いているか
  未確認。既存メンテナンス系LLM（rollup 等）が未ログなら、**スリープ経路にログ配線を通すのは
  新規作業**。「タダで流用」ではない点を実装時に見込む
- **保持ポリシー**: 生ログは肥大化するため、`llm_logs` の**保持期間/件数での prune を config 化**
  （§10）。プライバシー: 生ログは全コンテキストを含む旨をドキュメント/UIで注記

### ダッシュボード
- **スリープ履歴ビュー**（層1）を追加: いつ・何を kept/retired/refined したか＋理由、コスト
- 各エントリから **生プロンプト/生応答（既存 LLM ログ画面）へドリルダウン**
- retire/refine の **運営者取り消し（復活）導線**（§7.1 の可視化・上書きを満たす）

## 10. config
```toml
[skill_consolidation]
enabled = true            # ループ全体の on/off
trigger_new_sessions = 10 # N: 発火する新規活動(セッション)数
time_cap_hours = 24       # 保険トリガの時間キャップ
min_interval_secs = 3600  # 最短間隔フロア
include_archived_in_review = 3  # 棚卸しパケットに含める archived スキル数（再検討用, §7.1）
peer_advice = false       # Phase 2: ピア助言（既定 off）

[llm_logs]
retain_days = 90          # 生プロンプト/応答ログの保持日数（0=無期限）
```

## 11. 検証（実装時）

### 単体
- トリガゲート: (A)新規活動件数 / (B)time_cap / min_interval フロア、`last_..._at` 初期値の各分岐
- `skill_usage_log`: 注入時に記録される（応答一致ではない）
- `retire_my_skill`: archive され、`create_my_skill` で復活する（可逆）
- パケット組立: 成果シグナルが無いスキルは「シグナル不足」で提示される

### E2E
- 活動を仕込む → 棚卸しパス実行 → 本人判断に沿って kept/retired/refined が反映され、
  **監査ログ（層1）と生プロンプト/応答（層2）が両方残る**ことを assert
- 冪等/ゲート: 新規活動が無ければ no-op（`last_..._at` 更新のみ or 何もしない）
- archived スキルが次パケットに含まれ、本人が復活を選べる

### 思想の回帰テスト（最重要・§1.1 の担保）
- **同一の生スキル集合** を持つ、**人格の異なる2エージェント**に、各人格が別々の判断を返すよう
  スタブした棚卸し LLM を与える → 棚卸し後の keep/retire 結果が **エージェント毎に異なる**ことを
  assert（＝均質化していない＝機械の平均で刈っていない証明）

## 12. 段階リリース
1. **Phase 1（中核）**: `skill_usage_log`（注入時記録）+ `retire_my_skill` + スリープ棚卸しパス
   （人格判断・LLM1回）+ 監査ログ2層 + ダッシュボード（履歴/ドリルダウン/復活）。ピアなし
2. **Phase 2（任意）**: ピア助言レイヤー（非権威・既定 off）

## 13. 既知の割り切り（正直な前提）
- 本人判断は思想上の選択であり、客観的正しさは保証しない（それが狙い＝§1.1）
- 成果シグナルは best-effort（verify段/自己採点/会話帰結）で、常に揃うとは限らない。無ければ
  本人は少ない材料で判断する（シグナル不足として明示）
- 棚卸しは毎回 LLM を1回消費する。トリガ（蓄積量ベース）で頻度を抑える
- 生ログ保持は容量とのトレードオフ。`retain_days` で運営者が調整
- Phase 2 のピアは多様性を壊さないよう **助言まで**。権威化・自動採用は思想違反として禁止

## 付録: 第三者レビュー（PR #100）指摘への対応表
| # | 指摘 | 対応 |
|---|---|---|
| 1 | セッション自己スコアが実データ上未定義 | 数値を advisory 化。厳密な per-session 集約を不要に（§6.1） |
| 2 | スリープ採点は既存流用でなく新規 | §4 で新規コンポーネントと明記 |
| 3 | 平均prune が §1.1 と数学的に矛盾 | 機械prune撤回、本人判断へ（§1.2, §6） |
| 4 | archive がループから片道 | archived を棚卸しパケットに含め本人再検討＋運営者復活（§7.1） |
| 5 | 利用検出が応答一致で脆弱 | 注入時記録に変更（§8.1） |
| 6 | 統計的誤archive | 機械判定を廃止（§1.2） |
| 7 | トリガ循環 | 「未処理の新規活動」ベースに再定義（§5） |
| 8 | ベースライン窓/閾値未定義 | 閾値自体を廃止（§1.2） |
| 9 | 名称等（trusted_co_agents 他） | §7.2/§5/§6.2 で修正・補完 |
