# D-612: 時刻トリガーの一本化（`agent_schedules` へ統合）

- Issue: #612（関連: #585 / #586 / #588 / #605・PR #607 / #583 / #584）
- 状態: オーナー承認済み（D1〜D8 すべて推奨案で確定）
- 基点: origin/main 21dfff1

## 0. 要約

- 時刻トリガーの保存先を `agent_schedules` の 1 テーブルにする。間隔は `@every <N>s`、定時は 5 フィールド cron。1 セッションに何行でも置け、行ごとに `message`（プロンプト）を持つ。
- `session_heartbeat_config` / `session_heartbeat_instructions` / `agent_heartbeat_config` / `agents.heartbeat_instructions` / `heartbeat_instructions_audit` と、指示の多段解決（`resolve_session_heartbeat_instructions`）を削除する。
- 発火経路は、いまハートビートが通っている **TimedFire 経路の 1 本**にする（§8・判断事項 D1）。移行したハートビートの配送・プロンプトの形が変わらないようにするため。
- `last_fired_at` を書くのは発火経路だけにする。設定変更で動かすのは `anchor_at` だけ。次回発火の起点は `max(last_fired_at, anchor_at)`（§3）。
- ツールは `get_my_schedules` / `set_my_schedule` / `update_my_schedule` / `delete_my_schedule` の 1 系統にする。`get_my_heartbeat` / `set_my_heartbeat` / `update_heartbeat_instructions` / `read_heartbeat_instructions` は削除する。`run_my_heartbeat` は id 指定の手動発火に置き換える（判断事項 D2）。
- 移行は v57。有効なハートビート行を、間隔・位相・解決済みプロンプトを保ったまま `agent_schedules` へ写す。

## 1. 要件 / 非対象

### 1.1 要件（オーナー承認済み）

- R1 セッションごとに「定期実行（間隔 = `@every Ns`）」と「定時実行（cron）」を**複数**設定できる。トリガー 1 本ごとに自分のプロンプト（`message`）を持つ。
- R2 時刻トリガーの保存先を `agent_schedules` に一本化する。
- R3 旧ハートビートの仕組みを削除する: `session_heartbeat_config` / `session_heartbeat_instructions` / `agent_heartbeat_config` / `agents.heartbeat_instructions` / 指示の多段解決。
- R4 設定場所は 2 つ。
  - ダッシュボードのチャンネル画面（`web/src/pages/AgentChannels.tsx`）に、セッションごとのトリガー一覧（追加・編集・有効/無効・削除）を置く。
  - エージェントのツールは `set_my_schedule` 系 1 系統に統合し、`set_my_heartbeat` は廃止する。
- R5 移行後も、既存の有効ハートビートの挙動（間隔・位相・プロンプト）を変えない。プロンプトは移行時点の解決結果を `message` に固定する。
- R6 #612 の受入条件（§11）を全部満たす。

### 1.2 非対象

- cron / `@every` の構文拡張や、新しいトリガー種別（アラーム等）は扱わない。
- `SessionLocks` / TimedFire sink / extgate の said-less ターンの実装そのものは変えない（呼び方だけ変える）。
- 設定ファイルのキー `[agent] heartbeat_interval_secs` / `heartbeat_enabled` / `heartbeat_min_interval_secs` は、このブランチでは消さない（判断事項 D7）。
- #585 の後半（`agents.instructions` / persona が全 transport に漏れる件）は扱わない。ここで消えるのはハートビート指示のスロットだけ。
- チャンネル設定表のうち、ハートビート以外の列（readable / writable / whitelisted）は扱わない（§2.3）。
- 既知の別件 X1（sink がバインディングを解決できなくても発火済みとして記録される）は直さない（§8.4）。

## 2. 画面 / 操作

### 2.1 チャンネル画面（`AgentChannels.tsx`）に「時刻トリガー」セクションを追加する

- **一覧の取得**: `GET /api/agents/{id}/schedules` の結果を `session_id` ごとにまとめて表示する。
- **各行の表示**: 種別（間隔 / 定時。`cron_expr` が `@every` で始まるかで判定）、`cron_expr`、`timezone`（定時だけ）、`message` の先頭、有効トグル、`next_fire_at`、`last_fired_at`、`gated_reason`。
- **追加**: 対象セッション、種別、式、`timezone`、`message`、有効を入力して `POST /api/agents/{id}/schedules` を呼ぶ。
  - 種別が「間隔」なら、秒数の入力から `@every <N>s` を組み立てる。
  - 対象セッションは `GET /api/sessions` のうち `agent_ids` にそのエージェントを含むものから選ぶ（新しい API は足さない）。
    - 追記（QC で判明）: `GET /api/sessions` は 1 ページ最大 100 件で、更新の古い Discord / Nostr のセッションが 1 ページ目に入らず選べなかった。既存の `before` カーソルで最後のページまで読んでから絞る（API 追加なし）。
  - 発火経路の無いセッションはサーバが 400 で拒否するので、画面にはエラーとして出す。
- **編集**: `PATCH /api/schedules/{sid}`（cron_expr / timezone / message / enabled）。
- **有効/無効**: 同じ PATCH で `enabled` だけを送る。
- **削除**: `DELETE /api/schedules/{sid}`。確認ダイアログを出す。

