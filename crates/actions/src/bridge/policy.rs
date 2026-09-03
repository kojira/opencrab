/// `ActionDispatcher::new()` が登録する **core アクション**のうち inline 実行のまま
/// にするもの（`default_non_dispatch_tools` の種）。
///
/// **なぜ core だけ名前リストが残るか**: 分類の権威は各ツール定義の属性
/// （`GatewayActionDef.class`）へ移した（PR-2B）。ただし core アクションは
/// `actions` クレート自身の一次ツールで `GatewayActionDef` を持たない（属性を名乗る
/// 構築サイトが無い）。基準は「**ゲート固有の名前かどうか**」で、Discord / Nostr /
/// server の各 gateway 固有の名前は属性へ吸収して定数を消したが、core は「ゲート固有の
/// 名前」ではないのでここへ残す。`BridgedExecutor` はこの 2 定数から `dispatch` を合成し、
/// gateway / MCP の属性と 1 つの索引にまとめる。
///
/// **fail-closed**: `ActionDispatcher::new()` の全アクション名がこの集合か
/// [`CORE_DISPATCHABLE_ACTIONS`] のどちらか一方に属することを
/// `core_actions_are_classified_for_dispatch`（`crates/actions/src/subtask.rs`）が
/// 検査する。新しい core アクションを登録したら、どちらかへ入れない限りテストが落ちる。
pub const CORE_INLINE_ACTIONS: &[&str] = &[
    // (1) 制御系: そのターンを終える宣言。background 化すると同ターンに効かない。
    "declare_done",
    // (3) 同ターン結果依存: 生成した内声をそのターンの応答づくりに使う。
    "generate_inner_voice",
    // (3) 同ターン結果依存: 自己評価の結果を見てそのターンの応答を直す。
    "evaluate_response",
    // (3) 同ターン結果依存: 戻り値の task_id を update/record/close で使う。
    "open_task",
    // (3) 同ターン結果依存: 編集/削除/作成の成否を確認して次の操作へ進む用法が通常
    //     （mkdir → write、edit → 失敗なら別の編集、のような同ターンの連鎖）。
    "ws_edit",
    "ws_delete",
    "ws_mkdir",
    // (4) run 内共有状態: model_override / current_purpose を書き換える。
    "select_llm",
    // (4) run 内共有状態: 以後のスキル可視性（棚）を書き換える。
    "retire_my_skill",
    "restore_my_skill",
    // (4) run 内共有状態: 以後の system prompt に効く指示文の書き込み（owner 専用）。
    "update_instructions",
    // (4) 台帳の状態: contract / progress / close が同ターンに効かないと、以後の
    //     `get_task` と食い違う（「更新したのに古い契約が見える」）。
    "update_task_contract",
    "record_task_progress",
    "close_task",
    // (5) 純粋な読み取り（即答すべきもの）。dispatch すると質問 1 つが 2 ターン
    //     2 メッセージに割れるだけ。記憶想起フローは 2 段連鎖なので特に致命的。
    "get_system_info",
    "ws_read",
    "ws_list",
    "read_skill",
    "browse_memory_index",
    "search_memory_index",
    "retrieve_memory_nodes",
    "search_my_history",
    // 記憶の単位（宣言）の読み取り 2 つ（#379）。地図/範囲読みは即答すべき純読み取りで、
    // 結果を見て次の範囲や宣言を同ターンで決める。dispatch すると 2 ターンに割れるだけ。
    "survey_my_history",
    "read_my_history",
    "get_task",
    "analyze_llm_usage",
    "recall_model_experiences",
    // (6) 情報価値の無い短時間の書き込み。dispatch には必ず resume ターン
    //     （= ユーザーへの追加メッセージ）が 1 本付くので、報告する価値が無い
    //     書き込みを background 化すると雑音が増えるだけ。
    "update_impression",
    "save_model_insight",
    // (6) タグ操作（#359 / #313 段階2）。整理ラン（段階3）の中で「topic を読む → タグを
    //     決める → 付ける/外す/統合する」という短い書き込みループを回す。結果（新設できたか /
    //     何件付け替えたか）を同ターンで見て次の操作を決めるので background 化しない。短時間の
    //     書き込みで、dispatch すると resume ターンの雑音が増えるだけ。呼び出し元は
    //     `TRUSTED_ONLY_ACTIONS` にも入れて Nostr（caller=Agent）から触らせない。
    "tag_topic",
    "untag_topic",
    "merge_tags",
    // (6) 記憶の単位（宣言）の記録 2 つ（#379）。宣言/取り消しは短時間の書き込みで、
    //     結果（宣言できたか / 取り消せたか）を同ターンで見て次の操作を決める。呼び出し元は
    //     `TRUSTED_ONLY_ACTIONS` にも入れて Nostr（caller=Agent）から触らせない。
    "record_memory_unit",
    "retract_memory_unit",
    // (6) 宣言ランの窓の希望（#394）。1 行を UPSERT するだけの短時間の書き込みで、
    //     返り値（丸めた後の実際の設定）を同ターンで見て決め直す。dispatch する意味が無い。
    "plan_next_memory_window",
    // (6) 記憶の凝縮（#411）。ユニットを俯瞰した原則を core として刻む/更新する/取り消す短時間の
    //     書き込み。結果（刻めたか / 根拠が解決できたか）を同ターンで見て次の原則を決めるので
    //     background 化しない。呼び出し元は `TRUSTED_ONLY_ACTIONS` にも入れて Nostr（caller=Agent）
    //     から触らせない（宣言道具と同じ論拠）。
    "record_memory_core",
    "update_memory_core",
    "retract_memory_core",
];

