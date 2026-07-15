# 設計: スリープ時スキル棚卸し（エージェント自己 curation ループ）

> Status: 設計（未実装, 実装着手可） / rev.5（第三者レビュー #100 の1〜4回目指摘を反映）
> 関連: `docs/design-skill-system-v2.md`, `docs/design-memory-rollup-v2.md`
> 着想元: Behrouz et al. "Language Models Need Sleep: Learning to Self-Modify and Consolidate Memories" (arXiv:2606.03979, 2026)

## 1. なぜ（背景と目的）

OpenCrab は今、スキルを **作りっぱなし** にしている:

- `create_my_skill` で獲得したスキルは `effectiveness = None` のまま放置される
- `find_unused_skills` は「7日使われていない」= **古さ** でしか刈れず（しかも実際は archive せず
  ログ出力のみ, `skill.rs:221`）、内容の良し悪しで整理しない
- 「獲得 → 振り返り → 整理」の周期が無く、スキルは溜まる一方

これを、**エージェントがアイドル時（スリープ）に、自分の直近の経験を振り返って、自分のスキル棚を
自分の人格で棚卸しする**周期として閉じる。残す・引退・作り直す・細分化/統合を **本人が決める**。

### 1.1 最上位の設計思想（すべての判断軸）

**目的はエージェント毎に「強いベクトル（個性）」を育てることであり、正しさ・平均点ではない。**

- スキルX を極端に細分化して溜め込むエージェントがいてもよい。それが個性なら正解
- **平均点のエージェントなら、わざわざ人格を持たせる意味がない**
- 全エージェントを単一の基準へ均質化する仕組みは採らない

### 1.2 設計の変遷と根本的な気づき（レビュー3周で到達）

- **rev.1**: 「使用セッションの自己スコアが本人平均を下回ったら archive」= 統計ルールで刈った。
  §1.1 と数学的に矛盾（平均で切れば必ず約半数を刈る＝均質化）→ 撤回
- **rev.2/3**: 機械 prune をやめ本人判断へ。ただし「per-skill の effectiveness を**計算**して判断材料に
  する」路線を維持した結果、**計算が破綻**した:
  - OpenCrab は会話中に **全アクティブスキルを一律注入**する（`process.rs build_agent_context`,
    relevance 選別なし）。つまり「このスキルがこのセッションで使われた」という綺麗な信号が
    **構造的に存在しない**。注入回数も成果ラベルも **全スキルで uniform** になり差別化できない
- **根本的な気づき（rev.4）**: **per-skill の効果を"計算"しようとする限り破綻する**（アーキテクチャの
  制約）。計算をやめ、**エージェント本人が直近の経験を振り返って「どのスキルが自分に効いたか」を
  判断で帰属させる**方が、アーキテクチャにも §1.1（本人の人格で決める）にも忠実。metric 計算路線は
  そもそも思想と噛み合っていなかった

### 1.3 均質化の再発を能動的に防ぐ

計算した平均・順位を判断材料に渡すと、人格 LLM が自発的に平均比較して均質化に戻りうる。rev.4 は
**そもそも per-skill の計算値（平均・順位・スコア）を渡さない**。代わりに**セッション単位の生の
経験と結末**（後述）を渡し、帰属は本人に委ねる。個性の保持は**構造的には保証しない**——担保は
「機械判定の撤回＋計算値を渡さない＋人格プロンプト」による**緩和**であり、回帰テスト（§11）は
その必要条件を確認するに留まる（実 LLM 運用の個性を証明はしない、と正直に認める）。

### 1.4 論文との対応と、あえて外すもの
- **借りる**: Wake/Sleep 周期、オフライン自己整理（＝振り返り）、蓄積量ベースのトリガ
- **外す（移植不可）**: パラメータ拡張 / GKD / LoRA / RL による**重み更新**。API/CLI 消費側で重みは
  触れない。本設計は「モデルの self-modify」を「**エージェントの経験に基づくスキル棚 self-curate**」に読替
- **本人判断を採る理由**: §1.1。reward hacking は客観判定でなく **可逆 archive ＋ 運営者可視化・上書き**（§7）で抑える