### 2.2 ラベル

i18n（`web/src/i18n/locales/{ja,en}.json`）に `channels.triggers.*` を追加する。文言は「間隔」「定時」「プロンプト」「次回」「最終発火」。

### 2.3 削除するもの

- `ChannelConfigDto` の `heartbeat_enabled` / `heartbeat_interval_secs`（`web/src/api/types.ts:179-180`）。
- 画面側の該当列（`AgentChannels.tsx:88,124-133`）と `Setup.tsx:557-558`。

注記: この画面が呼んでいる `/agents/{id}/channel-configs` は、サーバ側に既にルートが無い（`crates/server/src/lib.rs` に無く、`scripts/gateway_boundary_audit.py:37` が禁止している）。チャンネル設定表そのものの扱いは判断事項 D5。

## 3. データと不変条件

### 3.1 保存形（既存テーブル。列は増やさない）

`agent_schedules`（`crates/db/src/schema/sql/full.rs:649-662`）:

| 列 | 意味 |
|---|---|
| `id` | AUTOINCREMENT。一意制約は無い（同じセッションに複数行を置ける） |
| `agent_id` | 発火させるエージェント |
| `session_id` | 発火先のセッション |
| `cron_expr` | 5 フィールド cron、または `@every <dur>` |
| `timezone` | cron の評価に使う tz |
| `message` | 発火時に渡すプロンプト |
| `enabled` | 有効/無効 |
| `anchor_at` | 起点 |
| `last_fired_at` | 最終発火時刻 |
| `created_at` / `updated_at` | 作成・更新時刻 |

### 3.2 `last_fired_at` の 2 つの役割と `anchor_at`

| 役割 | 間隔（`@every d`） | 定時（cron） |
|---|---|---|
| 次回を決める起点 | `base + d` が次回時刻そのもの | `base` より後の最初のスロット（`find_next_occurrence(.., inclusive=false)`） |
| 同じスロットを 2 回発火させない | `base = last_fired` なら次回は必ず `last_fired + d` | `inclusive=false` なので `base = last_fired` のスロットを再び返さない |

- `anchor_at` の役割は「これより前には遡って発火しない床」。
- 現状の `base = last_fired.or(anchor)`（`crates/server/src/schedule_cron.rs:127`）では、設定変更で `last_fired` を NULL に消さないと、新しい式で過去スロットへ遡って発火してしまう。そのため現状は消している（`crates/server/src/api/schedules.rs:261-275`）。これが #612 の指摘する不変条件違反の原因。

**新しい規則**

- I1 `base = max(last_fired_at, anchor_at)`。どちらか一方だけあればそれを使う。両方無ければ `None` で、即発火可になる。
  - 変更点は `schedule_next_fire_at` の 1 行だけ（`or` を `later_of` に替える）。
  - スケジューラは現在も `effective_last = later_of(db_last, attempt)` を渡している（`scheduler.rs:237`）。したがって実質の起点は `max(last_fired, attempt, anchor)` になる。
- I2 `last_fired_at` を書けるのは `set_agent_schedule_last_fired` だけ。呼ぶのはスケジューラの発火成功時だけ（`scheduler.rs:679`）。
  - INSERT は常に NULL で入れる。
  - `update_agent_schedule` の SET から `last_fired_at` を外す。構造で担保し、呼び出し側に規則を書かせない。
- I3 設定変更で動かしてよいのは `anchor_at` だけ。
  - 式 / tz の変更、または無効→有効の切り替えでは `anchor_at = now` にする。
  - message だけの変更と無効化では、何も動かさない。
- I4 同じセッションに何行あってもよい。スケジューラのキーは行の `id`（`EntryKey::Schedule { schedule_id }`・`scheduler.rs:83-88`）。

**I1〜I3 で得られること**

- (a) 設定変更後の次回は、必ず `now` より後になる。設定を触っても即発火しない。
- (b) 定常状態では `anchor_at ≤ last_fired_at` なので、`base = last_fired_at` になり、現行のハートビートと同じ位相になる。
- (c) 定時でも、同じスロットを 2 回発火しない（`base ≥ last_fired`、かつ `inclusive=false`）。

**挙動が変わる点（意図した変更）**

- 旧 `set_my_heartbeat` では、再有効化や間隔の短縮で「前回＋新間隔が既に過ぎていれば即発火」した（`crates/server/src/system_actions/definitions/heartbeat_schedules.rs:61` の説明）。
- 新しい規則では「now＋新間隔」まで待つ。受入条件「設定変更で意図せず即発火しない」に合わせるため。
- 今すぐ試したい場合は、手動発火（D2）を使う。

## 4. API 入出力

既存のルート（`crates/server/src/lib.rs:341-342`）はそのまま使い、新しいエンドポイントは足さない。

