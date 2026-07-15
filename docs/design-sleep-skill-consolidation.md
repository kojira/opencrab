# 設計: スリープ時スキル棚卸し（エージェント自己 curation ループ）

> Status: 設計（未実装） / rev.3（第三者レビュー #100 の1回目・2回目指摘を反映）
> 関連: `docs/design-skill-system-v2.md`, `docs/design-memory-rollup-v2.md`
> 着想元: Behrouz et al. "Language Models Need Sleep: Learning to Self-Modify and Consolidate Memories" (arXiv:2606.03979, 2026)

## 1. なぜ（背景と目的）

OpenCrab は今、スキルを **作りっぱなし** にしている:

- `create_my_skill` で獲得したスキルは `effectiveness = None` のまま放置される
- `find_unused_skills` は「7日使われていない」= **古さ** でしか刈れず（しかも実際は archive せず
  ログ出力のみ, `skill.rs:221`）、内容の良し悪しで整理しない
- つまり「獲得 → 振り返り → 整理」の周期が無く、スキルは溜まる一方

これを、**エージェントがアイドル時（スリープ）に、自分のスキル棚を自分の人格で棚卸しする**
周期として閉じる。残す・引退させる・作り直す・細分化/統合するを **エージェント本人が決める**。

### 1.1 最上位の設計思想（すべての判断軸）

**目的はエージェント毎に「強いベクトル（個性）」を育てることであり、正しさ・平均点ではない。**

- スキルX を極端に細分化して溜め込むエージェントがいてもよい。それが個性なら正解
- **平均点のエージェントなら、わざわざ人格を持たせる意味がない**
- 全エージェントを単一の基準へ均質化する仕組みは採らない

### 1.2 「機械が刈る」をやめ「本人が決める」へ（rev.1→2 の最重要変更）

rev.1 は「使用セッションの自己スコアが本人平均を下回ったら archive」という統計ルールで刈っていた。
これは §1.1 と数学的に矛盾する（平均で切れば定義上ほぼ半数を必ず刈る＝均質化）。**根本原因は
「機械（平均という軸）が取捨を決めた」こと。** そこで判定を **エージェント本人の人格** に委ねる:

- keep / retire / refine / subdivide / merge は **本人が決める**
- 数値は「判断材料（advisory）」に留め、機械的な閾値では切らない

### 1.3 均質化の再発を能動的に防ぐ（rev.2→3 で強化）

機械 prune を撤回しても、**判断材料に「平均・順位」を渡せば人格 LLM が自発的に平均比較して
均質化に戻りうる**（レビュー2回目 #6 の指摘）。これは構造的には人格プロンプトだけでは防げない。
そこで rev.3 では:

- 成果シグナルは **順位や平均でなく定性ラベル**で提示する（例: `helped` / `mixed` /
  `unclear` / `signal-insufficient`）。「平均より下」を誘発する数値提示をしない（§6.1）
- 個性が保たれることを**構造的には保証しない**と正直に認める。担保は「機械判定の撤回＋定性提示＋
  人格プロンプト」の組み合わせによる**緩和**であり、回帰テスト（§11）はそのための必要条件を
  確認するに留まる（実 LLM 運用の個性を証明はしない）

### 1.4 論文との対応と、あえて外すもの

- **借りる**: Wake/Sleep 周期、オフライン自己整理、蓄積量ベースのトリガ、「モデルの self-modify」を
  「**エージェントのスキル棚 self-curate**」に読替
- **外す（移植不可）**: パラメータ拡張 / GKD / LoRA / RL(ReSTEM) による**重み更新**。OpenCrab は
  API/CLI 経由の LLM 消費側で重みは触れない
- **本人採点を採る理由**: §1.1。reward hacking は客観判定でなく **可逆 archive ＋ 運営者可視化・
  上書き**（§7）で抑える

## 2. スコープ

