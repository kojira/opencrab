# 設計方針: コアを生かしたまま外側を差し替えられる構成

対応 issue: #191（方針）/ #155（統括）

このドキュメントは**個々のリファクタがどこへ向かっているか**の基準を定める。実装手順書ではなく、設計判断の拠り所として使う。

---

## 1. 目指す姿

**コアは生きたまま、外側（transport・拡張）を落とさずに差し替えられる**構成にする。

最終的な目的は **エージェントが自分自身で opencrab を開発できるようにすること**。

単一バイナリのままだと「自分が動いているコードを差し替える」＝「自分を落とす」ことになり、自己開発が成立しない。実際に、開発中のサーバ再起動によって会話中の受信メッセージが失われる事象が起きている（Discord のゲートウェイは接続中のイベントしか配信しないため、停止していた間の分は後から流れてこない）。

再接続時の取りこぼし補完は、再開点の永続化・DM の列挙・生イベントとの順序競合・古い分への誤返信といった問題を抱え、得られるものに対して実装量が過大なので**実装しない**。したがって答えは「**再起動そのものを減らせる構成にする**」になる。

## 2. 手段の選択

| 方式 | 落とさず差し替え | 判定 |
|---|---|---|
| 動的ライブラリの読み込み | 理論上可 | **採らない**。`repr(Rust)` に安定 ABI が無く、ジェネリクスは単型化されるので境界を越えられない。コンパイラ・feature・依存バージョンの一致も必要。C 互換 ABI と自前の vtable（`abi_stable` 等）を使えば trait 相当も越えられるが、その代償として**インターフェースを C 互換に制限し、vtable を維持し、panic とアロケータを境界の内側に閉じ込め続ける**必要がある。得より苦労が大きい |
| **別プロセス + プロトコル** | **可** | **採る。この repo で既に実証済み**（外部ツール連携は stdio の子プロセスとして動き、設定変更時の張り替えと、死活検出＋自己修復の再接続を持つ） |
| ビルド時の切り替え（feature） | 不可（再ビルドが必要） | 併用する。取り外し可能であることの担保として有効 |
| トレイト + レジストリ（同一プロセス） | 不可 | 併用する。**プロセス境界へ進む前段として必須** |

外部ツール連携（MCP）の protocol・死活検出・再接続は generic utility の参照になる。ただし具象 gateway は `server` の子ではなく独立 daemon であり、その daemon が自分の instance child を起動・張り替え・監督する。

## 3. コアが持つもの / 外に出すもの

ここで言う「**コア**」は特定のクレート名ではなく **gateway 非依存層**を指す（`core` / `db` / `actions`）。状態の実体を置く先は主に `actions`。`core` は下位層のため `actions` に依存できない（依存の向きは `DESIGN.md` §2.2 を参照）ので、「コアに置く」を `crates/core` と読み替えないこと。

プラグインは再起動されうるので、差し替え可能な gateway 間で共有される永続 **汎用会話・実行状態**はコアが 1 つだけ持つ。一方、具象 platform の状態をコアへ集めてはならない。承認済みの完全な所有境界と移行条件は [design-gateway-process-ownership.md](design-gateway-process-ownership.md)（Issue #1006）を基準とし、実装順序と gate は同文書の [§13 staged TDD plan](design-gateway-process-ownership.md#13-staged-assertion-level-tdd-execution-plan) に従う。この文書の「状態」「設定」「transport」は次の限定した意味で使う。

**コアが持つ汎用会話・実行状態**
- エージェント、subject、session、membership、会話履歴
- エージェント実行パイプラインとセッション単位の直列化
- バックグラウンド実行、subtask、完了通知
- 汎用 schedule / heartbeat（明示的に決定されたもの）、generic gate row、inbound dedup と pending-delivery/ack ledger
- 現行の正の INTEGER `subject_id`、単調 allocator/high-water、永続 tombstone、hashed single-use association grant
- LLM / tool など gateway 非依存の core 設定と、動的 capability metadata による実行分類

