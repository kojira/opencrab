# 設計: Webhook 出力のロスレス化 / Design: Lossless Webhook Output

> 作成日 / Created: 2026-06-05
> ステータス / Status: Draft（設計のみ・実装未着手）
> 対象 / Scope: opencrab Discord webhook 出力経路（agent / tool / session work-channel 通知）
> 参照 / Refs: `docs/design-agent-tool-webhooks.md`, `docs/design-discord-file-attachment.md`, `docs/discord.md`
> 注: 本ドキュメントは **設計提案のみ**。コード変更は含まない。
> 追記（2026-08-01 / #293）: 本書 §3.3 の 3 つの戦略のうち **file attachment** を実装済み。
> 長文（Discord の 1 通上限 2000 文字超）は chunk 投稿（§3.3-2）ではなく
> 「出だしのプレビュー + 全文ファイル添付」1 通で送るようになった。
> 決定パラメータは `docs/design-webhook-long-text-attachment.md` を参照。
> 本書中の「chunk 投稿 / `part X/N` で送る」という現状記述はその範囲で古い。

---

## 0. 要約 / TL;DR

現在、Discord webhook へ送られる tool / shell / subtask の出力は、**少なくとも 2 層**で
silently に欠落しうる：

1. **ツール発生源（execute_shell）**: 出力が `max_output_bytes`（既定 64 KiB）を超えると
   **先頭だけ残して末尾を捨てる head-only 切り捨て**が行われる。`truncated=true` は立つが、
   失われた末尾（多くの場合 *エラー本文や最終結果* が居る場所）は復元不能。これは webhook 以前に
   発生し、**モデル自身が見る出力にも影響する**。
2. **Webhook 整形層**: stdout/stderr は先頭 600 + 末尾 600 文字へ `head_tail_clamp` され
   **中央が落ちる**。terminal / progress メッセージは `truncate_chars` で **先頭のみ残して末尾切り捨て**。
   tool event メッセージは最後に 1500/2000 文字でハードクランプし末尾を `…` で潰す。tool event は
   1 run あたり 200 件で打ち切り、以降は黙って捨てる。

**【新要件】work-channel の webhook 出力は、command text / stdout / stderr / tool 引数 /
tool result preview / full-output artifact を redact してはならない。** 従来 `redact_secrets` /
`redact_secrets_line` / `redact_secret_token` が配送直前に全テキストへ `[REDACTED]` を被せていたが、
これは **work-channel 出力経路では廃止**する。ユーザーが見る work-channel のコマンド／出力内容は
**そのまま（unredacted）** 届けることを要件とする。

**本設計が扱う経路には redaction/masking の例外を一切設けない。** 従来は内部ログ（サーバーログ・
診断ログ）が **webhook 配送 URL 自体のトークン**（`redact_webhook_url`）を伏せることを許容していたが、
**この例外は廃止する**。webhook 配送 URL のトークンは、本設計が対象とする
debug / output / log 経路においても **マスクせずそのまま保持**する。理由は、URL がマスクされると
配送先の特定や障害切り分けが困難になり**デバッグが著しく難しくなる**こと、および万一 webhook が
漏洩・侵害されても **当該 webhook を即座に無効化（再生成）できる**ため、URL の機密性よりも
デバッグ可能性を優先できることである。

このトレードオフは明示的に受け入れる: raw な command / output / 配送 URL には secret や有効な
webhook URL が混入しうるため、**work-channel および本設計の covered 経路は「機密チャネル」として
運用上扱う**（§2 P4・§6 AC4 参照）。redaction による事故防止は
**チャネルそのものを sensitive 扱いすること**、および **漏洩時は webhook を無効化すること**で代替し、
出力・ログのロスレス性／デバッグ可能性を優先する。

本設計は、(a) 重要な出力を silently に切り捨てない、(b) 中央スライスや末尾切り捨てで
ログを失わない、(c) Discord のサイズ上限を **chunk 投稿 / ファイル添付 / 永続ログ参照** で
ロスレスに扱う、(d) **covered 経路（work-channel 出力・debug・log）では secret redaction も
webhook URL masking も行わず**、偶発欠落（切り捨て）だけを
明示マーカーで扱う、(e) 上流（ツール / モデル）由来の
欠落を「完全な出力」と偽らず明示する、(f) 完了サマリは簡潔のまま **必ず full output への参照を持つ**、
ことを目標とする。