### やること
- スリープ（既存 `memory_maintenance` ループ）に **スキル棚卸しパス**（新規コンポーネント）を追加
- 棚卸しは **本人の人格で判断**し、決定を **DB 直操作**で反映（§6.2, ActionContext は組まない）
- 判断材料は各スキルの **成果シグナル（定性ラベル）＋ベストエフォートの利用手掛かり**（数値閾値なし）
- 効かないスキルは **可逆 archive**（ハード削除しない）。復活も本人/運営者が可能
- スリープで起きたことを **2層で監査ログ化**（構造化 + 生プロンプト/生応答）
- 本人がスキルを整理するアクション `retire_my_skill` / `restore_my_skill` を新設（wake 時にも使える）
- （Phase 2・任意）信頼ピアのスキルを見て **助言** を生成（非権威, 既定 off）

### やらないこと（非目標）
- モデルの重み学習（LoRA/蒸留/RL）——不可能
- 平均・閾値・順位による**機械的な自動 prune**——§1.2/§1.3 で撤回
- 客観的「正しさ」の判定 / エージェント間の正規化——§1.1 に反する
- 会話中の relevance ベース スキル選別——本設計の前提外（§6.1 の限界の原因。将来課題）

## 3. アーキテクチャ概要

```
[Wake] エージェントが会話・行動（既存）
   └─ アクション実行は session_logs(log_type=tool_call 等) に既に残る（§6.1 の利用手掛かり素材）

[Sleep] memory_maintenance ループ（既存, 既定600秒ポーリング）
   ├─ ① メモリ統合（既存: 索引ビルド / キーワード補完 / 月次rollup）
   └─ ② スキル棚卸しパス（新規, 蓄積量ベースの専用ゲート §5）
        1. パケット組立: 各スキル {guidance, 成果ラベル, 利用手掛かり} を要約（§6.1）
        2. 人格判断:     本人のモデル＋人格で棚卸しを判断（LLM 1回, §6.2）
        3. 反映:         決定を DB 直操作で適用（archive/insert/update）＋監査ログ
        4. 監査ログ:     何を・なぜ したかを2層で永続化（§9）
        5. (任意)助言:   信頼ピアの効いてるスキルから助言生成（非権威, §7.2）
```

新規活動が無い tick は棚卸しゲート（§5）が弾いて即 return。走る時だけ LLM を1回消費する。

## 4. これは「新規コンポーネント」である（正直な明記）

レビュー（1回目 #2, 2回目 #2）の通り、棚卸しパスは **既存流用では成立しない**:

- `evaluate_response`（`llm_evaluation.rs`）は agent の action ループ内で引数の採点値を DB に書くだけ。
  LLM 判断は含まず、流用元にならない
- `run_maintenance_tick`（`memory_maintenance.rs:116`）には会話ロードも **ActionContext も
  dispatcher も workspace も無い**。渡っているのは `LlmRouterAdapter`・persona のみ

したがって「パケット組立 → 人格プロンプト → LLM 呼び出し → 応答パース → **DB 直操作でスキル反映** →
監査ログ」は **新規実装**。ActionContext を組んで既存アクションをディスパッチする重い道は採らない（§6.2）。

## 5. トリガ — 蓄積量ベース ＋ 時間フロア（循環と cold-start を排除）

```
棚卸しパスを発火する条件（いずれか）:
  (A) 前回棚卸し以降の「新規活動」が N件たまった      ← 密度に自動追従
  (B) 前回棚卸しから time_cap_hours 経過 かつ 新規活動が1件以上ある   ← 保険
制約:
  最短間隔 min_interval_secs（例 1h）は空けない
```

- **「新規活動」の定義（1回目 #7 の循環修正）**: 「前回 `last_skill_consolidation_at` 以降に新規
  ログ/終了を持つセッション数」。**採点済み件数ではなく未処理の活動量**（採点はパス内部でやるので
  「採点済み」で数えると常に0で発火しない循環に陥る）
- **初期値（2回目 #3 の cold-start 暴発修正）**: `last_skill_consolidation_at` の初期値 =
  **本機能のマイグレーション適用時刻（now）**。エージェント作成時刻にすると、履歴持ちの既存
  エージェント導入時に全履歴が「新規活動」に見え、初回 tick で全エージェントが一斉発火して
  LLM 支出スパイク＋全スキル一斉ミューテーションを起こす。now シードで既存履歴を初回母数に含めない
- `N` 初期値目安 = 10 セッション。すべて config 化（§10）

## 6. 棚卸しの中身

