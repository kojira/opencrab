# 設計: webhook の長文を「プレビュー + 全文ファイル添付」で送る

> 作成日 / Created: 2026-08-01
> ステータス / Status: 実装済み（issue #293）
> 対象 / Scope: Discord webhook へ出る全経路（subtask lifecycle / activity tool events / Nostr 受信転記）
> 参照 / Refs: `docs/design-agent-tool-webhooks.md`, `docs/design-webhook-output-lossless.md`（§3.3-3 `attach` 戦略の実体化）

---

## 0. 何を変えたか

**変更前**: 長文は 1900〜2000 文字ごとに分割し、`part X/N` を付けて**複数メッセージとして連投**していた。

**変更後**: Discord の 1 通上限（2000 文字）を超える本文は、**1 通の multipart/form-data**
（`payload_json` に出だしのプレビュー、`files[0]` に全文テキスト）で送る。
2000 文字以下は**従来どおり JSON 1 本**（添付しない）。

送信回数は本文の長さに依らず **常に 1 回**になった。

### やめた理由

1. **レート制限**: N 通の POST は Discord の 429 を踏みやすい。リトライを含めて大量に飛んだ実例がある。
2. **読みづらさ**: チャンネルが同一内容の断片で埋まる。
3. **コピーしづらさ**: 断片が別メッセージなので全文を取り出せない。

ロスレス性（全文が届くこと）は失われていない。分割で運んでいた全文が、そのまま添付ファイルの中身になる。

---

## 1. 決定したパラメータ

ポリシーは全経路で共有する。実体は `crates/actions/src/webhook_target.rs`（gateway 非依存層）。

| 項目 | 値 | 根拠 |
|---|---|---|
| 添付に切り替える閾値 `ATTACHMENT_THRESHOLD_CHARS` | **2000 文字** | Discord の 1 メッセージ上限そのもの。「1 通に収まるものは素のテキスト、収まらないものだけ添付」という単純な境界にすることで、**短いメッセージの見え方・送られ方が一切変わらない**（回帰しない）。 |
| プレビュー長 `ATTACHMENT_PREVIEW_CHARS` | **600 文字** | 「数百文字」＝ Discord で 5〜10 行、スクロールせず要旨が掴める量。案内文（150 文字程度）を足しても 2000 文字上限に十分な余裕がある。webhook 設定の `max_chars` はこれを**短くする方向にだけ**効く。 |
| ファイル名 | `<slug>.txt` | `slug` は**静的な語彙のみ**（`subtask-task` / `<event>-<tool_name>` / `nostr-inbound`）。`attachment_filename()` が ASCII 英数と `-_.` 以外を潰し、48 文字で切り、空なら `output.txt` にフォールバックする。**秘密・個人情報（pubkey / ユーザ名 / URL / callId / 引数）はファイル名に載せない**（中身は添付本体に入る）。パス区切りも潰れるのでディレクトリ脱出も起きない。 |
| content_type | `text/plain; charset=utf-8` | 全文はプレーンテキスト。Discord 上でプレビュー表示される。 |
| サイズ上限 `ATTACHMENT_MAX_BYTES` | **8 MiB** | 無課金サーバのアップロード上限 25 MiB の 1/3 弱。(a) boost 状況に依らず確実に通る、(b) 1 回の multipart POST が長時間化しない。超過分は**送信前に**切り詰め、ファイル末尾に `--- [truncated] ...` を付け、本文の案内にも `truncated` を明記する（成功を装わない）。切り詰めは UTF-8 境界を壊さない。 |
| 送信タイムアウト | JSON **30 秒** / multipart **60 秒** | Discord の応答は通常 1 秒未満。8 MiB を遅い回線で送り切る余裕として 60 秒。接続が黙って死んだときに配送 worker が永久待ちに入るのを防ぐ（worker が止まるとその run の後続配送が全部止まる）。 |

---

## 2. 変更した送信経路 / 変更しなかった経路

### 添付方式に切り替えた（＝ webhook へ HTTP POST する経路すべて）

| 経路 | 場所 | ファイル名 slug |
|---|---|---|
| subtask lifecycle（started の task 本文） | `crates/discord/src/gateway_actions/webhook.rs::build_started_messages` | `subtask-task` |
| activity / tool event（args・stdout・stderr 等） | 同 `build_tool_event_message` | `<event>-<tool_name>` |
| Nostr 受信 → Discord 転記 | `crates/server/src/nostr_runner_impl.rs::spawn_relay_post` | `nostr-inbound` |

terminal / progress メッセージは元々「概要 1 通」で分割していなかったため変更なし。

### 変更しなかった（＝ webhook ではない）