## 2. スコープ

### やること
- スリープ（既存 `memory_maintenance` ループ）に **スキル棚卸しパス**（新規コンポーネント, §4）を追加
- 棚卸しは **本人の人格が直近の経験を振り返って判断**し、決定を **DB 直操作**で反映（§6）
- **per-skill の効果計算はしない**。判断材料はセッション単位の生の経験＋結末＋弱い利用ヒント（§6.1）
- 効かないスキルは **可逆 archive**（ハード削除しない）。復活も本人/運営者が可能
- スリープの内容を **2層で監査ログ化**（構造化 + 生プロンプト/生応答, §9）
- 本人がスキルを整理するアクション `retire_my_skill` / `restore_my_skill` を新設（wake 時にも使える）
- （Phase 2・任意）信頼ピアのスキルを見て **助言**（非権威, 既定 off）

### やらないこと（非目標）
- モデルの重み学習——不可能
- 平均・閾値・順位・per-skill スコアによる**機械的判定**——§1.2/§1.3 で撤回
- 客観的「正しさ」/ エージェント間の正規化——§1.1 に反する
- 会話中の relevance ベース スキル選別——本設計の前提外（§1.2 の制約の原因。将来課題）

## 3. アーキテクチャ概要

```
[Wake] エージェントが会話・行動（既存）
   └─ 利用の弱い記録: 応答にスキル名が出たセッションを skill_usage_log(session_id 付) に（§8.1）
   └─ 結末は既存の記録に残る: verify評価(session_logs log_type=evaluation) / quality_score

[Sleep] memory_maintenance ループ（既存, 既定600秒ポーリング）
   ├─ ① メモリ統合（既存: 索引ビルド / キーワード補完 / 月次rollup）
   └─ ② スキル棚卸しパス（新規, 蓄積量ベースの専用ゲート §5）
        1. 振り返り素材の組立: スキル一覧(guidance) + 直近セッションの要約(既存メモリ索引)
                              + セッション単位の結末(verify/quality) + 弱い利用ヒント（§6.1）
        2. 人格判断: 本人のモデル＋人格で「どれが効いたか/整理すべきか」を判断（LLM 1回, §6.2）
        3. 反映: 決定を DB 直操作で適用（archive/insert/update）
        4. 監査ログ: 何を・なぜ したかを2層で永続化（§9）
        5. (任意)助言: 信頼ピアの効いてるスキルから助言（非権威, §7.2）
```

新規活動が無い tick は棚卸しゲート（§5）が弾いて即 return。走る時だけ LLM を1回消費する。

## 4. これは「新規コンポーネント」である（正直な明記）

`run_maintenance_tick`（`memory_maintenance.rs:116`）には会話ロードも **ActionContext も dispatcher も
workspace も無い**（渡っているのは `LlmRouterAdapter`・persona のみ）。`evaluate_response` も引数の
採点値を書くだけで流用元にならない。したがって「振り返り素材の組立 → 人格プロンプト → LLM →
応答パース → **DB 直操作でスキル反映** → 監査ログ」は **新規実装**。ActionContext を組んで既存
アクションをディスパッチする重い道は採らない（§6.2）。

## 5. トリガ — 蓄積量ベース ＋ 時間フロア（循環と cold-start を排除）

```
発火条件（いずれか）:
  (A) 前回棚卸し以降の「新規活動」が N件たまった      ← 密度に自動追従
  (B) 前回棚卸しから time_cap_hours 経過 かつ 新規活動が1件以上   ← 保険
制約:
  最短間隔 min_interval_secs（例 1h）は空けない
```

- **「新規活動」の定義（1回目 #7 の循環修正）**: 「前回 `last_skill_consolidation_at` 以降に新規
  ログ/終了を持つセッション数」。採点はパス内部でやるので「採点済み件数」で数えると常に0で
  発火しない循環に陥る → **未処理の活動量**で数える