---

## 1. 現状アーキテクチャの調査結果 / Current State

調査はすべて main ブランチのコードを直接読んで取得（推測ではない）。

### 1.1 出力切り捨て箇所の一覧（エビデンス付き）

| # | 箇所 | file:line | 種別 | 失われるもの |
|---|------|-----------|------|--------------|
| L0 | `truncate_bytes` (execute_shell) | `crates/actions/src/tools/shell.rs:190-196` | **head-only**（`bytes[..max]`、既定 65536） | **末尾を完全消失**。`truncated` フラグのみ。webhook 以前・モデルにも影響 |
| L1 | `head_tail_clamp` | `crates/discord/src/gateway_actions/webhook.rs:419-428` | head+tail（先頭/末尾 600、中央省略表示） | **中央**を消失（`…[N chars omitted]…` 表示あり） |
| L2 | `truncate_chars`（terminal） | `webhook.rs:175, 199-204` | head-only | terminal `result`/`error` の**末尾**（マーカー無し） |
| L3 | `truncate_chars`（progress） | `webhook.rs:192` | head-only | progress message の**末尾**（マーカー無し） |
| L4 | tool event 最終クランプ | `webhook.rs:490-495` | head-only（末尾を `…`） | tool event メッセージ末尾。既定 1500 / ハード 2000 |
| L5 | `clamp_with_ellipsis`（args） | `subtask_engine.rs:1062-1071`（`ARGS_SUMMARY_LIMIT=300`） | head-only（末尾 `…`） | tool 引数の末尾 |
| L6 | `short_json_preview` | `subtask_engine.rs:1074-1075`（400 固定） | head-only | 非 shell tool result JSON の末尾 |
| L7 | assistant content preview | `subtask_engine.rs:722`（500 固定） | head-only | progress 用本文の末尾 |
| L8 | shell result preview（progress） | `subtask_engine.rs:337`（500 固定） | head-only | progress 用 shell 出力の末尾 |
| L9 | tool event 件数 cap | `subtask_engine.rs:957-971`（`cap=200`） | **件数打ち切り** | 201 件目以降の tool event を黙って破棄（1 件の抑止通知のみ） |

> 行番号は調査時点のもの。実装着手時に再確認すること。

### 1.2 ロスレスに動いている箇所（参考・流用候補）

- `chunk_text` (`webhook.rs:108-117`) + `build_started_messages` (`webhook.rs:123-139`):
  subtask の **raw task text** を 1900 文字 chunk に分割し `part X/N` を付けて**全文**を順次送る。
  ファイル冒頭コメント（`webhook.rs:8`）に「raw task text はそのまま送る（要約も redact もしない）」と明記。
  → **出力側にこのロスレス chunk 機構を流用できる**。
- 配送は run 単位の単一 mpsc + 1 worker で**順序保証**（`webhook.rs:206-`）。chunk 投稿の順序は守れる。
- 既存設計 `docs/design-discord-file-attachment.md` に `discord_send_file` gateway action
  （serenity `CreateAttachment::path()`、最大 25 MiB、workspace 制限）が設計済み。
  → **full output のファイル添付**に流用できる。

### 1.3 Redaction（現状）と本設計での扱い

- `redact_secrets` / `redact_secrets_line` / `redact_secret_token` (`webhook.rs:307-380`):
  `sk-`/`ghp_`/`xoxb-`/`AKIA` プレフィックス、`KEY=VALUE`（KEY に TOKEN/SECRET/PASSWORD/KEY/API）、
  Bearer、長い base64/hex を `[REDACTED]` 化。**現状は配送直前に全テキストへ通している**。冪等。
- `redact_webhook_url` (`webhook.rs:290-295`): webhook URL 末尾トークンを `[redacted]` 化。

**本設計での方針（新要件）:**

- `redact_secrets*` を **work-channel 出力経路（command text / stdout / stderr / tool 引数 /
  tool result preview / full-output artifact）から外す**。これらは unredacted で配送する。
  従来「意図的なシークレットマスキング」と位置づけていたが、本要件では
  **出力のロスレス性が secret マスキングより優先**される（§0 トレードオフ参照）。