| メソッド / パス | 入力 | 出力 | 変更 |
|---|---|---|---|
| `GET /api/agents/{id}/schedules` | なし | `{agent_id, schedules: ScheduleDto[], count}` | 無し |
| `POST /api/agents/{id}/schedules` | `{session_id, cron_expr, timezone?="Asia/Tokyo", message, enabled?=false}` | `ScheduleDto`。不正な値や発火経路が無ければ 400 | 冪等に再登録するときに、`last_fired_at` を消さないようにする（`schedules.rs:190-196`） |
| `PATCH /api/schedules/{sid}` | `{session_id?, cron_expr?, timezone?, message?, enabled?}` | `ScheduleDto`。400 / 404 | `next_anchor_and_last_fired` を、`anchor_at` だけを返す関数に変える。`last_fired_at` は書かない |
| `DELETE /api/schedules/{sid}` | なし | `{id, message}`。404 | 無し |

- `ScheduleDto`（`schedules.rs:43-60`）の形は変えない。
- `next_fire_at` は I1 で算出する。`from_row` と scheduler は同じ関数を使う。

**削除する API 入力**

- `PUT /api/agents/{id}` の `heartbeat_instructions`（`crates/server/src/api/agents/core.rs:125,166`）。
- エージェントの patch の `heartbeat_instructions`（`crates/db/src/queries/agents.rs:48,180`）。
- serde は未知のフィールドを無視する設定なので、古いクライアントが送ってきても 400 にはならず、値が無視されるだけ。

## 5. ツール仕様

| ツール | 扱い |
|---|---|
| `get_my_schedules` | 残す。説明文から「ハートビートとは別物」を消し、`@every` が定期実行だと明記する |
| `set_my_schedule` | 残す。説明に「間隔実行は `@every 30m` のように書く」「複数登録でき、行ごとに message を持つ」を足す |
| `update_my_schedule` | 残す。説明の「cron_expr / timezone を変えたときは次回が今を起点に取り直される」は I3 と一致するので、そのまま残す |
| `delete_my_schedule` | 残す |
| `get_my_heartbeat` / `set_my_heartbeat` | 削除。定義は `heartbeat_schedules.rs:45-78`、実装は `crates/server/src/agent_heartbeat.rs` |
| `update_heartbeat_instructions` / `read_heartbeat_instructions` | 削除。定義は `heartbeat_schedules.rs:3-35`、実装は `crates/server/src/heartbeat_instructions.rs` |
| `run_my_heartbeat` | D2 の推奨案: `run_my_schedule { id }` に置き換える |

`run_my_schedule` の推奨仕様（D2）:

- 権限は `run_my_heartbeat` と同じで、オーナー / co_agent だけ（`OWNER_ONLY_ACTIONS`）。
- 対象は `get_my_schedules` の id で、所属チェックは `load_owned_schedule` を再利用する。
- 発火は定時発火と同じ関数（`run_one_heartbeat`）を spawn するだけ。`last_fired_at` は更新しない。
- 現在の `run_my_heartbeat` にある `session_id` 引数（別セッションを発火する）は引き継がない。所属チェックを通った id だけを受け付ける。

使い勝手の比較: 旧 `set_my_heartbeat{enabled, interval_secs}` の操作は、新 `set_my_schedule{cron_expr:"@every 1800s", message}` または `update_my_schedule{id, enabled}` で置き換えられる。

- 失うもの:
  - `interval_secs` 省略時の既定値補完。
  - 下限 300 秒 / 上限 24 時間のチェック（D4）。
  - 「エージェント共通の指示を 1 か所で書く」こと（行ごとに message を書く）。
- 得るもの:
  - 複数トリガーを置けること。
  - トリガーごとのプロンプト。
  - 定時実行。

## 6. 権限

- 新しい権限は足さない。
- `get/set/update/delete_my_schedule` は、現行どおり `TRUSTED_ONLY_ACTIONS`（`crates/actions/src/bridge/policy.rs:189-192`）に入れ、ハンドラ内でも `ensure_trusted` を通す（`crates/server/src/agent_schedule.rs:49-58`）。
- 削除するツール名は `policy.rs` の各リストから外す（114、152、173、180-181 行）。`run_my_schedule`（D2）は 152 行の位置に置き換える。
- ダッシュボードの CRUD は、既存の認証層の内側に置く（`schedules.rs:4-5`）。
- 移行前との差: 旧 `update_heartbeat_instructions`（オーナーだけ）で守られていたプロンプトは、行の `message` として trusted の呼び出し元が書けるようになる。
  - これは、既存の `set_my_schedule` の `message` と同じ権限。
  - オーナー限定に戻したい場合は判断事項（D8）。

## 7. 状態遷移（1 行）

```
(作成 enabled=false) --enable--> 有効[anchor=now]
(作成 enabled=true)  -----------> 有効[anchor=now, last_fired=NULL]
有効 --due & 発火成功--> 有効[last_fired=now]
有効 --due & 発火失敗--> 有効[メモリ attempt=now による backoff。DB は変えない]
有効 --式/tz 変更--> 有効[anchor=now。last_fired は保持]
有効 --message 変更--> 有効[変化なし]
有効 --disable--> 無効[anchor/last_fired は保持]
無効 --enable--> 有効[anchor=now]
任意 --delete--> (行なし)
```