**具象 gateway daemon が持つ platform 状態・責務**
- endpoint / account / application / bot ID、credential、外部 identity projection
- admission / delivery policy、subscription / filter / watch、表示整形・reaction
- platform schema、ローカル管理面、配置生成、instance child の起動・停止・再起動
- `(binding_id, delivery_id)` ごとの durable external-emission ledger、immutable guarantee、prepared request、adapter-protocol/capability digest、external reference
- platform の送受信、認証、generic caller role への分類

「transport は送受信だけ」は「汎用会話実行を持たない」という意味であり、platform policy・storage・authentication・lifecycle まで stateless にする意味ではない。generic な外部サービス supervision utility は共有してよいが、`server` は具象 gateway daemon を spawn / supervise / configure / proxy しない。各独立 daemon が自分の instance child を監督する。

外部送信は core の pending/ack ledger と gateway の emission ledger を組み合わせ、両方が immutable `delivery_guarantee` を保存する。gateway row は prepared request と、それを reconcile できる adapter-protocol/capability digest も保存する。core 内の work/ack replay と inbound dedup は exactly-once だが、外部 platform の保証を一律に exactly-once と呼ばない。hello は実際の `delivery_guarantee` を digest-covered capability として宣言し、新規 delivery row は non-legacy guarantee を必須とする。upgrade 前の terminal row は `legacy_unqualified` のまま replay/relabel せず、送信開始前と証明できる pending だけが初回 post-cutover send の直前に live guarantee を取得する。Nostr は事前に永続化した同一 signed event ID の再 publish により logical `exactly_once` を宣言できる。Discord は永続化した 25 文字以下の nonce と `enforce_nonce` を有効期間内だけ用い、曖昧性が期限を越えたら再送せず durable `indeterminate` にするため `at_most_once_indeterminate` である。utterance declaration が `exactly_once` を要求して hello capability が弱ければ hello を拒否する。declaration が弱い保証を許しても invocation envelope が independently `exactly_once` へ引き上げた場合は実行・送信前にその invocation を拒否し、暗黙 downgrade しない。

reconnect 時も row が権威である。terminal outcome は後の hello に関係なく外部 I/O なしで報告できる。unsent row は現在 capability が保存済み保証を満たす時だけ送信し、prepared row は保存済み保証と prepared primitive/protocol digest を認識・証明できる時だけ reconcile/retry する。downgrade または protocol mismatch は再送せず durable `operator_blocked`/`indeterminate` とし、強い adapter が弱い row を扱っても保証を relabel しない。

`co_agent` の権限は caller snapshot だけでは継続しない。core は initial model turn、queue の dequeue/retry、各 tool invocation、各 continuation（automatic / operation-driven / timed / subtask を含む）と outbound-delivery commit の直前に現在の relationship/revision を再検証し、revocation/mismatch なら以後の model/tool 実行も外部送信もなく pending work を generic に終了する。

external identity は `trusted_users.platform/source` の既知値だけでなく `rest`、`extgate`、Web、任意 opaque 値を含む全 row を canonical source-row fingerprint で disposition する。Discord/Nostr は導出可能な store、genuine REST/API は `api_principals`、その他は operator が fingerprint-bound に 1 個以上の gateway instance store または `api_principals` へ明示 mapping する。global `extgate` fan-out も edge ごとの確認が必要で、Web edge があれば Web-owned durable store/local admin と `--web-db` が必須である。snapshot/rollback は core と manifest 内の全 participating gateway DB を対象にし、guess/drop/silent duplicate/unmapped row を許さない。

core gate-admin の 6 operation は permissive-CORS public router から完全に外し、nonempty scoped bearer principal と mode `0600` を受理前に検証する専用 core UDS だけで提供する。public TCP は全 6 path で 404、empty-token production path は禁止し、これは各 gateway-local concrete admin UDS とも別 socket である。