- `redact_webhook_url` の **例外的存続は廃止する**。本設計が対象とする
  debug / output / log 経路（work-channel 出力に加え、サーバーログ・診断ログを含む covered paths）では、
  **webhook 配送 URL のトークンもマスクせずそのまま保持**する。URL がマスクされると配送先の特定・
  障害切り分けが困難になりデバッグ性を損なうため、また漏洩しても当該 webhook を即座に無効化できるため、
  URL の機密性よりデバッグ可能性を優先する（§0・§2 P4 のトレードオフ参照）。
- 結果として、covered 経路の出力・ログに現れる固定マーカーは redaction/masking 由来の
  `[REDACTED]` / `[redacted]` ではなく、偶発欠落（切り捨て）を示す `[truncated …]` のみとなる（§2 P4）。

### 1.4 Discord サイズ上限の扱い

- 定数: `DISCORD_CHUNK_LIMIT=1900`, `DISCORD_MESSAGE_LIMIT=2000`（`webhook.rs:20-21`）。
- task text は chunk 投稿でロスレス。**しかし tool/shell の出力は chunk されず、単発メッセージで切り捨て**。
- 完了/terminal サマリ（`build_terminal_message`）は「概要のみ・chunk しない」と明記（`webhook.rs:166`）。
  → **full output への参照（リンク/ファイル/ログ ID）が存在しない**。

### 1.5 問題の本質（結論）

1. **L0 が最も深刻**: 発生源で末尾を捨てるため、webhook 層で何をしても末尾は復元できない。
   しかも `truncated=true` という 1 ビットしか欠落情報が無く、「何バイト失ったか」「どこを失ったか」が不明。
2. **L1〜L8 は webhook 層のヒューリスティック切り捨て**で、中央/末尾を失う。マーカーの有無もまちまち。
3. **L9 は件数の silent drop**。
4. **full output へのポインタが存在しない**ため、ユーザーは「これが全部」と誤認する。

---

## 2. 設計方針 / Design Principles

P1. **発生源で全文を保持する（または明示的に量を記録する）。** ツール出力は webhook 整形の前段で
    全文（または上限つきだが「捨てた量・場所」を構造化した形）を保持する。webhook 用の短縮は
    あくまで**表示用 view**であり、full source は別に残す。

P2. **arbitrary な中央/末尾スライスを「最終的な真実」にしない。** 短縮はプレビュー目的に限定し、
    full output は必ず別経路（chunk 投稿 / ファイル添付 / 永続ログ参照）で到達可能にする。

P3. **Discord 上限はロスレス戦略で吸収する。** 既存の `chunk_text` 機構と `discord_send_file`
    機構を出力側に展開する。どちらも使えない/無効な場合は、**永続ログ参照（ID/URL）を必ず提示**する。

P4. **covered 経路（work-channel 出力・debug・log）では一切 redaction/masking を行わない。**
    command text / stdout / stderr / tool 引数 / tool result preview / full-output artifact、
    および **webhook 配送 URL のトークン** は、本設計が対象とする出力・debug・log 経路すべてで
    **unredacted / unmasked** で保持・配送する。したがって covered 経路に現れるマーカーは、
    偶発的な上限到達を示す `[truncated: N bytes/chars omitted — see <ref>]` **のみ**であり、
    `[REDACTED]` / `[redacted]` 等のマスキングマーカーは一切出さない。
    - **トレードオフ（明示）**: raw な command/output には secret（API キー・トークン・パスワード等）が、
      また debug/log には **有効な webhook 配送 URL（`/api/webhooks/` を含む）** が、そのまま混入しうる。
      これらを redact/mask しないため、**covered 経路は機密チャネルとして扱う**
      （アクセス権限・閲覧者・保持期間を sensitive 前提で運用する）。
    - **webhook URL がそのまま残ることの受容**: raw な webhook URL は漏洩しうる。これを承知のうえで
      受け入れる理由は、(1) URL がマスクされると配送先の特定・障害切り分けが困難になり
      **デバッグ可能性を優先**するため、(2) 万一 webhook が漏洩・侵害されても
      **当該 webhook を即座に無効化（再生成）できる**ため、URL の機密性は無効化によって回復可能だからである。
      出力・ログのロスレス性／デバッグ可能性を、チャネル単位の機密管理と webhook 無効化で担保する設計上の選択である。