- **cold-start 暴発の完全修正（2回目#3 + 3回目#4 + 4回目#1）**: 重要な事実 —
  `get_memory_index_config` は行が無いとき**非永続のメモリ内デフォルトを返すだけで行を作らない**
  （`memory_index.rs:822`）。したがって rev.4 が書いた「遅延生成で now を刻む」は**成立しない
  （誤り）**。config 行の永続化は**明示 UPSERT のみ**。正しい機構は:
  1. `last_skill_consolidation_at` を NULL 許容で追加（SQLite の ADD COLUMN DEFAULT は定数のみ
     なので初期 NULL）＋専用 getter/setter を持つ（§8.3）
  2. **初回遭遇時（行なし or NULL）= シード**: 活動0扱いで **`set_last_skill_consolidation_at(now)`
     を UPSERT して行を作り**、return する（既存履歴を「新規活動」に数えない＝一斉暴発を防ぐ）
  3. **棚卸し実行後も必ず `last_..._at = now` を UPSERT** して永続化（次 tick の基準にする）
  → config 行は自動生成されないので、永続化を明示 UPSERT に一本化するのが肝（`memory_maintenance.rs:138`
  の `unwrap_or_else` は破棄されるメモリ内フォールバックであり「行を刻む」ではない）
- `N` 初期値目安 = 10 セッション。すべて config 化（§10）

## 6. 棚卸しの中身

### 6.1 振り返り素材（per-skill の計算値は渡さない）

本人に渡すのは、**metric ではなく生の経験と結末**:

- **スキル一覧**: 各スキルの `guidance` / 説明 / 作成経緯（source）
- **直近セッションの要約**: 既存のメモリ索引（`build_memory_index_section`, `context_section.rs:60`）が
  生成済みの月次要約・topic を流用。「最近どんな会話・行動をしたか」の文脈。スリープからは
  `session_id` に空文字を渡せばエージェント全体の要約が得られる（4回目#2 で確認）。ただし索引未構築の
  若いエージェントは `Ok(None)` で文脈が薄く、**初回棚卸しは素材が少ない**点は許容する
- **セッション単位の結末**: そのセッションが良かったか — verify 段評価
  （`session_logs.log_type=evaluation` の score）や `evaluate_response` の
  `llm_usage_metrics.quality_score`。**セッション単位で提示**（per-skill に按分・計算しない）
- **弱い利用ヒント（ノイズあり・決定には使わない）**: `skill_usage_log`（§8.1, 応答名前一致 +
  session_id）から「スキルX の名前が出たセッション群」。**唯一スキルごとに値が変わる信号**だが
  偽陽性/偽陰性を含むため、あくまで本人が「効いた会話」を思い出す手掛かり

本人は「良かったセッション群でどんな振る舞い/スキルが効いていたか」を**自分で帰属**して判断する。
**システムは per-skill の効果を計算しない**（§1.2 の気づき）。

> **限界（正直に明記, 3回目#1/#2/#5）**: 全スキル一律注入のため、機械的な per-skill 帰属は
> 原理的に uniform になる。だから帰属は本人の judgment に委ねる。利用ヒントは既存 `record_used_skills`
> の名前一致（`skill_mentioned`, 脆弱）ベースで、精密な帰属は relevance 選別（将来課題）待ち。
> **`skills.actions` は DB に保存されない**（`create_my_skill` は YAML のみ、acquired は空）ため、
> rev.3 で案にした「actions ⋈ tool_call」突合は**採用しない**。

### 6.2 判断と反映（本人の人格・LLM 1回・DB 直操作）

出力は各スキルへのアクション（keep / retire / refine / subdivide / merge）＋**本人の理由**。
反映は **DB 直操作**（`archive_skill`/`insert_skill`/`update_skill`, `skills.rs`）:

- **ActionContext を組まない**（`create_my_skill` 経由にすると workspace/dispatcher が要り非自明, §4）
- スリープ生成/改良スキルは **DB-only（`file_path=None`）**。これは既存 `acquire_skill`（`skill.rs`）の
  挙動と同じで、DB/workspace 乖離を新たに生まない