`subject_id` は UUID 化せず、現行の opaque な正の INTEGER と既存 gate association を byte-for-byte 保存する。generic `subject_id_allocator`、`subject_tombstones`、`subject_association_grants`（単調 high-water、hard delete 前の永続 tombstone、短命・hashed・single-use grant）は gateway 種別に依存しない core mechanism である。既存 association は grant 不要、新規 first association のみ generic operator administration が発行した grant を `PUT instance` で atomic consume する。

目標の依存・所有図は 3 文書で共通である：

```text
operator -- concrete local admin --> gateway daemon --> gateway-owned store/secrets
                                      |       |
                                      |       +-- supervises --> gateway instance child
                                      |                            |
                                      +-- generic gate-admin UDS   +-- generic runtime UDS
                                                   |                            |
                                                   v                            v
                                              core gate admin <---------- core runtime
                                                   |                            |
                                                   +------ core conversation store ---+

server ---------------- generic core APIs only --------------------------> core
       (never configures, spawns, or supervises a concrete gateway daemon)
```

## 4. 現状との差分

#191 で得た、共有 SDK への具象 SDK 漏れを防ぐ依存検査、汎用 `AgentRuntime`、単一の session lock / subtask / completion 実体、動的 operation 宣言、generic runtime UDS は有効なので保存する。一方、#1006 の現行 inventory では Nostr daemon の core SQLite 直結・runtime import、core の具象 schema/query、server の具象管理 API、public router への generic gate-admin merge、server/shared lifecycle 登録簿、platform-shaped timed-fire、Discord の placement-only ownership、Web-owned identity migration destination の欠如が未解消である。Web/CLI の独立 generic-runtime client と server の dev-only Discord/Nostr QC 依存自体は違反ではない。