- `next_fire_at = f(cron_expr, timezone, max(last_fired, attempt, anchor))`。値はキャッシュしない。
- due の判定は `next ≤ now`、または `next = None`（起点なし）。

## 8. 失敗 / 競合 / 再試行

### 8.1 発火経路（D1）

現在、発火経路は 2 本ある。

| | HB（`FireKind::Heartbeat`） | schedule（`FireKind::ScheduledMessage`） |
|---|---|---|
| 呼ぶ関数 | `run_one_heartbeat`（`crates/server/src/heartbeat_fire.rs:36-78`） | `run_one_schedule`（`scheduler.rs:391-517`） |
| 仕組み | TimedFire sink（`crates/extgate/src/fire.rs:55-87`）へ投げて即 `Some` を返す。extgate が `run_v3_said_less_turn` を spawn する | `message` を speech として注入し、`run_agent_response` を呼ぶ |
| ロック | extgate 側で `SessionLocks::run_serialized(session_id)` に入る（`crates/extgate/src/completion.rs:168-171`） | scheduler が `run_serialized` で包む（`scheduler.rs:664-674`） |
| 配送 | gateway へ配送する | 自動配送しない |

- **推奨**: 全行を HB 経路で発火する。プロンプトは `format_heartbeat_prompt(HEARTBEAT_NEUTRAL_CHANNEL_LABEL, message)` のまま（`heartbeat_fire.rs:9-14`）。
- 削除するもの: `run_one_schedule` / `build_scheduled_context` / `FireKind` / `EntryKey::Heartbeat`。
- 理由: R5（移行ハートビートの配送とプロンプトの形を変えない）を満たし、行数も減る。
- 本番の `agent_schedules` は 0 件なので、schedule 経路の挙動に依存している利用者はいない。

### 8.2 同時刻に複数トリガーが立ったとき（確認済みの経路）

1. スケジューラの 1 回の rebuild で、due の行はそれぞれ別の `EntryKey::Schedule{id}` として spawn される（`scheduler.rs:585-619`）。キーが違うので、互いに in-flight でブロックしない。
2. 各タスクは `run_one_heartbeat` を呼び、sink の `fire_timed_turn` が `tokio::spawn(run_v3_said_less_turn)` を投げて、すぐ返る（`fire.rs:84-86`）。
3. `run_v3_said_less_turn` は `sink.runtime.session_locks()` の `run_serialized(session_id)` に入る。
   - server 側の `session_locks()` は、プロセス共有の `AppState::session_locks` を返す（`crates/server/src/agent_runtime_impl.rs:257-260`）。
   - ロックはセッション単位の `tokio::sync::Mutex`（`crates/actions/src/session_runtime.rs:64-69,95-124`）。
   - 同じセッションの通常 inbound ターン（`crates/extgate/src/inbound/turn.rs:61-63`）とも共有される。
4. said-less ターンは inbound の `turn_queues.try_reserve`（容量で捨てる経路）を通らない。そのため捨てられない。

**決定**: 同じセッションで同時刻に立ったトリガーは、両方とも**別々のターンとして直列に**実行される。

- 各ターンは自分の `message` を持ち、それぞれ応答（または NO_REPLY）する。捨てることも、1 本に束ねることもしない。
- 実行順は保証しない。spawn 済みタスクがロックを取る順になる。
- テストでは次を固定する（§14 RED-4）。
  - 2 本とも sink に届く。
  - 2 本のターンの区間が重ならない。
  - 各プロンプトが自分の `message` を含む。

### 8.3 再試行 / backoff

- 現行の仕組みを変えない。
  - 発火できない場合（sink なし・target 解決不可）は `None` を返し、`last_fired` は刻まない。
  - spawn 時にメモリの `attempts[key]=now` を記録し、次回を 1 周期ぶん後ろへ逃がす（`scheduler.rs:602-604,700-713`）。
- 再起動で attempt は消える。このとき `base` は DB の値に戻る。取りこぼしたスロットは 1 回にまとめて発火する。

### 8.4 既知の残リスク（直さない・別 Issue 候補）

- X1: HB 経路は sink へ投げた時点で `Some` を返す（`heartbeat_fire.rs:69-77`）。
  - 後で extgate の `resolve_live_binding` が失敗しても（`fire.rs:56-71`）、`last_fired_at` は進む。
  - in-flight もターン完了まで保持されない。そのため、ターンが周期より長いと同じ行が重なりうる。ただし同じセッションのロックで直列化される。
  - 現行ハートビートと同じ挙動なので、D1 を採用すると全トリガーにこの性質が及ぶ。

## 9. 互換性 / 移行 / 復旧

### 9.1 本番データ（件数のみ）

| テーブル | 状態 |
|---|---|
| `session_heartbeat_config` | enabled=1 は 2 行（Nostr、間隔 18000s / 10800s）。他は enabled=0 |
| `agent_schedules` | 0 件 |
| `session_heartbeat_instructions` | 0 件 |
| `agent_heartbeat_config` | 3 行（うち 2 行 enabled=1） |
| スキーマ | v56 |