/// core アクションのうち、**意図的に dispatch を許す**もの。
///
/// 「長時間かかる」か「同ターンで結果を使わない書き込み」だけを置く（dispatch には
/// resume ターンが 1 本付くので、その 1 通に見合う仕事に限る）。
pub const CORE_DISPATCHABLE_ACTIONS: &[&str] = &[
    // 長文の書き出しは payload が大きくなりうる。書けたかどうかは完了報告で足りる。
    "ws_write",
    // 学習の書き込み: 戻り値（skill_id）を同ターンで使わない。「覚えておいて」は
    // 非ブロックで処理して完了時に報告するのが自然な依頼。
    "learn_from_experience",
    "learn_from_peer",
    "reflect_and_learn",
    // 要約の保存: 同ターンで読み戻さない。
    "summarize_and_save",
    // スキル生成（server 側の gateway ツール `create_skill` と同分類。あちらは
    // 定義で `class.dispatch == Dispatchable` を名乗る）。
    "create_my_skill",
];

/// spawn_subtask のネスト上限。
pub(super) const MAX_DEPTH: u32 = 2;

/// owner のみが可視・実行できるアクション（#45）。
pub const OWNER_ONLY_ACTIONS: &[&str] = &[
    "update_instructions",
    "update_heartbeat_instructions",
    // LLM プロバイダ設定の即時変更（ルーターのホットスワップ）。外部ユーザー由来の
    // ターン（caller=Agent）からは一覧にも出さず実行もしない。owner のみ。
    "configure_llm_provider",
    // 許可コマンド（execute_shell のホワイトリスト）の管理。実行範囲を広げるため owner のみ。
    "manage_allowed_commands",
    // Nostr 連携設定（購読リレー/フィルタ/有効化）。外部発信・アイデンティティに関わるため owner のみ。
    "configure_nostr",
    // 自分の人格/モデル/推論強度/web 検索の変更。挙動を左右するため owner のみ。
    "configure_self",
    // MCP サーバ設定の管理（外部プロセス起動・env に秘密を含みうる）。owner のみ。
    "configure_mcp_server",
    // --- ローカルのシェル実行 / ファイル操作 / 実行許可リストの自己拡張（#330） ---
    // これらは「Nostr 上での活動」や「未信頼ユーザーとの会話」とは無関係の、
    // ホスト機の制御そのものであり、最上位の権限面。caller=Agent（Nostr 受信ターン /
    // 非オーナー相手の会話ターン）へ出す理由が無い。オーナー指示は「オーナー以外の指示で
    // ローカルのファイルを見る/変えるのも駄目」なので trusted_only ではなく **owner_only**
    // に揃える（CoAgent / TrustedUser にも開けない）。
    //
    // heartbeat tick / ダッシュボード / オーナー会話は全て caller=Owner なので、自律活動と
    // オーナー操作は従来どおり通る。sub-engine（depth>=1）は spawn 元の caller を継承する
    // （`subtask.rs` の `with_caller`）ので、Owner ターンから起動した実装用サブタスクは
    // caller=Owner のまま execute_shell を使える。
    //
    // シェルの許可リスト管理は `manage_allowed_commands`（上）が既に owner_only。同じことを
    // する `add_allowed_command` / `remove_allowed_command` が分類上素通しだった是正でもある
    // （bridge policy 層に owner ゲートを設ける。server ハンドラ側の owner 検査は多層防御と
    // して残る）。
    "execute_shell",
    "ws_read",
    "ws_list",
    "ws_write",
    "ws_delete",
    "ws_edit",
    "ws_mkdir",
    "add_allowed_command",
    "remove_allowed_command",
    // 時間を待たずにハートビートを手動発火（#599）。テスト用だが「今すぐ自律ターンを起こす」
    // 操作なので、オーナー / co_agent 以外（外部ユーザー由来の caller=Agent）には出さない。
    // 発火の実行内容は時間発火と同一（別経路を作らない）。
    "run_my_heartbeat",
];