完全な AS-IS evidence、production / dev-only / dead・legacy / test / comment / historical migration / persisted history の分類、および各 V01–V16 と TO-BE transition/completion criterion の 1 対 1 対応は [design-gateway-process-ownership.md §3–4](design-gateway-process-ownership.md#3-evidence-backed-as-is-violation-inventory) に集約する。core を直接 read-write できる stopped/offline phase は正確に 2 つだけである。(1) QC 前の単一 `project-core-state` は retained generic state を create/backfill/project できる sole generic projection transaction、(2) post-QC external freeze manifest と read-only `verify-freeze` 後の guarded destructive-cleanup transaction は全 check を再実行して legacy concrete source state だけを delete/drop し、generic state を保存し、別の cleanup/applied record に freeze/manifest ID を同じ core transaction で記録する。projection marker は first-phase commit から immutable で、startup、`verify-freeze`、cleanup のいずれも変更しない。stopped lost-response retry は core を read-only で開き、operation/request identity・version・immutable digests・initial fingerprint の完全一致だけを no-op `already_applied` とし、不一致は fail closed する。cleanup は全 gateway DB の immutable handle/read lock を commit/rollback まで保持し、preflight 後の mutation は deletion 前に abort する。runtime/daemon direct write と第 3 phase は禁止し、live write は gate-admin のみである。initial projection fingerprint は stopped retry 専用であり、post-start の `last_fired_at` 等は immutable lineage equality から除外する。過去の段階 1–2 の説明を future direction として再利用して `server` 所有へ戻してはならない。

## 5. 進む順序

この順でしか進めない。

1. **汎用の実体を transport から引き剥がし、下位層に 1 つへ寄せる**
   セッション直列化 / 登録簿 / 完了通知 / 汎用管理ツール群。
2. **上位から各ゲートウェイの名指しと in-process lifecycle registry を消す**
   上位が使えるのは、generic runtime の capability / liveness / binding registry または opaque な gate-admin/runtime protocol endpoint だけとする。#191 の `AgentGatewayLifecycle` / `AgentGatewayRegistry` を具象 gateway の集合として再利用してはならない。新しい gateway を足しても core/shared/server の registry 型・登録コード・起動順に手が入らない状態にする。
3. **境界をプロセス境界に置き換える**
   1〜2 で汎用会話実行を core に 1 つへ寄せた後、具象 gateway daemon を `server` から独立配備する。daemon は gateway-owned store と local admin を持ち、自分の instance child だけを監督する。core とは generic gate-admin UDS と runtime UDS だけで通信する。MCP の supervision 実装は generic utility の参考にはなるが、`server` が具象 gateway を子として所有する形は採らない。

   **1 を飛ばすと何が壊れるか（具体例）**: セッション単位の直列化ロックが transport 側に残ったままプロセスを切ると、1 つのセッションに対してロックが 2 プロセスに分かれる。「同一セッションの応答生成は同時に 1 本」という不変条件が破れ、同じ会話履歴から 2 本の応答が生成されて**二重投稿**になる（Nostr で実際に一度踏んだ失敗と同じ形）。
4. **自己開発の足場**
   エージェントが transport を再ビルドして張り替えても、コア（＝自分自身）は落ちない状態にする。

## 6. 判断基準（レビュー時にこれを使う）

- 新しい機能を **transport のクレートに置こうとしていないか**
  判定: 「**他の transport でも同じ意味で必要になるか**」— Yes なら汎用。下位層へ置く。
- 上位が **新しいゲートウェイを名指しで知る**変更になっていないか
  判定: 上位の状態に「そのゲートウェイ専用のフィールド」が増えていないか。
- **状態の owner を data class で決めたか**
  判定: gateway 間で共有する汎用会話・実行状態なら core。endpoint/account ID、credential、外部 identity、platform policy/subscription/display/lifecycle と external-emission ledger なら具象 gateway。再起動後も必要という理由だけで platform state を core へ置かない。
- **保証を platform capability より強く書いていないか**
  判定: core/gateway 両 row の immutable guarantee と prepared protocol digest を比較し、downgrade/upgrade/mismatch で relabel/resend しないか。core work/ack と inbound dedup、Nostr logical event identity、Discord bounded nonce/`indeterminate` を区別し、unsent/prepared/terminal の reconnect test があるか。
- **identity と admin socket を lossless/fail-closed に分離したか**
  判定: 全 source fingerprint に明示 disposition/destination proof があり、Web を含む participating store 全体を snapshot/rollback するか。public TCP は gate-admin 全 6 path が 404 で、protected core UDS と gateway-local UDS が別か。
- **revocation と subject non-reuse を全境界で守るか**
  判定: co-agent は各 model/tool/queue/continuation の直前に再検証するか。現行 INTEGER subject ID を書換えず、tombstone-before-delete と first-association grant を generic に強制するか。
- その変更は 3（プロセス境界）へ進む余地を **狭めていないか**
  判定: gateway と core の呼び出しが generic gate-admin/runtime protocol の**値／メッセージ**だけか。core DB handle や上位共有状態を渡さず、`server` に具象 daemon の config/supervision を足さない。新 gateway の追加で core source/schema/migration/redeploy が一切変わらないか。

## 7. 非目標

- 動的ライブラリによるプラグイン機構
- 再接続時の取りこぼし補完
- 1〜2 を飛ばしたプロセス分離（状態が分裂する）

## 8. 関連

- [Gateway process and storage ownership](design-gateway-process-ownership.md) — Issue #1006 の data-class owner、独立 daemon、移行・完了条件。この文書の broad rule を上記の語彙へ限定する。

- #191 方針（この文書の対応 issue）
- #155 統括: transport が実質ランタイム化している
- #190 web ゲートウェイの独立クレート化
- #157 汎用管理ツール群の移設 / #156 実行パイプラインの脱 transport / #158 / #159
- 非ブロック実行の分類基準は [DESIGN.md](DESIGN.md) §4.4 を参照