- `agent_heartbeat_config` について: 本番コードに読み手は無い。
  - 参照は `crates/db/src/queries/heartbeat.rs:61-264` の内部関数からだけ。
  - その関数（`resolve_agent_heartbeat` / `list_agents_with_heartbeat_enabled` / `resolve_channel_heartbeat_interval`）の呼び出し元はテストだけ。
  - scheduler は `session_heartbeat_config` しか列挙しない（`scheduler.rs:149`）。
  - したがって発火に使われていない。移さずに削除する。
- #612 本文の「有効 5 件（Nostr 2 / Discord 3）」は起票時点の数字。現時点で移行対象になる有効行は 2 行。

### 9.2 v57 マイグレーション（Rust の `up`。1 トランザクション）

1. `session_heartbeat_config` の `enabled=1` の各行を、`agent_schedules` へ INSERT する。
   - `cron_expr = "@every {secs}s"`。`secs` は現行の `resolve_session_interval_secs(interval_secs, default, min)` と同じ値にする。
     - 移行時点の設定値（default / min）は DB 層から見えない。そのため、`interval_secs` が NULL の行は移行を失敗させる（fail-closed）。
     - `interval_secs` が 300 未満の行も、実効間隔が変わるので失敗させる。
     - 本番の 2 行は両方とも値があり、300 以上なので該当しない。
   - `timezone = 'Asia/Tokyo'`（`@every` では使われない）。
   - `enabled = 1`。
   - `anchor_at` と `last_fired_at` は**そのまま**コピーする。位相を保つため。
   - `message` = 移行時点の解決結果。解決は次の順で最初に値があるものを使う。
     - `session_heartbeat_instructions.override_text`（NULL でなければ）。
     - `sanitize(agents.heartbeat_instructions)`（空でなければ）。
     - `DEFAULT_HEARTBEAT_INSTRUCTIONS`。
     - sanitize と既定文は v57.rs に凍結コピーする。本体の関数は削除されるため。
2. 次を DROP する: `session_heartbeat_instructions`、`session_heartbeat_config`、`agent_heartbeat_config`、`heartbeat_instructions_audit`。
3. `ALTER TABLE agents DROP COLUMN heartbeat_instructions` を実行する（bundled SQLite で使える。前例は `crates/db/src/schema/baseline.rs:398`）。
4. `SCHEMA_SQL`（`full.rs`、`sql/mod.rs`）からも同じテーブル・列を削除する。
   - `channel_config.heartbeat_*` は legacy 表。新規 DB では最後に DROP される（`crates/db/src/schema/mod.rs` の initialize）ので、historical migration 用の SQL には残す。

**enabled=0 の行**は移さない（判断事項 D3）。

**挙動の同一性**

- 旧 `next = last_fired.or(anchor) + interval`（`session_heartbeat.rs:491-498`）。
- 新 `next = max(last_fired, anchor) + d`。
- 旧 `set_my_heartbeat` は、起点があれば anchor を動かさない（`agent_heartbeat.rs` の set 本体）。そのため `anchor ≤ last_fired` が成り立ち、新旧の値は一致する。
- 念のため、移行テストで「移行前後の next_fire_at が一致する」ことを固定する。

**G ゲートの影響**: 現在の scheduler は live G を使っていない（`scheduler.rs:141` の `_live_g`）。統合による影響は無い。

### 9.3 番号衝突

- PR #1035 も v57 を追加する予定（`crates/db/src/schema/migrations/v57.rs`）。あわせて gateway-migrate の `core_user_version` を 57 に上げる。
- **後からマージする側が v58 に付け替える。**
- gateway-migrate の `manifest.rs:101`（`core_user_version == 56` の固定）も、付け替え後の番号に合わせる必要がある。

### 9.4 gateway-migrate との関係（D6）

`crates/gateway-migrate` は、legacy の `channel_config.heartbeat_*` を読み、次の関数で `session_heartbeat_config` / `session_heartbeat_instructions` へ投影している。

- `crates/gateway-migrate/src/projection.rs:197,322,436,462`
- `crates/gateway-migrate/src/source.rs:131,143`
- 使っている関数: `project_stopped_session_heartbeat_target_in_tx` / `resolve_heartbeat_projection_sources` / `heartbeat_projection_fingerprints`（`session_heartbeat.rs:70-260`）

v57 で投影先のテーブルが無くなるため、この経路はそのままでは動かない。

### 9.5 復旧

- v57 は非可逆（DROP を含む）。
- デプロイ前に DB のバックアップを取る。戻すときは「バックアップの復元＋旧バイナリ」に戻す。
- マイグレーションは 1 トランザクションで行う（`crates/db/src/schema/mod.rs`）。失敗すれば v56 のまま残り、起動エラーになる。
- 互換レイヤやフォールバック読み出しは作らない。

## 10. 変更箇所（ファイル単位）

**DB**