- **`situation_pattern` の扱い（3回目#3）**: このフィールドは実装上、二重の意味を持つ地雷
  （`create_my_skill` は状況パターン散文を入れ、`row_to_skill` は actions JSON として解釈する）。
  スリープ**新規生成**物は **`acquire_skill` と同じく `situation_pattern=""`** にして解釈衝突を避ける
  （actions は元々 DB で使えないので欠落は許容）。**ただし refine（既存スキルの `update_skill`）では
  既存 `situation_pattern` を保持し `""` で上書きしない**（外部由来の状況記述散文を消す副作用を防ぐ,
  4回目#3）。空にするのは新規生成スキルのみ
- 排他: 既存 `try_acquire_build_slot`（`memory_maintenance.rs:40`, agent_id 素キー）を、増分ビルドと
  相互排他しないよう **名前空間キー**（例 `skillcuration:{agent_id}`）で流用する
- 冪等/衝突: 古さ7日ゲート（`find_unused_skills`）は実際には archive せずログのみ（`skill.rs:221`）なので
  本人 retire と衝突しない

### 6.3 effectiveness の位置づけ
`skills.effectiveness` は**ダッシュボード表示用のソフトスコア**。本人が「効いた」と評価した記録から
更新してよいが、**棚卸しパケットには渡さない**（§1.3: 計算値を判断材料にしない）。自動 archive にも使わない。

## 7. 剪定の安全網 と ピア

### 7.1 安全網（統計ガードでなく可逆＋可視化＋対称アクション）
- **可逆 archive**: `retire_my_skill`（id で `archive_skill(true)`）↔ `restore_my_skill`（id で
  `archive_skill(false)`, **新設**）で対称化。既存 `restore_skill`（`skill.rs:213`）が実体
- **本人による再検討**: 棚卸し素材に **archived スキルを `include_archived_in_review` 件含める**
- **復活の駆動力**: 長期 archived 放置スキルはダッシュボードで「整理候補（恒久削除提案）」として
  運営者に可視化。放置＝死蔵を運営者が最終判断
- **運営者可視化・上書き**: §9 の監査ログ＋ダッシュボードで retire/refine を運営者が取り消せる

### 7.2 ピア（Phase 2・任意・既定 off）
既存資産（実在確認済み）: `trusted_co_agents`（`schema.rs:1284`）＋ `list_skills` ＋ `learn_from_peer`。
弱い領域に対し信頼ピアの効いてるスキルから **本人の人格の声で助言**。**採否は本人・自動採用しない**。
config トグル既定 off（権威を持たせない版）。

## 8. データモデルの変更

### 8.1 `skill_usage_log`（最小・**利用時**に記録・弱いヒント専用）
rev.2 で「注入時記録」は無意味と判明したため注入時ではなく、**利用が検出された時**に記録する。
既存 `record_used_skills`（`process.rs:1096`）が応答名前一致で `usage_count` を +1 する所に、
**session_id 付きの1行 insert を足す**だけ:
```sql
CREATE TABLE skill_usage_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL, skill_id TEXT NOT NULL, session_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_skill_usage_log_skill ON skill_usage_log(skill_id);
```
名前一致はノイズが多い（3回目#5）が、**唯一スキルごとに値が変わる信号**であり、§6.1 で弱いヒントと
してのみ使う（決定には使わない）と割り切る。`record_used_skills` は `session_id` を既に持つ
呼び出し文脈にある（`process.rs:1452` は run 後）ので配線可能。

### 8.2 新規アクション（wake でも使える／sleep は同じ DB 関数を直接呼ぶ）
- `retire_my_skill`（id で `archive_skill(true)`）
- `restore_my_skill`（id で `archive_skill(false)`, 対称化）

### 8.3 per-agent 状態: `last_skill_consolidation_at`
`agent_memory_index_config` に NULL 許容カラム追加。`AgentMemoryIndexConfig` 構造体・SELECT を拡張し、
**専用 setter `set_last_skill_consolidation_at`（行が無ければ作る UPSERT）** を新設する。
`get_memory_index_config` は行が無いとき**非永続デフォルトを返す**（`memory_index.rs:822`）ため、
**永続化は明示 UPSERT に一本化**（§5: 初回シード＋実行後 UPSERT）。既存
`upsert_memory_index_config`（`memory_index.rs:833`, ON CONFLICT DO UPDATE）と同型で実装できる。