- `crates/server/src/peer_review.rs`（`build_part_messages`）: **Discord webhook ではなく transport の `send_text`**。
  かつ `part X/N` の framing は「part X/N の生データを読め」というプロンプト規約とセットのプロトコルで、
  レビュアー側の LLM が読む。ここを添付にすると LLM が本文を読めなくなるため触らない。
- `crates/server/src/heartbeat_delivery.rs`（`chunk_text`）: 非 Discord transport（Nostr 等）の
  1 イベント上限に合わせた分割。ファイル添付という概念が無い transport なので触らない。
- `chunk_text` / `build_part_messages` 自体は**削除しない**。上記 2 経路が使っており、
  添付が使えない場面のフォールバックにもなる。

---

## 3. 秘密のマスクの担保

**添付の中身は、プレビューと同じ 1 本の文字列から作る。** `build_message_with_optional_attachment(text, slug)` は
`text` の先頭 N 文字をプレビューに、`text` 全体を添付バイト列にする。添付が別経路から生データを
拾い直すことは構造上ありえないので、**呼び出し前に掛かっているマスクは添付側にもそのまま効く**。

具体的には、nostr CLI の `nsec` / `secret_key` マスク（`crates/nostr/src/cli.rs::mask_secrets`）は
ツール実行の出口で掛かっており、webhook 層が見る stdout/stderr は既にマスク済みである。

なお work-channel 出力（command / stdout / stderr / args / result）に対する
`redact_secrets` は `docs/design-webhook-output-lossless.md` §2 P4 の決定により**意図的に外してある**。
本 issue はその決定を変更しない（添付は「新しい漏洩経路」を作らない、という担保のみを行う）。

テスト: `attachment_is_built_from_the_same_masked_string`（actions）、
`test_build_tool_event_message_attachment_carries_upstream_masking` / `attachment_body_keeps_upstream_masking`（discord）。

---

## 4. 非ブロック性（#284 と同根の詰まり対策）

multipart は JSON より明確に重い。ここで詰まると受信・応答・発火が止まるため、以下を不変条件とする。

1. **HTTP は必ず spawn 済みタスクの中でだけ await する。**
   - subtask / activity: `spawn_run_worker_with_sink` が起動した worker タスク内。呼び出し元は
     `tx.send(DeliveryBatch)`（unbounded・同期・即時）を叩くだけ。
   - Nostr 転記: `spawn_relay_post` の `tokio::spawn` 内。
2. **整形と添付バイト列の生成（切り詰め含む）は spawn の前**に済ませる。巨大ボディをそのまま投げない。
3. **ロックを保持したまま送らない。** 宛先解決の DB ロックはブロック内で閉じてから送信に入る。
4. **タイムアウトを必ず入れる**（§1）。
5. **失敗は握って続行**。Nostr 転記はログのみ。subtask/activity は既存の backoff retry に乗り、
   使い切ったら `on_giveup` sink へ短い説明を渡して終わる。呼び出し元は巻き込まれない。

テスト: `delivery_never_blocks_the_caller`（discord）、`relay_post_never_blocks_the_caller` /
`relay_post_failure_is_swallowed`（server）。

---

## 5. 送信失敗時の扱い

既存の `send_with_retry`（429 は `Retry-After` を尊重して attempt を進めず再送、その他は
`[0,2,10,30,120]` 秒 backoff）をそのまま使う。添付固有の追加は 1 点だけ:

> **添付付き送信が 4xx（429 を除く。413 Payload Too Large / 400 等）で弾かれたら、
> 添付を落として本文（プレビュー）だけを JSON で 1 回送り直す。**

同じボディを何度投げても通らないので retry は無意味であり、要旨がチャンネルに残るほうが
「全部消える」より良い。5xx / ネットワークエラーは従来どおり backoff retry する。
フォールバックは 1 段だけで、そこで失敗すれば通常の give-up 経路に合流する。

テスト: `rejected_attachment_falls_back_to_preview_only_json`。

---

## 6. テスト

実 Discord へは一切送らない。依存を増やさず、ローカルに最小の HTTP モック
（`TcpListener` + 手書きレスポンス）を立てて**何回・どの Content-Type で・何を送ったか**を検査する。

- 閾値超過で **1 回の multipart 送信**になり、分割連投しないこと（送信回数を検査）
- プレビューが指定長で、全文が添付本文と一致すること
- 閾値以下は**従来どおり JSON のみ**（回帰）
- 添付本文にも秘密マスクが効くこと
- サイズ上限超過時の切り詰め（UTF-8 妥当性を含む）
- ファイル名のサニタイズ
- 添付送信が呼び出し元をブロックしないこと / 失敗しても後続が進むこと
- `allowed_mentions: {parse: []}` が multipart（`payload_json`）でも維持されること（#252 の担保）
