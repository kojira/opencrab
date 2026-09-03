# テストハーネス棚卸し（DESIGN-TURN-CONTINUATION §13 カバレッジ監査）

**判定基準**: `DESIGN-TURN-CONTINUATION.md §13`（1 生成の形 16 ケース）＋ ターン合計（QC シナリオ 5 本）
＋ §13.1（表から設計に戻った 10 点 a〜j）。各行が「どのテストで／どの観測点まで／最外層で assert
されているか」を表にし、未カバー＝**穴**を列挙する。観測点の語彙は `TEMPLATE-TDD-INSTRUCTION.md §1`
の観測境界に揃える。

**観測境界（テンプレ §1）**: ①配送回数/本文（REST `responses`／ゲート say・op 呼び出し）・
②保存件数/本文（memory_sessions = `session_logs` の speech 行）・③LLM 呼び出し回数/イテレーション・
④残留マーカー（NO_REPLY/CONTINUE が配送・保存・次プロンプトに無い）・⑤ゲート反応（👀🏁🤐❌）。

**結論（要約）**:
- 2026-09-02 QC の 3 件（#898/#899/#900）は、**最外層で ①②③⑤ を回数つきで pin していなかった**行に
  集中して漏れた。エンジン層テストは `on_response_text` の**モック callback 発火**（内部境界）を pin
  するのみで、実配線の配送回数・保存件数・ゲート反応（🤐）を観測していなかった。
- 本 PR は §13 の穴のうち **#2/#9/#11/#12/#6(🤐)** を最外層の赤テストで埋めた（下表 ★）。
- 本 PR で追加した穴埋め（第 2 弾）: §13 #7（緑）・§13.1 a（赤）・c（赤）・f（緑）・g-reaction（赤）。
- 残る穴（#8/#14/#16 の最外層・§13.1 d/e/g-repost/j）は下表 △。実装 PR（#901/#903/#904）または後続で埋める。
- 監査中に恒真の裏返し（共有バッファ相互汚染）を `scenario_b`・`scenario_c` に見つけ発端 message で scope して是正した（§4）。
- **rebase 状況（tip=2e02a00d・#901/#903/#904 全込み）**: 監査赤ピンは全て緑化（#901 で `audit_898`/`audit_s13_1c`/
  `continue_marker_j`＝§13.1 a の warn 実装、#903 で `audit_899a`、#904 で `audit_900b`/`audit_900c`/`audit_s13_1g_reaction`）。#905 は現 tip で**全緑**。
- #904 レビュー所見の非回帰ピンとして gate ハンドラ `is_utterance` と正典 `is_known_utterance_op` の
  パリティを `utterance_parity.rs` に固定（§6・現 tip 緑）。

凡例: ★=本 PR で追加した最外層赤テスト・○=既存の最外層テストで pin 済み・◇=エンジン/ゲート層のみ
（最外層は未 pin）・△=最外層に穴（未カバー）。テスト名の無印は緑（非回帰）、(赤) は現 tip で赤。

---

## 1. §13 ケース × 観測点 カバレッジ