P5. **上流由来の欠落は「完全」と偽らない。** L0 の `truncated` のように上流（ツール / モデル / 外部 API）が
    既に切り捨てている場合、webhook は **「これは部分出力である」と明示**し、可能なら欠落量と
    full source への参照を併記する。検出できない欠落は「検出できなかった」と書く（沈黙しない）。

P6. **完了/サマリ webhook は簡潔のまま、必ず full output を指す。** terminal/summary は短く保つが、
    **full output 参照（添付ファイル名 / ログ ID / ダッシュボード URL）を必須フィールド**にする。

P7. **silent な打ち切りを禁止する。** 件数 cap（L9）や上限到達は、**必ず可視のメッセージ**で
    「N 件 / N バイトを表示しなかった、full は <ref>」と告知する。

---

## 3. 提案アーキテクチャ / Proposed Design

### 3.1 出力の二層モデル（Source-of-truth と View の分離）

```
[ツール実行] ──► RawOutput（full、または「上限つき＋欠落メタ」）
                    │
                    ├─(a) 永続化: OutputArtifact（DB or ファイル）──► ref_id / file path
                    │
                    └─(b) 表示用 view: WebhookOutputView
                           ├─ preview（短縮のみ・**unredacted**）
                           ├─ loss_info（欠落の種別・量・位置）
                           └─ full_ref（(a) への参照）
                    │
                    ▼
              配送 strategy（下記 3.3）で Discord へ
```

- **OutputArtifact**: 1 ツール実行（または 1 subtask 完了）の full output を保持する永続レコード。
  最低限 `id`, `run_id`, `tool_call_id`, `kind`(stdout/stderr/result), `full_text`(or path),
  `byte_len`, `created_at`, `source_truncated`(上流 L0 由来か), `source_truncated_bytes` を持つ。
- **WebhookOutputView**: Discord 表示用。preview は **プレビューと明示**し、必ず `full_ref` を持つ。
  preview / full_ref / artifact のいずれも **secret redaction も webhook URL masking も適用しない**（§2 P4）。
  短縮はあくまでサイズ削減（length clamp）のみで、`[REDACTED]` / `[redacted]` 置換は行わない。

### 3.2 欠落の構造化 / LossInfo

`truncated: bool` を、以下の構造化情報へ置き換える（概念スキーマ。実装時に Rust 型へ）:

```
LossInfo {
  reason:   "source_limit" | "discord_size" | "event_cap" | "none",
  scope:    "head" | "tail" | "middle" | "count",
  omitted:  u64,            // 失った byte / char / 件数
  unit:     "bytes" | "chars" | "events",
  detectable: bool,         // 量を正確に測れたか（測れない場合 false）
  full_ref: Option<Ref>,    // full source への参照（無ければ None と明示）
}
```

- `reason=source_limit` は L0（上流ツール由来）。P5 に従い「部分出力」と表示。
- `reason=none` かつ `full_ref` 有り＝「Discord 上は短縮だが full は参照可能」。
- `detectable=false`（例: モデル/外部 API が黙って削った可能性）は
  **「欠落を検出できなかった可能性あり」と正直に表示**（沈黙しない）。

### 3.3 配送 strategy（Discord 上限のロスレス吸収）

出力サイズと設定に応じて、以下を優先順位で選ぶ。**どれを選んでも full output が到達可能**であること。

1. **inline preview のみ**（出力が 1 メッセージに収まる）: そのまま 1 通。`full_ref` 不要。
2. **chunked posting**（既存 `chunk_text` を出力へ流用）: full を 1900 文字 chunk で `part X/N` 送信。
   tool 出力にも `build_started_messages` 相当の chunk 機構を展開。順序は既存 mpsc worker で保証。
   - チャネルの noise を避けたい場合は、`mode` 設定（下記 3.4）で chunk を抑制し 4/5 にフォールバック可。
3. **file attachment**（`discord_send_file` 流用）: full output を `run_<id>_<tool>.log` 等として添付。
   25 MiB 上限を超える場合は 5 へ。
4. **persisted log reference**: OutputArtifact の ID / ダッシュボード URL（例
   `http://localhost:3000/...` or LLM log UI、`docs/design-llm-log-ui.md` 参照）をメッセージに埋める。