### 6.1 判断材料（定性・ベストエフォート・数値閾値なし）

各スキルについて本人に渡すパケットを組む。**数値で切らない**ので厳密な per-session スコア集約
（1回目 #1）は不要。次を素材にする:

- `guidance` / 説明 / 作成経緯（source）
- **成果シグナル → 定性ラベルで提示（§1.3）**: そのスキルが関与したセッションの verify 段評価
  （`session_logs.log_type=evaluation` の score）や `evaluate_response`（`llm_usage_metrics
  .quality_score`）を、`helped` / `mixed` / `unclear` / `signal-insufficient` に丸めて渡す。
  **生の平均値・順位は渡さない**（平均比較＝均質化の誘発を避ける）
- **利用手掛かり（ベストエフォート・弱いヒント）**: スキルの宣言アクション（`skills.actions`）が
  棚卸し対象期間のセッションで発火したか（`session_logs.log_type=tool_call` 等と突合）。
  **注記**: 共有アクション（`send_speech` 等）はスキル間で重複するため帰属は曖昧。あくまで弱い
  ヒントで、これで keep/retire を決めない
- 類似・重複スキルの並び（統合/細分化の判断用）

> **利用シグナルの限界（2回目 #1）**: OpenCrab は会話中に全アクティブスキルを一律注入しており
> （`process.rs build_agent_context`, relevance 選別なし）、「注入回数」は全スキルでほぼ定数になり
> 差別化に使えない。rev.2 が案にした注入時記録テーブル（`skill_usage_log`）は**廃止**し、上記の
> アクション発火ベースの弱いヒントに留める。精密な利用帰属は relevance ベース スキル選別（将来課題,
> §2 非目標）が入るまで得られない、と割り切る。**主素材は成果ラベルと本人の判断。**

### 6.2 判断と反映（本人の人格・LLM 1回・DB 直操作）

本人のモデル＋人格でパケットを見て棚卸しを判断。出力は各スキルへのアクション
（keep / retire / refine / subdivide / merge）＋**本人の理由**。

反映は **DB 直操作**で行う（2回目 #2）:
- `archive_skill` / `insert_skill` / `update_skill`（`skills.rs`）を直接呼ぶ
- **ActionContext を組まない**。`create_my_skill` 経由にすると workspace/dispatcher が要り非自明
- スリープで新規生成/改良したスキルは **DB-only（`file_path = None`）** とする。これは既存の
  `acquire_skill`（`skill.rs`）が既に DB-only スキルを作るのと同じ扱いで、**DB とワークスペースの
  乖離を新たに生まない**（`skills/*.skill.md` を生成しないのは acquired と同一挙動）
- 排他: 棚卸しはスキル表を書き換えるため **per-agent の in-flight ガード**を設ける。既存
  `try_acquire_build_slot`（`memory_maintenance.rs:40`, agent_id キーの汎用パターン）を別キーで流用
- 冪等/衝突: 既存の古さ7日ゲート（`find_unused_skills`）は**実際には archive せずログのみ**
  （`skill.rs:221`）なので、本人 retire と衝突しない

### 6.3 effectiveness の位置づけ
`skills.effectiveness` は**表示・並べ替え用のソフトスコア**。本人判断や成果ラベルから更新してよいが、
**これで自動 archive はしない**。用途はダッシュボード可視化のみ（パケットには生値でなく §6.1 の
定性ラベルを渡す）。

## 7. 剪定の安全網 と ピア

### 7.1 安全網（統計ガードではなく可逆＋可視化＋対称アクション）
- **可逆 archive**: `retire_my_skill`（id で archive）↔ `restore_my_skill`（**id で un-archive, 新設**）で
  対称化（2回目 #7）。rev.2 は復活を `create_my_skill` の全内容再投入に頼っていて非対称だった
- **本人による再検討**: 棚卸しパケットに **archived スキルを `include_archived_in_review` 件含める**
  ことで、本人が後日「戻す」判断をできる（1回目 #4 の片道問題の緩和）
- **復活の駆動力**（2回目 #7）: 長期間 archived のまま放置されたスキルは、ダッシュボードで
  「整理候補（恒久削除の提案）」として運営者に可視化。放置＝死蔵を運営者が最終判断できる