/// owner / co_agent / trusted_user のみ（素の Agent は不可）のアクション（#45）。
/// `execute_skill` は現行の gateway に実装が無い防御的エントリ（将来追加時に
/// 最初からゲートされるように残している）。
pub const TRUSTED_ONLY_ACTIONS: &[&str] = &[
    "create_skill",
    "execute_skill",
    // スキル生成（core 版）と自律学習（#351）。Nostr は誰でも話しかけられるので、会話の
    // 流れでスキルを作らせ続ければスキル棚をスパムで汚染できる。オーナー明示の要望
    // （2026-08-03「スキルを作るのもなし。スパム的に作らされる可能性あるからだめ」）で
    // caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは一覧にも出さず
    // 実行もしない。gateway 版の `create_skill`（上）と同じ棚に揃える。owner/co_agent/
    // trusted_user が自分の意思で触るターン（heartbeat tick / ダッシュボード / オーナー
    // 会話）は全て caller=Owner なので従来どおり通る。`learn_from_experience` /
    // `learn_from_peer` / `reflect_and_learn` はいずれも新スキル（または記憶）を生成する
    // 学習系で、`create_my_skill` と同じく棚へ書き込むため同じゲートに揃える。
    "create_my_skill",
    "learn_from_experience",
    "learn_from_peer",
    "reflect_and_learn",
    "read_heartbeat_instructions",
    // エージェント自身の Nostr 受信 → Discord 転記先設定（#252 段階 C）。**owner 限定に
    // はしない** — 自分の転記先を自分で決めるのがこの機能の目的で、エージェントが自分の
    // 意思で触るターン（heartbeat tick / ダッシュボード / オーナー会話）は全て
    // caller=Owner なので妨げられない。一方 caller=Agent は「未信頼の外部ユーザーと
    // 会話しているターン」なので、そこへ開けると Nostr の会話ターンで自分宛受信を
    // 任意の Discord チャンネルへ流させられる。`set_my_heartbeat`（#247/#251）と同じ扱い。
    "get_my_nostr_relay",
    "set_my_nostr_relay",
    // 自分のハートビート（自律実行）の有効化と間隔（#247）。**owner 限定にはしない** —
    // 自分の設定を自分で触れることがこの機能の目的で、エージェントが自分の意思で
    // 触るターン（heartbeat tick / ダッシュボード / オーナーとの会話）は全て
    // caller=Owner なので妨げられない。一方 caller=Agent は「未信頼の外部ユーザーと
    // 会話しているターン」を意味するので、そこへ開けると会話で自律実行を起動させられる
    // （費用と挙動に効く / #240 の「意図せず自律実行が始まる」の再来）。
    "get_my_heartbeat",
    "set_my_heartbeat",
    // 定時実行（#455）。`set_my_heartbeat` と同じ理由: **owner 限定にはしない**（自分の
    // 定時実行を自分で決めるのが目的で、本人が触るターン〔heartbeat tick / ダッシュボード /
    // オーナー会話〕は caller=Owner）。一方 caller=Agent（未信頼の外部ユーザー会話ターン）へ
    // 開けると、会話で「毎朝○時に外部出力する」を仕込ませられる（#240 の再来）ので塞ぐ。
    // 更新・削除（#477）も同じ棚: **owner 限定にはしない**（自分の巡回をやめる/間隔を変えるのが
    // 目的で、本人が触るターンは caller=Owner）。一方 caller=Agent（未信頼の外部ユーザー会話）
    // からは一覧にも出さない。ハンドラ内でも所属チェック（agent_id＋session）で多層防御する。
    "get_my_schedules",
    "set_my_schedule",
    "update_my_schedule",
    "delete_my_schedule",
    // VC 参加/退出。可視性 == 強制の対称化（#45）: 非 trusted の Agent には
    // 一覧にも出さない。ハンドラ側はさらに厳しく owner/trusted_user のみ許可
    // （co_agent は一覧に見えても実行は拒否される）。
    "join_voice_channel",
    "leave_voice_channel",
    // 本鍵（アイデンティティ）の切替。外部ユーザーが勝手に乗っ取れないよう owner/
    // trusted のみ（inbound=Agent には一覧にも出さず実行もしない）。
    "nostr_switch_identity",
    // 生成鍵の npub 一覧。nsec は返さないが、自分の鍵一覧は運用者/自分（caller=Owner の
    // ターン: heartbeat / ダッシュボード / オーナー会話）だけが見ればよい情報で、外部
    // ユーザー由来の会話ターン（caller=Agent）へ出す必要は無い。`nostr_switch_identity`
    // と対で使う管理系ツールなので同じ trusted ゲートに揃える。
    "nostr_list_keys",
    // caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）に素通しだった 9 個
    // （#356）。棚卸しで OWNER_ONLY にも TRUSTED_ONLY にも入っておらず外部ユーザー由来の
    // 会話ターンから使えていたもの。オーナー要望（2026-08-03「記憶検索はいいと思う。他の
    // ツールさえ使えなければ」）に従い 9 個すべて **trusted_only**（owner_only ではない）。
    // owner / co_agent / trusted_user が自分の意思で触るターン（heartbeat tick /
    // ダッシュボード / オーナー会話 / 信頼済みユーザー会話）は全て caller!=Agent なので
    // 従来どおり通る。#351/#353 と同じ手口＝既存の caller ゲートへの追加のみで、新しい
    // 概念・列・設定は足していない。
    //
    // 通知転送先（webhook）の設定・読み取り。一番危険なのは `set_default_*` — Nostr で
    // 話しかけた第三者にエージェントの通知先 URL を自分のサーバへ向け替えられると、以後の
    // 通知内容がそこへ流れる。読み取り側（`get_*` / `list_*`）も設定済み URL を露出する。
    // これら 6 個は `SystemGatewayActions`（server 側 own ツール / #157 S5）の実装。
    "set_default_webhook",
    "set_default_subtask_webhook",
    "get_default_webhook",
    "get_default_subtask_webhook",
    "list_webhooks",
    "list_subtask_webhooks",
    // 記憶インデックス設定の書き込み。他の `configure_*` は全て OWNER_ONLY なのにこれだけ
    // 素通しだった漏れの是正。owner_only ではなく trusted_only に揃える（#356 のオーナー
    // 決定）。`SystemGatewayActions`（server 側 own ツール / #157 S1）の実装。
    "update_memory_index_config",
    // ホスト・システム情報の露出。core inline アクション（`CORE_INLINE_ACTIONS`）。
    "get_system_info",
    // `execute_shell` の許可コマンド一覧＝ローカル構成の露出。`execute_shell` 本体は
    // OWNER_ONLY（#330）だが、その許可リストの読み取りは素通しだった。
    // `SystemGatewayActions`（server 側 own ツール / #157 S1）の実装。
    "list_allowed_commands",
    // 記憶へのタグ付け（#359 / #313 段階2）。Nostr は誰でも話しかけられるので、会話の
    // 流れで記憶にタグを付けさせ続ければタグ語彙をスパムで汚染できる（#351/#353 と同じ
    // 論拠）。整理ラン（段階3）は caller=Owner で走る（heartbeat と同じ前例）ので支障は
    // 無い。`OWNER_ONLY` ではなく **trusted_only** — owner だけでなく CoAgent /
    // TrustedUser も従来どおり使える。owner / co_agent / trusted_user が自分の意思で触る
    // ターン（heartbeat tick / ダッシュボード / オーナー会話 / 信頼済みユーザー会話）は
    // 全て caller!=Agent なので通る。いずれも core dispatcher のアクション
    // （`CORE_INLINE_ACTIONS` / `crates/actions/src/memory_access.rs`）で、既存の caller
    // ゲートへの追加のみ＝新しい概念・列・設定は足していない。
    "tag_topic",
    "untag_topic",
    "merge_tags",
    // 記憶の単位（宣言）道具 4 つ（#379 #376 段階1）。タグ道具（上）と同じ論拠で
    // **trusted_only**: Nostr（caller=Agent）は誰でも話しかけられるので、会話の流れで
    // 生ログを俯瞰させ・宣言させ続けると、記憶レイヤをスパムで汚染できる。宣言ラン
    // （段階2）は caller=Owner で走る（heartbeat と同じ前例）ので支障は無い。owner /
    // co_agent / trusted_user が自分の意思で触るターン（heartbeat tick / ダッシュボード /
    // オーナー会話 / 信頼済みユーザー会話）は全て caller!=Agent なので従来どおり通る。
    // いずれも core dispatcher のアクション（`crates/actions/src/memory_units.rs`）で、
    // 既存の caller ゲートへの追加のみ＝新しい概念・列・設定は足していない。読み取り 2 つ
    // （survey / read）は整理ラン用の `ORGANIZE_ALLOWED_TOOLS` にも入る（記録 2 つは段階2）。
    "survey_my_history",
    "read_my_history",
    "record_memory_unit",
    "retract_memory_unit",
    // 宣言ランの窓の希望（#394）。同じ論拠で trusted_only: caller=Agent（Nostr の受信ターン）
    // から触れると、話しかけるだけで他人の宣言ランの窓を動かせてしまう。宣言ラン本体は
    // caller=Owner で走るので支障は無い。
    "plan_next_memory_window",
    // 記憶の凝縮 道具 3 つ（#411）。宣言道具と同じ論拠で trusted_only: caller=Agent（Nostr の
    // 受信ターン）から触れると、会話の流れで人格の核（core）をスパムで汚染できる。凝縮ラン本体は
    // caller=Owner で走る（宣言ラン・heartbeat と同じ前例）ので支障は無い。core dispatcher の
    // アクション（`crates/actions/src/memory_units.rs`）で、既存の caller ゲートへの追加のみ。
    "record_memory_core",
    "update_memory_core",
    "retract_memory_core",
];