5. **常に**: 1〜4 のいずれでも、**preview メッセージに `LossInfo` と `full_ref` を併記**する。

> どの戦略も使えない異常時（永続化失敗・添付失敗）は、**フォールバックで成功を装わず**、
> 「full output を保存/添付できなかった」とエラーを可視化する（CLAUDE.md「失敗を隠さない」に準拠）。

### 3.4 設定 / mode

`docs/design-agent-tool-webhooks.md` の `summary` / `full` / `off` 概念を拡張:

- `summary`（既定）: preview 1 通 + **full_ref 必須**（file or log ref）。中央/末尾の silent loss を禁止。
- `full`: preview + full を chunk 投稿（3.3-2）。
- `attach`: preview + file 添付（3.3-3）。
- `off`: webhook 自体を送らない（出力欠落の概念対象外）。

`max_chars` / cap は **preview の上限**としてのみ意味を持ち、**full の喪失を意味しない**よう再定義。

### 3.5 L0（発生源 head-only 切り捨て）への対応

ここは webhook 層では直せない。設計上の選択肢を提示（実装方針は実装フェーズで決定）:

- **(A) full を別保持**: execute_shell が `max_output_bytes` で切る前に full を OutputArtifact へ
  退避（または stdout/stderr をファイルへスピル）し、`source_truncated_bytes` を正確に記録。
  webhook はこの artifact を `full_ref` に使う。
- **(B) head-only を head+tail へ**: 少なくとも末尾（エラー/結果が居がち）を残す `head_tail` 化。
  ただしこれは緩和策であり middle loss は残る。(A) と併用が望ましい。
- **(C) 量の明示**: 最低限、`truncated: bool` を `omitted_bytes: u64` へ拡張し
  「何バイト失ったか」を必ず伝える（P5）。

> 推奨: (A)+(C)。(B) は (A) が間に合わない場合の暫定。

---

## 4. 変更が見込まれるファイル・関数 / Likely Changes

設計のみ。以下は実装フェーズでの**変更候補**であり、本 PR では触らない。

| 領域 | ファイル | 関数 / 箇所 | 変更内容（案） |
|------|----------|-------------|----------------|
| 発生源 | `crates/actions/src/tools/shell.rs` | `truncate_bytes:190`, `call:174-186` | full 退避 or head+tail 化、`omitted_bytes` 記録（3.5） |
| 発生源設定 | `crates/actions/src/tools/config.rs` | `max_output_bytes:48-49,94-96` | full spill 設定・閾値の追加 |
| webhook 整形 | `crates/discord/src/gateway_actions/webhook.rs` | `head_tail_clamp:419`, `truncate_chars:199`, `build_terminal_message:144`, `build_progress_message:183`, `build_tool_event_message:454`, `summarize_shell_result:397` | preview/view 化、`LossInfo`+`full_ref` 付与、chunk 機構の出力展開 |
| webhook 整形 | `webhook.rs` | `chunk_text:108`, `build_started_messages:123` | 出力 chunk への再利用（抽象化） |
| tool event sink | `crates/discord/src/gateway_actions/subtask_engine.rs` | `WebhookToolEventSink::on_event:957-971`(cap), `summarize_tool_args:1050-1071`, `short_json_preview:1074`, `722`, `337` | cap の可視化、preview 化、full_ref 付与 |
| 配送 | `webhook.rs` | mpsc worker `206-`, `DeliveryBatch:102` | file 添付 / log ref バッチ種別の追加 |
| ファイル添付 | （新規 or 既存）`discord_ops.rs` ほか | `discord_send_file`（`docs/design-discord-file-attachment.md`） | full output 添付の実体 |
| 永続化 | `crates/db/src/{schema.rs,queries.rs}` | 新規 `output_artifacts` 相当 | OutputArtifact 保存・参照 |
| 型 | `crates/core/src/engine/types.rs` / `subtask_webhook.rs` | tool event / result 型 | `truncated:bool`→`LossInfo`、`full_ref` フィールド |
| redaction（**変更**） | `webhook.rs:307-380` | `redact_secrets*` | **covered 経路（work-channel 出力・debug・log）の呼び出しを除去**。command/stdout/stderr/args/preview/artifact に適用しない（P4）。view 生成時に redact しない |
| webhook URL マスク（**除去**） | `webhook.rs:290-295` | `redact_webhook_url` | **covered 経路（work-channel 出力・debug・log）の呼び出しを除去**。配送 URL トークンも masking せずそのまま保持する（P4）。例外的存続は廃止 |