| §13 | 生成の形 | 期待（配送/保存/次/残留/🤐） | 最外層テスト | 判定 |
|---|---|---|---|---|
| #1 | 本文のみ | 本文1/1/終了/なし/付けない | `scenario_a_mention_becomes_say`(nostr)・`scenario_a_message_..`(discord) | ○（配送・ターン数。保存は未 pin） |
| #2 | 本文＋最終行 CONTINUE | 本文1/1/**進む**/なし/付けない | ★`audit_898_continue_split_..`(赤) を 3 連鎖 | ★（配送3/保存3/LLM3/残留） |
| #3 | CONTINUE のみ（本文空） | 0/0/進む/なし/— | エンジン `continue_marker_e`（次呼ばない側）・機構は #16 経路 | ◇（最外層で空生成配送0 は未 pin） |
| #4 | 「…です CONTINUE」同一行 | 文字列1（剥がさない）/1/終了/文字列残る/付けない | `continue_marker.rs::same_line_..`・engine `continue_marker_f2` | ◇（配送層純関数のみ） |
| #5 | 途中行 CONTINUE・最終行本文 | 全文1/1/終了/残る＋warn1/付けない | engine `continue_marker_f`・`continue_marker.rs::midtext_..` | ◇（warn 発生の最外層 pin なし） |
| #6 | reply×N（本文なし） | reply N/N/終了/なし/**付けない** | `scenario_a3_reply_..`・`scenario_a3_three_replies_..`(nostr 配送3/LLM1)／★`audit_900c_..`(discord 赤・🤐 0) | ○＋★（🤐 なしは本 PR で追加） |
| #7 | reply×N＋本文 | reply N＋本文1/N+1/終了/なし/付けない | `audit_s13_7_replies_plus_body_..`(nostr・非回帰緑) | ○（配送 N+1・保存 N+1・LLM1） |
| #8 | reply×N＋本文＋最終行 CONTINUE | reply N＋本文1/N+1/**進む**/なし/付けない | — | △ **穴**（発話＋本文＋継続の複合未 pin） |
| #9 | reply×N＋CONTINUE のみ | reply N/N/**進む**/なし/付けない | ★`audit_900b_reply_plus_continue_..`(赤) | ★（配送3/LLM3） |
| #10 | query ツール＋本文（holding） | 本文1（宣言）/1/ツール経路で進む/剥がすだけ/付けない | `scenario_main_second_request_..`・`scenario_shell_stdout_..` 等 | ○（settle/resume・resume 本文。保存件数は未 pin） |
| #11 | NO_REPLY のみ | 0/**0**/終了/なし(DB にも無)/**付ける** | ★`audit_899a_..`(nostr 赤・配送0/保存0/履歴なし)／`scenario_e_no_reply_..`(discord 🤐 付ける) | ★＋○ |
| #12 | 本文＋末尾 NO_REPLY | 本文1/1/終了/なし(以降破棄+ログ)/付けない | `scenario_no_reply_terminates_..`(nostr 配送・破棄ログ)／★`audit_899b_..`(保存1・非回帰緑) | ○＋★ |
| #13 | NO_REPLY＋CONTINUE 両方 | 0/0/終了(NO_REPLY 優先)/なし/付ける | engine `continue_marker_g`・`continue_marker.rs::nostr_no_reply_wins` | ◇（最外層で 配送0/🤐 未 pin） |
| #14 | reply×N＋NO_REPLY | reply N/N/終了/なし/付けない（発話あり） | — | △ **穴**（reply＋NO_REPLY で 🤐 なしを最外層で未 pin） |
| #15 | 発話 op が permission denied | 既存 ❌/turn_failed | 既存 #883 経路（extgate/discord） | ○（既存・本監査の主対象外） |
| #16 | CONTINUE 連鎖が max_iterations(depth0=30) | 各イテレーション分/同/上限停止 stopped_by_limit warn/なし/付けない | engine `continue_marker_d`（max=3 で停止） | ◇（最外層で全配送＋stopped_by_limit 未 pin） |

### ターン合計（QC シナリオ）

| シナリオ | LLM/配送/保存 | 最外層テスト | 判定 |
|---|---|---|---|
| plain3（本文＋CONTINUE ×2→本文） | 3/3/3 | ★`audit_898_..`(赤) | ★ |
| reply3-in-one | 1/3/3 | `scenario_a3_three_replies_..`(配送3/LLM1)＋★`audit_900c_..`(🤐 0) | ○＋★（保存3 は未 pin＝△） |
| reply1＋CONTINUE ×2→reply1 | 3/3/3 | ★`audit_900b_..`(赤・配送3/LLM3) | ★（保存3 は未 pin＝△） |
| noreply（NO_REPLY のみ） | 1/0/0 | ★`audit_899a_..`(赤) | ★ |
| sleep60（execute_shell→settle→完了報告） | 2/2/2 | `scenario_main_second_request_..`・`scenario_shell_stdout_..` | ○（配送順・resume 本文。保存件数は未 pin） |

## 2. §13.1（表から設計に戻った 10 点）× カバレッジ