### 8.4 既存の活用
- `archive_skill`/`insert_skill`/`update_skill`/`find_skill_by_name_any`（`skills.rs`）＝ DB 直反映
- `build_memory_index_section`（`memory_index/`）＝ 直近セッション要約の素材
- verify段評価（`session_logs`）/ `llm_usage_metrics.quality_score`（session_id 付）＝ 結末素材の**元データ**。
  **注（4回目#2・正直）**: 列は存在するが「直近セッション群の結末を **agent 単位**でまとめて引く」関数は
  無い（`list_recent_session_logs` は単一 session keyed, `get_recent_evaluations` は session_id を返さない）。
  → **新規クエリ2本が必要**: ①`memory_sessions WHERE agent_id AND log_type='evaluation' ORDER BY id DESC
  LIMIT n` ②`quality_score` を session_id 付きで束ねる。列は揃っており実装は自明だが「既存の活用」ではない
- `insert_llm_log`（`llm_logs.rs:35`, `session_id: Option`）＝ 生ログ（§9）
- マイグレーションは `MIGRATIONS`（`schema.rs:333`）に次番号で追加

## 9. スリープ監査ログ（2層・生プロンプト/生応答まで）

**「原則 VII: 後から読める」**（`process.rs:798`）を満たす。

**層1: 構造化監査 → `agent_logs`（`context="sleep"`）**（`schema.rs:1381`）。1スリープ=1エントリ:
```json
{ "trigger":"activity>=N|time_cap", "memory":{...既存 MaintenanceReport...},
  "skill_curation":[{"skill":"..","action":"kept|retired|refined|created|merged","reason":".."}],
  "cost":{"llm_calls":1,"tokens":..}, "llm_log_ids":[".."], "errors":[] }
```

**層2: 生プロンプト/生応答 → 既存 `llm_logs`**（`schema.rs:1341`, prompt/response フル保存）。
- **確定した配線（3回目で確認）**: llm_logs へ insert するのは SkillEngine のログコールバック
  （`process.rs:849`）だけで、スリープの bare `LlmRouterAdapter`（`memory_maintenance.rs:155`）は
  書かない。よって**棚卸し LLM 呼び出しごとに `insert_llm_log` を明示的に呼ぶ新規配線が必須**。
  `session_id` はスリープに user session が無いので **`None`**（`insert_llm_log` は Option で受ける）
- **保持ポリシー**: `llm_logs` の保持期間 prune を config 化（§10）。生ログは全文脈を含む旨をUI注記

**ダッシュボード**: スリープ履歴ビュー（層1）→ 生プロンプト/応答（既存 LLM ログ画面）へドリルダウン。
retire/refine の restore 導線＋長期 archived の整理候補提示。

## 10. config
```toml
[skill_consolidation]
enabled = true
trigger_new_sessions = 10
time_cap_hours = 24
min_interval_secs = 3600
include_archived_in_review = 3
peer_advice = false

[llm_logs]
retain_days = 90   # 生 prompt/response 保持日数（0=無期限）
```

## 11. 検証（実装時）

### 単体
- トリガ: (A)新規活動 / (B)time_cap / min_interval / **NULL初回 now 刻み**の各分岐
- `retire_my_skill`/`restore_my_skill`: id で archive ↔ un-archive（対称・可逆）
- 素材組立: per-skill の計算スコア/順位/平均が**渡されていない**ことを assert（§1.3 の均質化誘発回避）
- スリープ生成スキル: `situation_pattern=""`・`file_path=None`（row_to_skill が壊れない）

### E2E
- 活動を仕込む → 棚卸し実行 → 本人判断どおり DB 更新＋監査ログ層1＋生ログ層2 が残る
- **cold-start 回帰**: 既存履歴のみのエージェント（config 行なし含む）に初回導入して**暴発しない**
- archived が素材に含まれ本人が restore を選べる

### 思想の回帰テスト（§1.1・限界も明記）
- 同一スキル集合 × 人格の異なる2エージェント → 人格差で異なる判断を返すスタブ LLM → keep/retire が
  エージェント毎に異なることを assert