- `crates/db/src/schema/migrations/v57.rs`（新規）、`migrations/mod.rs`: §9.2 の移行。
- `crates/db/src/schema/sql/full.rs`、`sql/mod.rs`: 削除したテーブル・列を SCHEMA から除く。
- `crates/db/src/queries/agent_schedules.rs`: `update_agent_schedule` から `last_fired_at` を外す（I2）。テストも更新する。
- `crates/db/src/queries/session_heartbeat.rs`: 削除。
- `crates/db/src/queries/heartbeat.rs`: 削除。
- `crates/db/src/queries/mod.rs`: 上の 2 ファイルの re-export を外す。
- `crates/db/src/queries/agents.rs`: `heartbeat_instructions` のフィールドと patch を削除する。
- `AgentRow` を組み立てているファイルからフィールドを削除する: `crates/actions/src/soul.rs`、`crates/cli/src/main.rs`、`crates/core/src/agent.rs`、`crates/core/src/import/import_service.rs`、`crates/core/src/context_budget/core_process_e2e/assembly_identifiers.rs`、`crates/db/src/queries/gate_binding.rs`、`crates/db/src/queries/subject.rs`、`crates/nostr/src/gate_provision.rs`、`crates/server/src/api/agents/core.rs`、`crates/server/src/api/llm.rs`、`crates/server/src/intake_process.rs`、`crates/server/src/agent_runtime_impl/peer_review_removal_test.rs`。

**スケジューラ / 発火**

- `crates/server/src/schedule_cron.rs`: `schedule_next_fire_at` の base を `later_of(last_fired_at, anchor_at)` にする（I1）。
- `crates/server/src/scheduler.rs`:
  - ブロック (A)（ハートビートの列挙）を削除する。
  - `EntryKey::Heartbeat` と `FireKind` を削除する。
  - D1 採用時は `run_one_schedule` / `build_scheduled_context` も削除し、全行を `run_one_heartbeat(state, agent_id, target, &message)` で発火する。
  - `default/min_interval_secs` の引数を削除する。
- `crates/server/src/heartbeat_fire.rs`:
  - `run_one_heartbeat` に `message: &str` を渡す。
  - 指示の解決（`resolve_session_heartbeat_instructions`）を削除する。
  - `heartbeat_log` の source を `"schedule"` にし、schedule_id を載せる。

**API / ツール**

- `crates/server/src/api/schedules.rs`: `next_anchor_and_last_fired` を `next_anchor` にし、`create_schedule_core` で `last_fired_at` を消さないようにする。
- `crates/server/src/agent_heartbeat.rs`: 削除（D2 を採用する場合、`run_my_schedule` は `agent_schedule.rs` へ移す）。
- `crates/server/src/heartbeat_instructions.rs`: 削除。
- `crates/server/src/lib.rs`: `pub mod agent_heartbeat` と `heartbeat_instructions` を削除する。`heartbeat_limits` は D7 に従う。
- `crates/server/src/system_actions/definitions/heartbeat_schedules.rs`: 定義を削除し、説明文を更新する。
- `crates/server/src/system_actions/definitions.rs`: 同上。
- `crates/server/src/system_actions/gateway_actions.rs`: ルーティングを削除する（78-83 行ほか）。
- `crates/actions/src/bridge/policy.rs`: 削除したツール名を外し、D2 を反映する。
- `crates/server/src/process/prompt.rs:239-247`: カテゴリの表を更新する。
- `crates/server/src/config/maintenance.rs`: `HeartbeatLimits`（D7）。

**テスト / 付帯**

- テスト:
  - `crates/server/src/system_actions/tests/{agent_heartbeat_basics,agent_heartbeat_schedule,heartbeat_instructions,definitions,mod}.rs`
  - `crates/db/src/queries/tests/heartbeat.rs`
  - `crates/server/src/scheduler/tests*.rs`
  - `crates/server/src/api/schedules/tests.rs`
  - `crates/server/tests/{qc_harness_e2e/heartbeat_925.rs,discord_qc_harness/heartbeat.rs}` など
  - 旧仕様を固定しているテストは削除し、§14 のテストに置き換える。
- baseline / 文書: `baseline/l1`、`baseline/l2` の tool カタログ、`README.md:42,285,318-321,434`、`skills/opencrab-handbook.skill.md`。
- gateway-migrate: D6 に従う。

**Web**

- `web/src/pages/AgentChannels.tsx`、`web/src/api/schedules.ts`（新規。既存のエンドポイントを呼ぶだけ）、`web/src/api/types.ts`、`web/src/pages/Setup.tsx`、`web/src/i18n/locales/{ja,en}.json`。

## 11. 受入条件（#612 の全条件と本書の対応）

| # | 条件 | 対応 |
|---|---|---|
| 1 | 時刻トリガーの実装が 1 つ | 保存は `agent_schedules` だけ。D1 採用時は発火経路も 1 本 |
| 2 | `last_fired_at` を進めるのは発火時だけ（全経路） | I2: `update_agent_schedule` の SET から外す。INSERT は NULL。書くのは `set_agent_schedule_last_fired` だけ |
| 3 | 設定変更で意図せず即発火しない | I1+I3: 変更後の次回 > now |
| 4 | 間隔と定時が同じセッションで両立 | I4 |
| 5 | 同じセッションに複数トリガーを置ける | I4。一意制約は無い |
| 6 | 同時刻に複数立ったときの挙動が決まっている | §8.2（別ターンとして直列）。テストで固定 |
| 7 | 既存ハートビートが移行後も同じ挙動 | §9.2（位相の等式、配送経路、プロンプトの形） |
| 8 | チャンネル指示を読む経路が残らない | `resolve_session_heartbeat_instructions` / `resolve_heartbeat_instructions` 系と、その読み手をすべて削除。`git grep heartbeat_instructions` が v57 の凍結コードと過去 migration 以外で 0 件 |
| 9 | `agent_heartbeat_config` が消える | v57 で DROP し、クエリも削除 |
| 10 | トリガーごとに別のプロンプト | 行の `message` |
| 11 | エージェント向けツールの使い勝手が落ちない | §5（失うもの / 得るもの）。D2 / D4 |
| 12 | 正味の行数が減る | ファイルを削除し、scheduler を 1 経路にする。PR に `git diff --stat` を添える |