---

## 5. テスト戦略 / Testing Strategy

### 5.1 単体テスト（既存 `#[cfg(test)]` に追加）

- **発生源**: `truncate_bytes` が full を失わない（or `omitted_bytes` を正しく報告）こと。
  64 KiB 超の stdout/stderr で末尾が artifact から復元できること。
- **chunk ロスレス性**: 任意長の入力に対し、chunk 投稿を**連結すると元と一致**する（往復同一性）。
  マルチバイト境界（日本語）で文字化けしないこと（既存 `chunk_text` の char 単位を維持）。
- **LossInfo 正しさ**: `reason`/`scope`/`omitted`/`unit` が実際の欠落と一致。`detectable=false` の経路で
  「検出不能」表示になること。
- **covered 経路の unredacted/unmasked 保証（新要件）**: 従来 redact/mask されていた代表的な文字列が
  **covered 経路の出力・debug・log でそのまま保持される**ことをアサートする。最低限、以下のサンプルを含む入力で
  出力に `[REDACTED]` / `[redacted]` 等のマスキングマーカーが **一切現れず**、原文の文字列がバイト一致で残ること:
  - API キー風: `sk-ABC123...`, `ghp_ABC123...`, `xoxb-...`, `AKIA...`
  - `KEY=VALUE` 形式: `API_TOKEN=...`, `SECRET=...`, `PASSWORD=...`, `API_KEY=...`
  - `Authorization: Bearer <token>`、長い base64/hex 文字列
  - **webhook 配送 URL 風**: `/api/webhooks/` を含む URL（例
    `https://discord.com/api/webhooks/123456789012345678/AbCdEf-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX`）。
    末尾トークンを含む URL 全体が **`[redacted]` 化されずバイト一致で残る**こと。
  - 上記を command text / stdout / stderr / tool 引数 / tool result preview / full-output artifact、
    およびサーバーログ・診断ログの **各 covered 経路**で個別に検証する
    （どれか 1 経路でも `[REDACTED]` / `[redacted]` が出たらテスト失敗）。
- **マーカーの単一化**: covered 経路の出力・ログに現れるマーカーは `[truncated …]` のみで、
  `[REDACTED]` / `[redacted]` が混入しないこと。`LossInfo` は切り捨て由来のみを記録し、
  redaction/masking 由来の欠落を持たないこと（P4）。
- **webhook URL の保持**: `redact_webhook_url` が covered 経路（work-channel 出力・debug・log）で
  作用しないこと。`/api/webhooks/` を含む文字列が当該経路を通過しても **無改変で残る**ことをアサートする。
- **preview に full_ref 必須**: 短縮が起きたあらゆる view が `full_ref`（file/log ref）を持つこと
  （持たない view を生成したらテスト失敗）。
- **event cap 可視化**: 200 件超で「N 件省略 + full_ref」メッセージが出ること（silent drop 不可）。

### 5.2 統合 / プロパティテスト

- mock webhook receiver（HTTP）を立て、巨大出力（数 MiB）の tool 実行で
  受信メッセージ群＋添付＋log ref から **full output を再構成できる**ことを検証。
- 配送順序: 同一 run の chunk が `part X/N` 順で届くこと（既存 mpsc worker の不変条件）。
- 異常系: 永続化失敗・添付失敗時に**成功を装わず**エラーが可視化されること（フォールバック禁止の検証）。

### 5.3 手動 / E2E

- `./dev.sh start` 後、ダッシュボードからエージェント登録 → 大量出力コマンド（例 `seq 1 100000`）を
  実行し、Discord work-channel で (1) preview が簡潔、(2) full がファイル/chunk/ref で取得可能、
  (3) 末尾・中央が失われていない、を目視確認。
- secret 風文字列を出力するコマンド（例 `echo API_TOKEN=sk-test-EXAMPLE1234567890`）を実行し、
  Discord work-channel で **`[REDACTED]` に置換されず原文のまま表示される**ことを目視確認（新要件）。