// `nostr_run`（薄い nostaro passthrough / #268）は**ここに入れない**（#303）。
// opencrab が Nostr 連携で担保するのは ①鍵のエージェント間混同防止 ②nsec の隠蔽 の
// 2 点だけで、①は常に当該エージェント自身の `--config` を渡す passthrough の構造が、
// ②は出力マスクが担保している。caller による露出制限はどちらにも要らない。
// caller=Agent が指すのは **Nostr 受信ターン**（`crates/nostr/src/sink.rs`）と、非オーナー
// 相手の会話ターン。ここへ入れると Nostr 受信ターンから `nostr_run` が丸ごと消えるため、
// 「Nostr 上で自律的に活動する」という目的そのものを塞ぐ。
// （heartbeat tick は caller=Owner なので元から塞がれていない。上の各コメントも同じ。）
//
// `nostr_zap` は同じ理由で**ここに入れない**（#306）。以前は `nostr_dm` と共に入っていたが、
// `nostr_run` を開けた時点で `nostr_run zap` / `nostr_run dm` が同じターンから通るように
// なり（当時の passthrough deny は `init`/`watch`/`relay` の 3 つだけ）、inner ツール名だけを
// 隠しても能力は塞げていなかった。一貫性は**制約を増やす方向ではなく減らす方向**で取る、
// というのがオーナーの決定（#306）。使うかどうかはエージェントが自分で判断する。
//
// **`nostr_dm` は #514 で別扱いになった**: DM は秘密鍵漏洩で過去に遡って全部読めるため
// 送信禁止（オーナー決定）。定義から削除し、送信のもう一方の経路 `nostr_run dm` も
// passthrough deny（`crates/nostr/src/cli.rs` の `PASSTHROUGH_DENIED_SUBCOMMANDS` に `dm`）で
// 塞いだ。#306 の「減らす方向」とは逆の追加だが、#306 は「DM か zap か」の caller ゲートの
// 話で、#514 は「DM という機能そのものを持たない」というより上位の決定なので矛盾しない。
// 上の nostr_switch_identity / nostr_list_keys は残る — こちらは①鍵の混同防止に
// 直接効き、`nostr_run` 側でも `init` が deny されていて迂回路が無い。
// nostr_zap のゲートを外した状態は `nostr_messaging_passes_the_gate_for_agent_caller` が固定する。