- #586 の完了条件: 9 を満たし、`agents.heartbeat_instructions` も削除することで満たされる。閉じる前に、読み手と書き手を再度 grep で確認する。
- #585 はハートビート部分だけ解消する。`agents.instructions` / persona の漏れは残るので、#585 は閉じない（オーナー確認）。

## 12. オーナー判断事項

- **D1 発火経路の統一**
  - 推奨: 全トリガーを TimedFire（ハートビート経路。gateway へ配送し、ハートビートの枠で包んだプロンプト）で発火する。`run_one_schedule`（speech を注入し、配送しない）は削除する。
  - これが無いと、移行したハートビートの配送とプロンプトの形を保ったまま、発火経路を 1 本にできない。
- **D2 `run_my_heartbeat` の扱い**
  - 推奨: `run_my_schedule{id}`（オーナー / co_agent、`last_fired` は更新しない）に置き換える。
  - これが無いと、エージェントが待たずにトリガーを試せなくなる。
  - 別案は単純削除。
- **D3 無効（enabled=0）のハートビート行**
  - 推奨: 移さずに捨てる。
  - 移したい場合は `enabled=0` の行として写す（`interval_secs` が NULL の行の扱いも決める必要がある）。
- **D4 `@every` の下限 / 上限**
  - 旧 `set_my_heartbeat` は 300s〜24h を強制していた。`set_my_schedule` は「0 より大きい」ことしか見ない（`schedule_cron.rs:14-18`）。
  - 推奨: 追加しない。新しい制約を足さないため。
  - 必要なら「無いと、エージェントが短周期で費用を増やせる」ことを理由に、下限を足す。
- **D5 チャンネル画面の既存チャンネル設定表**
  - この表は、既に存在しない API を叩いている。
  - 推奨: 本件では heartbeat 列だけを外す。表自体の撤去は別 Issue にする。
- **D6 gateway-migrate の heartbeat 投影**
  - 本番で #1006 の移行（投影と cleanup）が完了済みなら、投影処理を削除する。
  - 未完了なら、v57 より前に実行する必要がある。本番の完了状況はオーナーに確認する。
- **D7 `HeartbeatLimits` と設定キー**（`heartbeat_interval_secs` / `heartbeat_min_interval_secs` / `heartbeat_enabled`、`heartbeat_config_rx`）
  - 統合後は、読み手が無くなる（`heartbeat_enabled` は現時点で既に未使用）。
  - 推奨: 本件で `HeartbeatLimits` の読み手ごと削除し、設定キーは別 Issue で廃止する。
- **D8 プロンプトを書き換える権限**
  - 旧: ハートビート指示はオーナーだけが書けた。
  - 新: trusted の呼び出し元が `message` を書ける。
  - 推奨: 現行の `set_my_schedule` と同じ trusted のまま。

## 13. 根拠（ファイル:行）

**`agent_schedules`**

- テーブル定義（一意制約なし）: `crates/db/src/schema/sql/full.rs:649-662`
- `update_agent_schedule` が `last_fired_at` を上書きしている: `crates/db/src/queries/agent_schedules.rs:115-141`
- 発火時の更新: `agent_schedules.rs:143-160`

**旧ハートビートのテーブル**

- 定義（PK は `(agent_id, session_id)`）: `full.rs:617-626`、`full.rs:630-639`
- upsert が CONFLICT 時に `last_fired_at` を触らない（#605）: `crates/db/src/queries/session_heartbeat.rs:297-334`
- 指示の多段解決: `session_heartbeat.rs:380-408`
- ハートビートの次回計算: `session_heartbeat.rs:491-498`
- 間隔の解決（下限への床上げ）: `session_heartbeat.rs:463-478`

**cron**

- 次回計算（base と `inclusive=false`）: `crates/server/src/schedule_cron.rs:121-150`
- 下限を設けない方針: `schedule_cron.rs:14-18`

**API（`crates/server/src/api/schedules.rs`）**

- 変更時に `last_fired_at` を NULL にしている: `schedules.rs:256-275`
- 冪等再登録時に `last_fired_at` を消している: `schedules.rs:186-196`
- DTO: `schedules.rs:43-60,92-130`

**scheduler（`crates/server/src/scheduler.rs`）**

