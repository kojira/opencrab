# 設計: per-agent 定時実行（agent_schedules / #455）

ハートビート（固定短間隔の tick）とは別に、「毎朝 7 時にまとめを書く」「3 時間ごとに巡回する」
のような**時刻・周期ベースの定時実行**を提供する。#439 の**中央スケジューラ（同一時刻源）**に
載せて実装する（別ループを立てず、2 つのスケジューラが並立して衝突するのを避ける）。

## データモデル

`agent_schedules`（`crates/db/src/schema.rs`・v37 で新設 / v38 で語彙整合）:

| 列 | 内容 |
|---|---|
| `id` | AUTOINCREMENT |
| `agent_id` | 対象エージェント |
| `session_id` | 注入先の**一本化されたセッション**（`nostr-{agent}` / `discord-{agent}-{guild}-{channel}`） |
| `cron_expr` | 標準 5 フィールド cron（例 `0 7 * * *`）、または `@every 3h` |
| `timezone` | cron 評価に使う tz（既定 `Asia/Tokyo`） |
| `message` | 発火時にエージェントへ注入する指示文 |
| `enabled` | 既定 **0（無効・fail-closed / #240）** |
| `anchor_at` | 発火起点（rfc3339）。`@every` の周期起点、cron の「これ以前を遡及発火しない床」 |
| `last_fired_at` | 最終発火時刻（rfc3339）。`None`=未発火 |
| `created_at` / `updated_at` | |

**語彙・持ち方はハートビートに揃える**（v38）:
- `last_fired_at`（heartbeat と同名。旧 `last_run_at` を v38 で RENAME）。
- **次回発火時刻は列に持たず照会時に算出**（heartbeat の `next_fire_at` と同じ方針・stale フリー）。
  cron 計算は wake 時のみ・件数も僅少でホットパスに無く、キャッシュ列は cron 式/tz/enabled 変更時の
  無効化漏れによる stale リスクだけを増やすため作らない。

## cron / `@every` の解釈（`crates/server/src/schedule_cron.rs`）

- **標準 5 フィールド cron** は [`croner`] で解釈し、`timezone`（[`chrono-tz`]）で評価して UTC 保存。
- **`@every <dur>`** は cron ライブラリに寄せず**自前パーサ**でアンカー方式に解決する
  （`base + dur`。単位 `d`/`h`/`m`/`s` と連結 `1h30m` 可）。周期ゼロ以下は暴走を避けて拒否。
- 次回発火 `next_fire_at = schedule_next_fire_at(cron, tz, anchor, last_fired)`:
  `base = last_fired_at.or(anchor_at)`。`@every` は `base + dur`、cron は `base` 以降の最初のスロット。
- 解釈不能な式は **CRUD で 400**、実行時も**発火対象外（fail-closed）**。

## 発火機構（中央スケジューラ・`crates/server/src/scheduler.rs`）

中央スケジューラは毎ウェイクで `session_heartbeat_config`（HB）と `agent_schedules`（schedule）を
DB から読み直し、永続アンカーから正確な次回発火時刻を算出し、最も近い発火まで眠る。

- **エントリキーは enum**（`EntryKey::Heartbeat{session_id}` / `EntryKey::Schedule{schedule_id}`）。
  schedule は同一セッションに複数ぶら下がるので session_id 文字列では別スケジュールを誤ブロックする。
- 発火時は `message` を対象セッションへ **self-message として注入**し、**通常メッセージ処理経路**
  （`process::run_agent_response`・**caller=Owner**＝HB tick と同じ自己実行）で 1 ターン走らせる。
  heartbeat の probe（SPEAK/LEARN/IDLE）とは別物なので SPEAK/LEARN/IDLE 解釈はしない。
- **応答は生成・記録するが自動配送はしない**（#458 intake と同型）。外界への出力はエージェントが
  自分のツール域（HB tick と同じ）で行う。
- **多重実行防止**: 走行中（in-flight）の同一スケジュールは skip し `last_fired_at` を進めない。
- **missed-run 圧縮**: 再起動をまたいで過ぎたスロットは**1 回だけ**発火（遡及実行はしない）。
- **異常終了 backoff**: パニック/失敗で終わったターンは `last_fired_at` を刻まず、メモリの
  last_attempt で次回を後ろへ逃がして再発火ループを止める（heartbeat と同じ）。
- **即時反映（#437）**: CRUD 後に `scheduler_wake` を鳴らして rebuild させる。再起動不要。
- **同一セッションの schedule 同士は `SessionLocks` で直列化**する。

## G（`agent.heartbeat_enabled`）を掛けない — 重要

`heartbeat_enabled`（G）は**ハートビートのマスタスイッチ**であって schedule のものではない。
**schedule 発火に G は掛けない**（統括裁定）。schedule は自身の `enabled`（既定 0）で制御する。

**帰結**: 運用者が `heartbeat_enabled=false` にしても**定時実行は止まらない**。止めるには各
スケジュールの `enabled` を false にする。これは意図した挙動（heartbeat と schedule は別概念で、
「HB を切ったら日次サマリまで黙って止まる」のは驚きの方向）。config `heartbeat_enabled` の説明にも明記。

## CRUD API（`crates/server/src/api/schedules.rs`）

既存のダッシュボード系エージェント設定 API と同じ認証層の内側に置く（新しい認可ゲートは足さない）。
**新しい自己設定ツールは追加しない**（自律作用面を広げない）——schedule はオーナーがダッシュボードから管理する。

| メソッド | パス | 内容 |
|---|---|---|
| `GET` | `/api/agents/{id}/schedules` | 一覧（各行に照会時算出の `next_fire_at`） |
| `POST` | `/api/agents/{id}/schedules` | 新規登録（cron/tz 検証・session 所属検証・enabled なら anchor=now） |
| `PATCH` | `/api/schedules/{sid}` | 部分更新（cron/tz 明示変更・有効化で anchor=now・last_fired=NULL / 無効化は位相保存） |
| `DELETE` | `/api/schedules/{sid}` | 削除 |

- cron/`@every`/timezone は保存前に検証し、不正なら **400**。
- `session_id` は**そのエージェントの発火経路を持つセッション**（`nostr-`/`discord-`）に限る（不正 400）。
  「登録できたのに永遠に発火しない行」や他エージェントのセッションを作らせない。
- 変更後は `scheduler_wake` を鳴らして即時反映（#437）。

## jitter — 採用しない

発火時刻の jitter は**機構も設定項目も列も作らない**（オーナー裁定）。同時アクセス集中は実測で
問題化しておらず、#439 が直す「発火時刻が読めない」を作り直すことになるため。

## 外部作用面（#455）

schedule 発火は**無人で反復・指向型の外部出力を予測可能に起動できる**（ツール域が HB と同一なので、
エージェントがツールを呼べば外界へ影響しうる）。この面は **HB と同一のまま許容**（A 案・オーナー裁定）で、
schedule だけ狭めも広げもしない。schedule は既定無効（fail-closed）でオーナー認証下の明示分だけ動く。