- **運営者可視化・上書き**: §9 の監査ログ＋ダッシュボードで retire/refine を運営者が取り消せる

### 7.2 ピア（Phase 2・任意・既定 off）
- 既存資産（実在確認済み）: `trusted_co_agents`（`schema.rs:1284`）＋ ピアの `list_skills` ＋
  `learn_from_peer` アクション（`learning.rs`）
- 弱い領域に対し信頼ピアの効いてるスキルから **本人の人格の声で助言**を生成。**採否は本人。
  自動採用しない。** ダッシュボード表示 or 次ターンの反省材料
- config トグル、**既定 off**（権威を持たせない版）

## 8. データモデルの変更

### 8.1 新規アクション（wake 時にも使える／sleep は同じ DB 関数を直接呼ぶ）
- `retire_my_skill`（id で `archive_skill` を叩く薄い action）
- `restore_my_skill`（id で un-archive する薄い action, 対称化）

> rev.2 で案にした `skill_usage_log` テーブルは **廃止**（§6.1）。利用手掛かりは session_logs の
> 既存アクション記録から棚卸し時に導出し、新テーブルは持たない。これに伴い `build_agent_context` の
> シグネチャ改修（session_id 引き回し, 2回目 #4）も不要になる。

### 8.2 per-agent 状態: `last_skill_consolidation_at`
memory index config 系テーブルに1カラム追加 or KV。**初期値 = マイグレーション適用時刻(now)**（§5）。

### 8.3 既存の活用
- `archive_skill` / `insert_skill` / `update_skill` / `find_skill_by_name_any`（`skills.rs`）
- `skills.effectiveness`（ソフトスコア, §6.3）
- verify段評価（`session_logs`）/ `llm_usage_metrics.quality_score` = 成果ラベルの素材（§6.1）
- `session_logs`(`log_type=tool_call`)＝アクション発火の利用手掛かり（§6.1）
- `memory_maintenance` の人格スレッド（`persona_name`/`personality` は tick に既渡し）
- マイグレーションは `MIGRATIONS`（`schema.rs:333`, `PRAGMA user_version`）に次番号で追加

## 9. スリープ監査ログ（2層・生プロンプト/生応答まで残す）

自律的な棚卸しは silent にスキルを消す/変えるため、**「原則 VII: 後から読める」**（`process.rs:798`）
を満たす監査が必須。2層で残す。

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
  "llm_log_ids": ["..."],
  "errors": []
}
```

### 層2: 生プロンプト/生応答 → 既存 `llm_logs`
`llm_logs`（`schema.rs:1341`: `prompt TEXT`/`response TEXT` + tokens/latency/model/agent_id/
session_id/error）は LLM 呼び出しの生 prompt/response をフル保存する既存機構。

- **確定した配線コスト（2回目 #5）**: llm_logs へ insert しているのは **SkillEngine のログ
  コールバック**（`process.rs:849` の `set_log_callback`）のみで、`LlmRouterAdapter` 自体は書かない。
  スリープの `run_maintenance_tick` は **engine を介さず bare な adapter**（`memory_maintenance.rs:155`）
  を使うため、**棚卸し LLM 呼び出しごとに `insert_llm_log` を明示的に呼ぶ新規配線が必須**。
  「流用」ではあるが insert は手で書く（既存メンテナンス系LLMも現状 llm_logs 未記録）
- **保持ポリシー**: 生ログ肥大化のため `llm_logs` の保持期間/件数 prune を config 化（§10）。
  生ログは全コンテキストを含む旨をUIで注記

### ダッシュボード
- **スリープ履歴ビュー**（層1）: いつ・何を kept/retired/refined したか＋理由＋コスト
- 各エントリから **生プロンプト/生応答（既存 LLM ログ画面）へドリルダウン**
- retire/refine の **運営者取り消し（restore）導線**＋長期 archived の整理候補提示（§7.1）

## 10. config
```toml
[skill_consolidation]
enabled = true
trigger_new_sessions = 10       # N
time_cap_hours = 24
min_interval_secs = 3600
include_archived_in_review = 3  # 棚卸しパケットに含める archived 数（再検討用, §7.1）
peer_advice = false             # Phase 2（既定 off）