| §13.1 | 設計決定 | 最外層テスト | 判定 |
|---|---|---|---|
| a | 空 CONTINUE 連続 3 回で warn 1 行（停止せず） | `continue_marker_j_empty_chain_..`(engine・赤) | ★（warn 1 行＋停止しないを pin・現 tip 赤） |
| b | Nostr 各イテレーション=同一宛先への 1 投稿 | ★`audit_898_..`（3 standalone say が順に出る） | ★（宛先相関までは未 pin） |
| c | Discord 各イテレーション=1 メッセージ（結合/編集しない） | `audit_s13_1c_continue_split_..`(discord・赤) | ★（3 分割=3 メッセージ・結合なし） |
| d | REST 各イテレーション=`responses` に 1 要素追加 | — | △ **穴**（REST レーンの CONTINUE 分割 未 pin） |
| e | scheduler/intake/heartbeat 起点も同期待（NO_REPLY 保存なし含む） | heartbeat: `heartbeat_h1_two_posts_flag_only_on_last`/`heartbeat_h2_no_reply_stays_silent`/`heartbeat_h3_declaration_then_subtask_then_report`/`heartbeat_h4_unbound_gateway_fires_nothing`（discord_qc・#925）＋`heartbeat_h1_nostr_two_standalone_posts`（qc_harness）／scheduler・intake の平文起点: — | ★(heartbeat=非ユーザー起点ターンを配送/保存/LLM/残留/🏁/🤐/typing/warn で pin・H1 は say2・保存2・🏁 は2件目のみ・CONTINUE/NO_REPLY 残留0/ H2 沈黙・🤐 なし/ H3 宣言🏁0＋報告🏁1/ H4 未接続で warn1・配送0・LLM0・捏造0・Nostr は say2 standalone・🏁/🤐/typing 対象なし)＋△(**typing の開始順序（開始<本文1）は未 pin**＝typing broadcast が別 async タスク（`spawn_channel_typing`）で log を出し、say とのバッファ index 順が非決定。**停止（activity ended 後は tick 0）は決定形で pin**＝keepalive interval 8s を超えて待ち tick が増えないことで #915・DIRECTION-LOG 625 の「入力中が残る」を捕捉)＋△(scheduler/intake の平文起点は未 pin) |
| f | sub-engine(depth>0) も有効 | `continue_marker_i_sub_engine_..`(engine・緑) | ○（sub-engine profile で CONTINUE 継続＋max 上限・非回帰） |
| g | reaction/repost のみ＝#6（N 配送/N 保存/🤐 なし） | reaction: `audit_s13_1g_reaction_..`(discord・赤・🤐 なし)＋`scenario_c_reaction_..`／repost: — | ★(reaction 🤐)＋△(repost=同一 utterance 機構だが nostr の repost DI 配線が未確認の穴) |
| h | 空白のみ＋CONTINUE＝#3 | — | △（#3 と同じく最外層未 pin） |
| i | typed_history on/off で同一期待 | ★`audit_899a_..`（typed off で履歴に NO_REPLY なしを pin） | ○（両モードの網羅は未） |
| j | 途中配送失敗（ゲート error）は継続を止める（❌/turn_failed） | — | △ **穴**（配送失敗で継続停止の pin なし） |

## 3. 恒真疑い（エンジン/ゲート層の内部境界 pin）

| テスト | pin する境界 | 問題 |
|---|---|---|
| engine `test_on_response_text_fires_on_every_iteration` | モック callback の発火回数 | ⚠ **内部境界**。実配送・実保存を保証しない（#898 の温床） |
| engine `continue_marker_b_speech_then_marker_..` | 発話＋CONTINUE→LLM 2・**モック配送 Vec に 2 本** | ⚠ 配送 Vec はモック callback。実 nostr/discord 配送・speech 保存を通さない（§13 #2 の最外層穴） |
| gate `a_normal_reply_gets_no_no_reply_reaction`(discord wiring) | text 応答返信→🤐 でない | ⚠ **発話クラス（reply DI・最終 content 空）ターン**を観測せず（§13 #6 の 🤐 を取り逃した箇所） |
| discord `scenario_b`（是正前） | `count_kind(reply)==1`・unscoped `find(reply)` | ⚠ 「自分が唯一の reply producer」前提＝別テストを足すと壊れる恒真の裏返し。§4 で是正 |

**on_tool_call 経路の恒真是正（テストレビュー所見）**: `api_e2e::test_tool_only_generation_saves_no_empty_speech_899` は content=**空**のみを通し、旧 `!content.trim().is_empty()` filter でも同一に通る（#903 の NO_REPLY 対応を区別しない＝恒真）。`api_e2e::test_no_reply_text_with_tool_call_saves_no_speech_899_guard` を追加し、content=**"NO_REPLY"（非空）**＋照会 tool を on_tool_call に通して「NO_REPLY speech 保存 0・次ターン typed に NO_REPLY なし」を固定（ガードを旧 filter へ revert すると赤）。非 NO_REPLY 本文は保存される正の対照で恒真でないことを実証済み。