/// アクション名 → 権限/深度ポリシー（#45 の単一の表）。
///
/// 以前は可視性（`list_tools`）だけがこれらのリストを参照し、実行
/// （`dispatch_inner`）は depth 系しか強制していなかったため、「一覧から
/// 隠したツールをモデルが名前指定で実行できる」食い違いがあった。
/// 可視性と実行時強制は必ずこの関数を参照すること（discord 側ハンドラの
/// typed gate は多層防御としてそのまま残る）。
///
/// **名前リストで決まる 3 つだけ**を持つ（`owner_only` / `trusted_only` /
/// `depth_capped`）。sub-engine 遮断（旧 `blocked_in_subengine`）はツール定義の属性
/// （`class.sub_engine == Blocked`）が権威になったため、`BridgedExecutor` が
/// 名前 → `ToolClass` の索引から引く（この構造体には持たせない）。
pub struct ToolPolicy {
    pub owner_only: bool,
    pub trusted_only: bool,
    /// depth >= MAX_DEPTH でブロック（ネスト上限）。
    pub depth_capped: bool,
}

pub fn tool_policy(name: &str) -> ToolPolicy {
    ToolPolicy {
        owner_only: OWNER_ONLY_ACTIONS.contains(&name),
        trusted_only: TRUSTED_ONLY_ACTIONS.contains(&name),
        depth_capped: name == "spawn_subtask",
    }
}

/// Bridges `ActionDispatcher` to the `ActionExecutor` trait so that
/// `SkillEngine` can drive real actions.
///
/// Holds both the dispatcher and a pre-configured `ActionContext`.
/// Optionally holds `GatewayActions` to merge gateway-specific tools.
/// MCP ツール名の名前空間プレフィックス（`opencrab_mcp::MCP_TOOL_PREFIX` と一致させる。
/// actions は mcp に依存できない＝依存循環になるため定数で持つ）。
///
/// dispatch 分類でも使う: MCP ツールは運用者が繋いだ任意の外部ツールで、性質
/// （配送系か / 同ターンで結果を使うか）を静的に分類できないため、**既定 inline**
/// （安全側）にする（[`crate::subtask::SubtaskToolDispatcher::should_dispatch`]）。
pub const MCP_TOOL_PREFIX: &str = "mcp__";