[llm_logs]
retain_days = 90                # 生 prompt/response 保持日数（0=無期限）
```

## 11. 検証（実装時）

### 単体
- トリガゲート: (A)新規活動件数 / (B)time_cap / min_interval / **初期値=now** の各分岐
- `retire_my_skill` / `restore_my_skill`: id で archive ↔ un-archive できる（対称・可逆）
- パケット組立: 成果シグナルが無いスキルは **`signal-insufficient` ラベル**で提示される
  （生値/順位が漏れていないことも assert ＝ §1.3 の均質化誘発回避）

### E2E
- 活動を仕込む → 棚卸しパス実行 → 本人判断に沿って DB が更新され、**監査ログ層1＋生ログ層2** が
  両方残ることを assert
- ゲート: 新規活動が無ければ no-op。**既存履歴のみのエージェントに初回導入しても暴発しない**
  （初期値 now のため活動0扱い）ことを assert（cold-start 回帰）
- archived スキルが次パケットに含まれ、本人が restore を選べる

### 思想の回帰テスト（§1.1 の担保・ただし限界を明記）
- 同一スキル集合 × 人格の異なる2エージェントに、人格差で異なる判断を返すスタブ LLM を与え、
  keep/retire 結果がエージェント毎に異なることを assert
- **限界（2回目 #6・正直に明記）**: これは「人格差が反映される配線」を確認するに留まり、実 LLM 運用で
  個性が保たれる**構造的保証ではない**。均質化の緩和は §1.3（機械判定撤回＋定性提示＋人格プロンプト）に依存

## 12. 段階リリース
1. **Phase 1（中核）**: `retire_my_skill`/`restore_my_skill` + スリープ棚卸しパス（人格判断・DB直反映・
   LLM1回）+ トリガ(now シード) + 監査ログ2層(llm_logs 明示配線) + ダッシュボード。ピアなし
2. **Phase 2（任意）**: ピア助言（非権威・既定 off）

## 13. 既知の割り切り（正直な前提）
- 本人判断は思想上の選択で、客観的正しさは保証しない（それが狙い＝§1.1）
- 個性の保持は**構造的には保証しない**。§1.3 の組合せで緩和するのみ
- 利用シグナルは弱い（全スキル一律注入のため）。主素材は成果ラベルと本人判断。精密な利用帰属は
  relevance 選別（将来課題）待ち
- 棚卸しは毎回 LLM を1回消費。トリガで頻度を抑える
- 生ログ保持は容量トレードオフ。`retain_days` で運営者が調整
- Phase 2 のピアは **助言まで**。権威化・自動採用は思想違反として禁止

## 付録: 第三者レビュー対応表
| # | 指摘（回） | 対応 |
|---|---|---|
| 1(1) | セッション自己スコア未定義 | 数値を撤廃し定性ラベル化（§6.1）。厳密集約不要 |
| 2(1) | 採点は新規コンポーネント | §4 で明記 |
| 3(1) | 平均prune が §1.1 と矛盾 | 機械prune撤回、本人判断へ（§1.2） |
| 4(1) | archive がループから片道 | archived をパケットに含め＋`restore_my_skill`＋運営者整理候補（§7.1） |
| 5(1) | 利用検出が応答一致で脆弱 | 注入時記録は無意味と判明→廃止。弱いヒントに格下げ（§6.1） |
| 7(1) | トリガ循環 | 「未処理の新規活動」ベースに再定義（§5） |
| 1(2) | 注入時記録が無意味化 | `skill_usage_log` 廃止、成果ラベル主導（§6.1, §8.1） |
| 2(2) | スリープに action 足場が無い | DB 直操作で反映・DB-only スキル・ActionContext不要（§6.2） |
| 3(2) | cold-start 暴発 | 初期値=マイグレーション時刻(now)（§5, §8.2） |
| 5(2) | llm_logs はスリープ経路から未記録 | 棚卸しLLMを明示 `insert_llm_log`（§9 層2） |
| 6(2) | advisory化は均質化を構造保証しない | 定性ラベル提示で誘発回避＋限界を正直に明記（§1.3, §11） |
| 7(2) | restore が非対称 | `restore_my_skill`（id）新設で対称化（§7.1, §8.1） |
