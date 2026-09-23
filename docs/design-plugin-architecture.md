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

プラグインは再起動されうるので、差し替え可能な gateway 間で共有される永続 **汎用会話・実行状態**はコアが 1 つだけ持つ。一方、具象 platform の状態をコアへ集めてはならない。完全な所有境界と移行条件は architecture review 承認後の [design-gateway-process-ownership.md](design-gateway-process-ownership.md)（Issue #1006）を基準とし、この文書の「状態」「設定」「transport」は次の限定した意味で使う。

**コアが持つ汎用会話・実行状態**
- エージェント、subject、session、membership、会話履歴
- エージェント実行パイプラインとセッション単位の直列化
- バックグラウンド実行、subtask、完了通知
- 汎用 schedule / heartbeat（明示的に決定されたもの）、generic gate row、exactly-once ledger
- LLM / tool など gateway 非依存の core 設定と、動的 capability metadata による実行分類

**具象 gateway daemon が持つ platform 状態・責務**
- endpoint / account / application / bot ID、credential、外部 identity projection
- admission / delivery policy、subscription / filter / watch、表示整形・reaction
- platform schema、ローカル管理面、配置生成、instance child の起動・停止・再起動
- platform の送受信、認証、generic caller role への分類

「transport は送受信だけ」は「汎用会話実行を持たない」という意味であり、platform policy・storage・authentication・lifecycle まで stateless にする意味ではない。generic な外部サービス supervision utility は共有してよいが、`server` は具象 gateway daemon を spawn / supervise / configure / proxy しない。各独立 daemon が自分の instance child を監督する。

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

#191 で得た、共有 SDK への具象 SDK 漏れを防ぐ依存検査、汎用 `AgentRuntime`、単一の session lock / subtask / completion 実体、動的 operation 宣言、generic runtime UDS は有効なので保存する。一方、#1006 の現行 inventory では Nostr daemon の core SQLite 直結・runtime import、core の具象 schema/query、server の具象管理 API、server/shared lifecycle 登録簿、platform-shaped timed-fire、Discord の placement-only ownership が未解消である。Web/CLI の独立 generic-runtime client と server の dev-only Discord/Nostr QC 依存は違反ではない。

完全な AS-IS evidence、production / dev-only / dead・legacy / test / comment / historical migration / persisted history の分類、および各違反と TO-BE transition/completion criterion の 1 対 1 対応は [design-gateway-process-ownership.md §3–4](design-gateway-process-ownership.md#3-evidence-backed-as-is-violation-inventory) に集約する。過去の段階 1–2 の説明を future direction として再利用して `server` 所有へ戻してはならない。

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
  判定: gateway 間で共有する汎用会話・実行状態なら core。endpoint/account ID、credential、外部 identity、platform policy/subscription/display/lifecycle なら具象 gateway。再起動後も必要という理由だけで platform state を core へ置かない。
- その変更は 3（プロセス境界）へ進む余地を **狭めていないか**
  判定: gateway と core の呼び出しが generic gate-admin/runtime protocol の**値／メッセージ**だけか。core DB handle や上位共有状態を渡さず、`server` に具象 daemon の config/supervision を足さない。

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