- キー: `scheduler.rs:83-88`
- rebuild（ハートビートは 149、schedule は 206）: `scheduler.rs:138-266`
- `_live_g` が未使用: `scheduler.rs:141`
- schedule 経路: `scheduler.rs:391-517`
- 本体ループ: `scheduler.rs:529-719`
- HB の `last_fired` 更新: `scheduler.rs:633`
- schedule の `run_serialized` と `last_fired` 更新: `scheduler.rs:664-690`

**TimedFire 経路と `SessionLocks`**

- `run_one_heartbeat`（投げたら `Some`）: `crates/server/src/heartbeat_fire.rs:36-78`
- sink が spawn する: `crates/extgate/src/fire.rs:55-87`
- said-less ターンの `run_serialized`: `crates/extgate/src/completion.rs:163-171`
- inbound ターンの容量チェックと `run_serialized`: `crates/extgate/src/inbound/turn.rs:50-63`
- 共有ロック: `crates/server/src/agent_runtime_impl.rs:257-260`
- 共有ロックの実体: `crates/server/src/lib.rs:123`
- Mutex: `crates/actions/src/session_runtime.rs:64-69,95-124`

**`agent_heartbeat_config`**

- 読み手が内部関数とテストだけ: `crates/db/src/queries/heartbeat.rs:61-264`、`crates/db/src/queries/tests/heartbeat.rs`

**ツール**

- 定義: `crates/server/src/system_actions/definitions/heartbeat_schedules.rs:3-197`
- 権限: `crates/actions/src/bridge/policy.rs:114,152,173,180-192`
- カテゴリ: `crates/server/src/process/prompt.rs:239-247`
- `run_my_heartbeat`: `crates/server/src/agent_heartbeat.rs`（set 本体は 329-, run は 482-）

**Web**

- `web/src/pages/AgentChannels.tsx:3,88,124-133`
- `web/src/api/channel_configs.ts`
- `web/src/api/types.ts:171-181`
- ルート禁止: `scripts/gateway_boundary_audit.py:37`

**マイグレーション**

- 現在の最新: `crates/db/src/schema/migrations/v56.rs`
- `manifest.rs:101`（56 固定）
- gateway-migrate の投影: `crates/gateway-migrate/src/projection.rs:197,322,462`

## 14. 実装段階の完了チェックリスト

**設計条項**

- [ ] I1〜I4、§8.2 の決定、§9.2 の移行を満たす。
- [ ] §12 の D1〜D8 がオーナー承認済みで、承認どおりに実装されている。

**対象の production seam**

- [ ] `schedule_next_fire_at`
- [ ] `update_agent_schedule`
- [ ] `create_schedule_core` / `update_schedule_core` / `update_schedule`（PATCH）
- [ ] `rebuild_entries` と `run_scheduler`
- [ ] `run_one_heartbeat`
- [ ] v57 の `up`
- [ ] `policy.rs` の各リスト
- [ ] ツール定義と dispatch
- [ ] `AgentChannels.tsx`

**RED（先に落ちるテストを書く）**

- [ ] RED-1: `update_agent_schedule` に別の `last_fired_at` を渡しても、DB の値が変わらない（db）。
- [ ] RED-2: cron を変更した直後、および無効→有効にした直後の `next_fire_at` が now より後で、`last_fired_at` が保持されている（api/schedules と tool の両方）。
- [ ] RED-3: 同じ行で、`last_fired` と同じスロットを 2 回返さない。新しい式で過去スロットへ遡らない（schedule_cron）。
- [ ] RED-4: 同じセッションで、`@every` と cron の 2 行が同時に due になる。sink に 2 件届き、各プロンプトが自分の message を含み、共有 `SessionLocks` の下で区間が重ならない（scheduler + extgate conformance の fake runtime）。
- [ ] RED-5: v56 のフィクスチャ（有効 HB 2 行・無効行・`agent_heartbeat_config`・指示 3 種）で v57 を実行する。
  - `agent_schedules` の行数、`@every Ns`、anchor / last_fired、解決済み message が期待どおり。
  - 移行前後で next_fire_at が一致する。
  - 旧テーブルと列が消えている。
  - `interval_secs` が NULL または下限未満の有効行では、移行が失敗する。
- [ ] RED-6: ツールカタログに `set_my_heartbeat` / `get_my_heartbeat` / `*_heartbeat_instructions` が無い。D2 のツールがオーナー / co_agent だけに見える。

**GREEN**

- [ ] 上の RED がすべて通る。
- [ ] `cargo test --workspace` が通る。
- [ ] `cargo clippy` が通る（既知の #1037 を除く）。
- [ ] web のテストと型チェックが通る。
- [ ] `scripts/gateway_boundary_audit.py` のテストが通る。
- [ ] `git grep -E 'session_heartbeat|agent_heartbeat_config|heartbeat_instructions|set_my_heartbeat'` の結果が、v57 の凍結コードと過去 migration / historical SQL だけになっている。

**禁止事項**

- [ ] 旧テーブルを読むフォールバックや互換レイヤを作らない。
- [ ] 新しい権限や API エンドポイントを足さない。承認されていない抽象を足さない。
- [ ] `last_fired_at` を発火経路以外で書かない。
- [ ] 公開文書やテストに、本番の ID・パス・ホスト名を書かない。
- [ ] 正味の行数を増やさない。