- **限界（2回目#6・正直）**: これは「人格差が反映される配線」の確認に留まり、実 LLM 運用の個性を
  構造的に保証しない。均質化緩和は §1.3 に依存

## 12. 段階リリース
1. **Phase 1（中核）**: `skill_usage_log`(利用時) + `retire_my_skill`/`restore_my_skill` + スリープ棚卸し
   （振り返り判断・DB直反映・LLM1回）+ トリガ(now シード3経路) + 監査ログ2層(llm_logs 明示) +
   ダッシュボード。ピアなし
2. **Phase 2（任意）**: ピア助言（非権威・既定 off）

## 13. 既知の割り切り（正直な前提）
- 本人判断は思想上の選択で、客観的正しさは保証しない（それが狙い＝§1.1）
- **per-skill の効果は計算しない**。全スキル一律注入のため原理的に uniform になる。帰属は本人の
  judgment。利用ヒントは名前一致ベースで脆弱、精密化は relevance 選別（将来課題）待ち
- `skills.actions` は DB で使えない（`situation_pattern` の二重意味）。スリープ生成物は空にして回避
- 個性の保持は**構造的には保証しない**。§1.3 の組合せで緩和のみ
- 棚卸しは毎回 LLM を1回消費。トリガで頻度を抑える
- 生ログ保持は容量トレードオフ。`retain_days` で調整
- Phase 2 のピアは **助言まで**。権威化・自動採用は思想違反として禁止

## 付録: 第三者レビュー対応表
| # | 指摘（回） | 対応 |
|---|---|---|
| 1(1) セッション自己スコア未定義 | per-skill 計算を撤廃、セッション単位の生の結末を渡す（§6.1） |
| 2(1) 採点は新規 | §4 明記 |
| 3(1) 平均prune が §1.1 矛盾 | 機械prune撤回（§1.2） |
| 4(1) archive 片道 | archived を素材に含め＋`restore_my_skill`＋整理候補（§7.1） |
| 5(1) 利用検出脆弱 | 名前一致は弱いヒントに格下げ・決定に使わない（§6.1, §8.1） |
| 7(1) トリガ循環 | 未処理活動ベースに再定義（§5） |
| 1(2) 注入時記録が無意味 | 注入時記録を廃し、利用時記録＋本人帰属へ（§6.1, §8.1） |
| 2(2) action 足場なし | DB直操作・DB-only・ActionContext不要（§6.2） |
| 3(2) cold-start | now シード（§5, 実装は3回目#4で完全化） |
| 5(2) llm_logs 未配線 | 明示 `insert_llm_log`（session_id=None）（§9） |
| 6(2) 均質化を構造保証しない | 計算値を渡さない＋限界を正直明記（§1.3, §11） |
| 7(2) restore 非対称 | `restore_my_skill` 新設（§7.1, §8.2） |
| 1(3) 成果ラベルの帰属不能 | per-skill 計算を全廃、帰属は本人 judgment（§1.2, §6.1）＝ 根本転換 |
| 2(3) actions ⋈ tool_call 不能 | 突合を採用しない（`skills.actions` は DB 非保存）（§6.1） |
| 3(3) situation_pattern 破壊 | スリープ生成物は `situation_pattern=""`（§6.2） |
| 4(3) now シード未閉 | rev.4 は「遅延生成で刻む」と誤り→rev.5 で明示 UPSERT に一本化（§5, §8.3） |
| 5(3) 既存利用検出無視 | `record_used_skills` の脆弱性を明記し弱いヒント化（§6.1, §8.1） |
| 1(4) cold-start が非永続で発火せず | config 行は自動生成されない。初回シード＋実行後 UPSERT で永続化（§5, §8.3） |
| 2(4) 結末素材は「既存の活用」でなく新規クエリ2本 | §8.4 で正直に格下げ＋初回は素材薄と明記（§6.1, §8.4） |
| 3(4) refine で situation_pattern 散文消去 | refine は既存を保持、空は新規生成のみ（§6.2） |