## 4. 監査中に是正した相互汚染（恒真の裏返し）

dry-run 観測 BUFFER は test binary 全体で共有・累積し never-clear（DB は各テスト独立）。
`scenario_b_reply_resolves_e_number_and_settles` は `count_kind(&buf,"reply")==1` と unscoped な
`find(|c| c.kind=="reply")` で唯一の reply producer を暗黙前提にしていた。本 PR で発端 message(701)/
body(B_REPLY) に scope して自己完結させた。**共有バッファのハーネスに count/find を書くときは必ず
自テストのマーカー（body/message）で scope する**（§5 観測点①に反映）。

## 5. 標準 assert セット（テンプレ §1 に転記済み・発話/継続/沈黙/ゲート変更は全部）

最外層で 5 観測点を**回数つき**で pin する（`.any` ではなく count・kind 区別・自マーカーで scope）:

1. **配送回数と本文** — REST `responses` 件数／ゲート say・op 呼び出し回数。kind（standalone/reply/
   reaction）を区別し、自テストの body/message マーカーで scope（§4 の相互汚染回避）。
2. **保存件数と本文** — memory_sessions=`session_logs` の speech 行を件数で。剥がすべきマーカー
   （NO_REPLY/CONTINUE）が content に**残らない**ことも。
3. **LLM 呼び出し回数/イテレーション** — mock カウンタ（`system_prompts().len()`／`AtomicUsize`）。
4. **残留マーカー** — 配送本文・保存本文・**次プロンプト**（Assistant ロールメッセージ）に
   NO_REPLY/CONTINUE が無い。
5. **ゲート反応** — 👀🏁🤐❌ の付与先（発端/自分の投稿）と回数。🏁 は count=1、activity ended
   起点、付け先は core が指定した最終生成の最後の投稿で pin する。あわせて「発話が 1 つでも
   あったターンに 🤐 を付けない」を発端 message id 相関で確認する（discord_qc のみ観測可）。

**アンチパターン**: エンジン層のモック配送コールバック発火だけで「配送された」と見なさない（§3 の ⚠）。
実配線（`delivery_effect`→`apply_delivery_effect`→dry-run 配送・speech 保存・system reaction）を通す。

---

## 6. 発話 op 分類のパリティ（#904 レビュー所見・非回帰）

`crates/server/tests/utterance_parity.rs::gateway_handler_is_utterance_has_parity_with_canonical`:
gate-client 各ハンドラの `InvokeHandler::is_utterance` と正典 `opencrab_gateway::is_known_utterance_op`
（say|reply|reaction|repost）のパリティを固定する。

- 各 gateway が宣言する op ごとに `handler.is_utterance(op) == is_known_utterance_op(op)`。
- discord 発話集合 = {reply,reaction}・nostr = {reply,reaction,repost}・いずれも ⊆ 正典。
- 正典が照会/操作 op（resolve/follow/unfollow/kind0/upload）を発話にしないことも明示ピン。

#904（edc2c478）で `is_utterance` がハンドラに入って以降で緑。gateway 側の発話分類が正典から
ずれる回帰（新 op を発話にし忘れる／照会を発話扱いする）を最外層で捕まえる。

## 付録: 最外層ハーネスと観測チャネル

- `crates/server/tests/qc_harness_e2e.rs`（nostr say/op レーン）: dry-run say バッファ
  `CapturedSay{kind, body}`／NO_REPLY 破棄ログ／`subtask_registries.has_running`／
  `session_logs`（speech/tool_call/tool_result）／mock `system_prompts().len()`。
- `crates/server/tests/discord_qc_harness_e2e.rs`（discord レーン）: dry-run
  `Captured{kind, body, emoji, channel, message}`（say/reply/reaction/system_reaction/typing）。
- エンジン層 `crates/core/src/engine/skill_engine.rs`・配送純関数 `crates/actions/src/continue_marker.rs`・
  ゲート層 `crates/discord/src/message_loop_wiring_tests.rs` は内部境界（§3）。
- REST レーン（§13.1 d・#7/#8 の穴）は `crates/server/tests/api_e2e.rs` 等が候補だが CONTINUE/発話
  分割配送は未 pin。

本監査の対象外（発話/継続/沈黙/ゲートを主観測点としない）: `api_e2e`/`e2e_local`/`output_limit_e2e`/
`web_*`/`extgate conformance`/`llm/*`。§13 に触れる変更が生じたら §5 の標準セットを追加する。