- webhook 配送 URL 風文字列を出力するコマンド（例
  `echo https://discord.com/api/webhooks/123456789012345678/EXAMPLE-token`）を実行し、
  Discord work-channel・サーバーログ・診断ログのいずれの covered 経路でも
  **`/api/webhooks/` を含む URL が `[redacted]` 化されず原文のまま残る**ことを目視確認（新要件）。

---

## 6. 受け入れ基準 / Acceptance Criteria

- **AC1**: 重要な stdout/stderr/tool 出力が **silently に切り捨てられない**。短縮が起きる場合は
  必ず `LossInfo`（量・位置）と full への参照が同伴する。
- **AC2**: arbitrary な中央スライス / 末尾切り捨てが **「最終的な出力」にならない**。full は
  chunk / 添付 / log ref のいずれかで**ロスレスに**到達可能（往復同一性テストが通る）。
- **AC3**: Discord サイズ上限が **明示的なロスレス戦略**（3.3）で扱われる。どの戦略でも full_ref が付く。
- **AC4**: covered 経路（work-channel 出力の command text / stdout / stderr / tool 引数 /
  tool result preview / full-output artifact、およびサーバーログ・診断ログ）が
  **unredacted / unmasked** で配送・保持される。従来 redact/mask されていた代表的な文字列
  （`sk-…` / `ghp_…` / `KEY=VALUE` / `Bearer …`、および `/api/webhooks/` を含む webhook 配送 URL）が
  **出力・ログにそのまま保持され**、OpenCrab の redaction/masking に起因する
  `[REDACTED]` / `[redacted]` 等のマスキングマーカーが covered 経路に**一切現れない**
  （§5.1 の保持テストが通る）。covered 経路に現れるマーカーは `[truncated …]` のみ。
- **AC4b**: covered 経路（work-channel・debug・log）を **機密チャネルとして扱う前提**がドキュメント化
  されている（raw command/output に secret が、debug/log に有効な webhook URL が混入しうるトレードオフを明示）。
  `redact_webhook_url` による配送 URL トークンの masking は **covered 経路では行わない**
  （例外的存続は廃止）。raw な webhook URL が漏洩しうることを受け入れる根拠として、
  **デバッグ可能性を優先すること**、および **漏洩・侵害時は当該 webhook を即座に無効化できること**が
  明記されている。
- **AC5**: 上流（ツール / モデル / 外部）由来の欠落（L0 等）が **「部分出力」と明示**され、可能なら
  欠落量と full_ref を伴う。検出できない欠落は「検出できなかった可能性」と正直に表示する。
- **AC6**: 完了 / サマリ webhook は**簡潔のまま**だが、**必ず** full output への参照（ファイル名 / log ID /
  URL）を含む。
- **AC7**: event cap などの打ち切りが **可視メッセージ**で告知される（silent drop ゼロ）。
- **AC8**: 永続化 / 添付の失敗時に**フォールバックで成功を装わない**（エラーが可視）。

---

## 7. オープンな問い / Open Questions

1. full output の保存先: DB（`output_artifacts` テーブル）か、workspace 内ファイルか、両対応か。
   サイズ・保持期間・GC ポリシーは？
2. `full_ref` の到達手段: ローカルダッシュボード URL は外部 Discord から踏めない。
   ファイル添付を既定にするか、認証付き log UI（`docs/design-llm-log-ui.md`）を整備するか。
3. 既定 mode: `summary`（preview + ref）を既定とし、`full`(chunk) は opt-in でよいか。
4. L0 の full spill をデフォルト有効にするとディスク/メモリ負荷が増える。閾値・上限の妥当値は？
5. 後方互換: 既存 `truncated: bool` を消費している箇所（payload を読む外部 receiver）への影響範囲。

---

## 8. 非対象 / Out of Scope

- 本設計が対象としない経路（covered 経路外のサブシステム）における redaction ルールの拡充（別タスク）。
  本設計は covered 経路（work-channel 出力・debug・log）の redaction/masking 除去のみを扱う。
- Discord 以外の通知先（Slack 等）への一般化。
- モデル側のコンテキスト圧縮（`docs/design-compaction-error-handling.md` 系）。
