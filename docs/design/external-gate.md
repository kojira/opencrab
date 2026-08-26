# External gate V3 最小契約・設計・検収

本書は external gate 第1段の唯一の正本である。旧契約、`DESIGN-EXTGATE.md`、`DESIGN-EXTGATE-V2-ADDENDUM.md` と衝突した場合は本書を採る。

## 1. 目的と脅威モデル

- 言語非依存 gateway と core の境界を、敷設、会話、配送、検収まで一本文書で固定する。
- 接続する gateway は operator が敷設した身内であり、core は gateway の振る舞いを警察しない。
- 相互接続が動かなければ、契約違反側を特定して gateway または core を直す。
- 防御対象は実装事故、壊れた frame、二重取り込み、二重配送、ack 不明、無権限の敷設操作である。

## 2. 用語と識別子

| 語 | 定義 |
|---|---|
| core | UDS を listen し、敷設状態、認可、会話、delivery を所有する本体プロセス。 |
| gateway | UDS に connect し、外部サービスとの I/O と wire 変換だけを所有する別プロセス。 |
| operator | gateway と設定を敷設する管理者。Bearer の唯一の利用者。 |
| instance | gateway 設定の単位。1 instance は exact 1 subject を所有する。 |
| binding | 1 外部会話と 1 core session を結ぶ世代。`session_id` は `session_id_for_binding(binding_id)`、現行実装の値は `extgate-` + `binding_id`。 |
| origin | gateway が同一外部発言へ繰り返し付ける nonempty 文字列。binding 内だけで一意性を持つ。 |
| seq | core が記録した origin に、binding ごとに 1 から単調採番する正整数。 |
| delivery | 1 回の `say` 書き出しを表す core 所有行。自動再送の単位にはしない。 |
| live | process-local registry に存在する、hello 済みで未 close の接続。永続状態ではない。 |

識別子は次で固定する。

- `instance_id`、`binding_id`、`delivery_id`、`activity_id` は canonical lowercase UUID text。UUID version は制限しない。
- request `id` は UTF-8 で 1..128 byte。core が作る bind ID は `bind:` + `binding_id`、say ID は `delivery_id` と byte-equal。
- `kind_id`、`address`、`origin`、`author_id` は nonempty UTF-8。`kind_id` は instance の opaque 値であり、別の登録先を参照しない。
- `subject_id` は positive i64。既存 `agents.agent_id` を置換せず、`agents.subject_id` との exact-one 写像にする。
- `config_digest` は `config_b64` を RFC 4648 standard padded base64 decode した byte 列の SHA-256 lowerhex 64 文字。
- 時刻列は UTC Unix nanoseconds の signed i64。

## 3. 線

### 3.1 transport と frame

- core は設定キー `gate.listen_socket` の Unix domain socket path を 1 本だけ listen する。このキーは V3 で新設する。**空・欠落は「listen しない」**（gateway 未使用でも core は起動する。startup recover は実行する）。非空のときだけ検証し、相対 path・既存の非 socket path は起動失敗にする。
- socket file mode は `0660`。所有 user/group は配備設定で固定し、world-write は付けない。
- frame は UTF-8 JSON object 1 個と LF 1 byte。LF を含め 1,048,576 byte 以下。
- LF 到着前に上限を超えた入力は `too_large` として接続を close する。残りを読み捨てて同期回復しない。
- invalid UTF-8、invalid JSON、JSON object 以外は `bad_request` として接続を close する。
- frame 内の全 object 階層で duplicate member を拒否する。payload と attachments 要素も対象に含める。
- core と gateway は認識しない field を全階層で無視する。field 順序は意味を持たない。
- 必須 field の欠落、型違い、範囲外、未知 enum は `bad_request`。hello では close、running 中の request では error 応答後 keep とする。
- fatal error で信頼できる request `id` を抽出済みなら `err` を 1 回書いてから close する。抽出できなければ stable code を log して応答 0 のまま close する。

### 3.2 共通 JSON 型

```text
Attachment = {kind:"image",url:string}
ErrDetail  = string | null
```

`Attachment.url` は absolute `https` URL。`attachments` は配列として必須で、画像が無いときは `[]`。`said.text` と `attachments` が同時に空なら `bad_request` で記録 0。添付 byte、base64、multipart、local path は受理しない。

### 3.3 message の完全表

表の field は全て必須である。追加 field は §3.1 の規則で無視する。

| 方向 / `m` | exact required fields | 型・値 | 応答 |
|---|---|---|---|
| gate→core `hello` | `id,m,protocol,instance_id,revision,config_digest` | `m="hello"`; `protocol=2`; `revision` positive u64 | core→gate `ok`。失敗は `err` 後 close。 |
| gate→core `said` | `id,m,binding_id,origin,author_id,text,attachments` | `m="said"`; identity は nonempty; `text` は string; `attachments` は `Attachment[]` | core→gate `ok` に `seq`。記録失敗は `err`。 |
| core→gate `bind` | `id,m,binding_id,address` | `m="bind"`; `id="bind:"+binding_id` | gate→core `ok`。受理不能は `err(code="bind_failed")`。 |
| core→gate `say` | `id,m,binding_id,payload` | `m="say"`; `id=delivery_id`; payload は §3.4 | gate→core `ok`。外部 API が非受理と確定した場合だけ `err(code="external_rejected")`。 |
| core→gate `activity` | `m,binding_id,activity_id,state` | `m="activity"`; `state="started"|"ended"` | 応答なし。 |
| response `ok` | `id,m` または `id,m,seq` | `m="ok"`; hello/bind/say は前者、said は `seq:positive-i64|null` を持つ | pending request 1 件を消費。 |
| response `err` | `id,m,code,detail` | `m="err"`; `code` は §5.4; `detail` は `ErrDetail` | pending request 1 件を消費。 |

response は request と同じ `id` を返す。pending の request 種に合わない shape、未知 ID、消費済み ID、bind pending への `external_rejected`、say pending への `bind_failed` は `response_invalid` で接続を close する。close 時の全 pending say は §6.4 の `indeterminate` へ移す。

hello 成功後、core は当該 instance の open binding 全件へ `binding_id ASC` で bind を書き、各 ID を独立 pending として先に登録する。応答順序は問わない。matching `ok` 後だけ当該 binding を acknowledged set に加える。bind `err`、write failure、60 秒経過は `bind_failed` として接続を close する。hello を 10 秒以内に受理できなければ `protocol_order` で close する。

### 3.4 say payload と activity

- core が生成する payload は exact `{"text": body}`。`body` は 1 byte 以上の UTF-8 string。
- gateway は payload の `text` 以外の member を無視する。`text` 欠落、string 以外、空 string は `external_rejected` を返し、外部 API I/O は 0。
- gateway は外部 API が投稿を受理したと確認した後だけ say `ok` を返す。受理前後が不明なら `ok` / `err` を作らず socket を close する。
- activity は表示専用 best-effort。core はターン開始前に `started`、終了 path の finally 相当箇所で同じ `activity_id` の `ended` を 1 回書く。activity 失敗から say を生成、再送、変更しない。

### 3.5 said 応答と同seq

- `(binding_id,origin)` が既存なら、core は `accept_inbound` と会話記録を呼ばず、保存済みの初回 seq を `ok.seq` に返す。payload 差分も再処理しない。
- 新規 origin が core の認可と記録を通過した場合だけ `external_origins` へ 1 行入れ、割り当てた seq を返す。
- core が受理判定で捨てて会話へ記録しなかった場合は origin 行を作らず `ok.seq=null` を返す。
- JSON、binding、store のエラーは `err` であり `seq=null` に変換しない。
- 異なる binding の同じ origin は別発言として、それぞれの binding で seq を採番する。

## 4. 状態機械

### 4.1 lifecycle と registry

接続状態は `PRE_HELLO`、`RUNNING`、`CLOSED` の 3 つだけ。`RUNNING` は process-local registry の `instance_id -> {connection_identity, revision, socket, acknowledged_bindings, pending}` で表す。`connection_identity` は process-local の同一性比較だけに使い、DB、wire、log に出さない。startup の registry は空で、DB から復元しない。

hello の instance 検査と registry 登録、revision POST / instance DELETE の live 検査と DB commit は、同じ registry lock で直列化する。close cleanup は同じ `connection_identity` の entry だけを消し、古い task が新しい接続を消さない。lock poison、task panic、cleanup failureは listener を停止して runtime fail-loud にする。

open binding のうち matching bind `ok` 済みの集合だけを acknowledged とする。新 binding の bind 待ち中も既存 acknowledged binding の said は通す。接続全体を別状態へ落とさない。

### 4.2 core が受ける state × message 総当たり表

`close` は registry cleanup と pending say の terminal 化までを含む。`write 0` は DB と wire の両方で当該 message の成功効果が無いことを示す。

| inbound | PRE_HELLO | RUNNING | CLOSED |
|---|---|---|---|
| valid `hello` | DB の instance、enabled、revision、digest を検査。同 instance の live が無ければ registry 登録→`ok`→open binding の bind。 | `protocol_order`→close、hello 効果 0。 | reader task 終了済み。parser 呼出 0、write 0。 |
| invalid `hello` | `bad_request|protocol_unsupported|instance_unknown|instance_disabled|revision_mismatch|config_digest_mismatch|instance_active|store_error`→close、registry 登録 0。 | `protocol_order`→close、write 0。 | parser 呼出 0、write 0。 |
| valid `said`, binding acknowledged/open | `protocol_order`→close、記録 0。 | §3.5 と §7 に従い `ok(seq)` または `err`。 | parser 呼出 0、write 0。 |
| valid `said`, binding absent/closed/unacknowledged | `protocol_order`→close、記録 0。 | `binding_unknown|binding_closed|instance_not_ready`、keep、記録 0。 | parser 呼出 0、write 0。 |
| malformed `said` | `protocol_order`→close、記録 0。 | `bad_request`、keep、記録 0。 | parser 呼出 0、write 0。 |
| matching bind `ok` | `protocol_order`→close。 | pending を消費し binding を acknowledged にする。 | parser 呼出 0、write 0。 |
| matching bind `err` | `protocol_order`→close。 | code が `bind_failed` なら pending を消費して close。それ以外は `response_invalid`→close。 | parser 呼出 0、write 0。 |
| matching say `ok` | `protocol_order`→close。 | pending を消費し delivery を `delivered` にする。store failure は close + runtime fail-loud。 | parser 呼出 0、write 0。 |
| matching say `err` | `protocol_order`→close。 | code が `external_rejected` なら pending を消費し delivery を `failed` にする。それ以外は `response_invalid`→close。 | parser 呼出 0、write 0。 |
| unknown/malformed/consumed response | `protocol_order`→close。 | `response_invalid`→close。未知 ID 自身の delivery write 0。 | parser 呼出 0、write 0。 |
| gate→core `bind|say|activity` | `protocol_order`→close。 | `unknown_message`、keep、write 0。 | parser 呼出 0、write 0。 |
| unknown `m` | `protocol_order`→close。 | `unknown_message`、keep、write 0。 | parser 呼出 0、write 0。 |
| invalid UTF-8/JSON/non-object/duplicate | `bad_request`→close。 | `bad_request`→close。 | parser 呼出 0、write 0。 |
| oversized frame | `too_large`→close。 | `too_large`→close。 | parser 呼出 0、write 0。 |
| EOF/I/O failure/task panic | close。instance 未同定なら registry write 0。 | `disconnect`→全 pending say を `indeterminate`、authority 条件付き cleanup。 | cleanup 済み。二重 terminal write 0。 |
| hello 10 秒経過 | `protocol_order`→close。 | 発生しない。 | timer 破棄。 |
| bind 60 秒経過 | 発生しない。 | `bind_failed`→close。未 ack binding は未承認のまま。 | timer 破棄。 |

## 5. 敷設 admin

### 5.1 共通規則と Bearer

- 次の 6 operation だけを extgate admin surface とする。全 path は既存 HTTP server 上に置く。
- `Authorization` は exact `Bearer <token>`。token は `OPENCRAB_GATE_OPERATOR_TOKEN` から startup に 1 回読み、直後に `remove_var` し、redacted memory 型だけへ保持する。
- expected token が空または欠落した状態では全 6 operation を拒否する。presented token の欠落、scheme 違い、空、不一致も同じ 401 にする。
- 比較は expected/presented の最大長まで XOR し、長さ差も accumulator に入れる。token 値を argv、file、Debug、log、status、error detail に出さない。
- 401 の body は byte-exact `{"error":{"code":"unauthorized","detail":null}}`、Content-Type は `application/json; charset=utf-8`。
- 401 以外も `{"error":{"code":<code>,"detail":<string|null>}}`。内部 path、SQL、token は detail に入れない。
- request object の duplicate member は `bad_request`。未知 member は無視する。必須 member 欠落、型違い、invalid base64/UUID/digest は `bad_request`。
- validation 順序は Bearer → media/JSON/duplicate → path と必須型 → 参照先 → state conflict → transaction/store。同段は下表の field 順。

### 5.2 DTO

```text
Instance = {instance_id:UUID,kind_id:string,subject_id:positive-i64,revision:positive-u64,
            enabled:boolean,config_b64:string,config_digest:Digest,
            created_at:i64,updated_at:i64,deleted_at:i64|null}
Binding  = {binding_id:UUID,instance_id:UUID,address:string,created_at:i64,closed_at:i64|null}
```

### 5.3 6 operation

GET と DELETE の request body は 0 byte。body があれば `bad_request`。

| # | operation | exact request | success | operation errors |
|---|---|---|---|---|
| 1 | `GET /api/gate-instances/{instance_id}` | body なし | `200 Instance`。deleted row は存在しないものとして扱う。 | `bad_request,instance_unknown,store_error` |
| 2 | `PUT /api/gate-instances/{instance_id}` | `{kind_id,subject_id,enabled,config_b64}` | new `201 Instance`; byte-equivalent existing `200 Instance` | `bad_request,subject_unknown,instance_conflict,store_error` |
| 3 | `DELETE /api/gate-instances/{instance_id}` | body なし | `200 {instance_id,deleted:true}`。同 TX で open binding を close。 | `bad_request,instance_unknown,instance_active,store_error` |
| 4 | `POST /api/gate-instances/{instance_id}/revisions` | `{expected_revision,enabled,config_b64}` | `201 {instance_id,revision,enabled,config_digest}` | `bad_request,instance_unknown,revision_conflict,instance_active,store_error` |
| 5 | `PUT /api/gate-bindings/{binding_id}` | `{instance_id,address}` | new `201 Binding`; byte-equivalent open row `200 Binding` | `bad_request,instance_unknown,instance_disabled,binding_closed,binding_conflict,address_in_use,store_error` |
| 6 | `DELETE /api/gate-bindings/{binding_id}` | body なし | `200 {binding_id,closed:true}`。既に closed なら同じ write-zero response。 | `bad_request,binding_unknown,store_error` |

Instance PUT は revision 1 で作る。既存 ID の比較対象は `kind_id,subject_id,enabled` と base64 decode 後の config bytes。DELETE 済み ID の PUT は `instance_conflict` で復活させない。Revision POST は `expected_revision` 一致時だけ revision を 1 増やす。

### 5.4 stable error 語彙の完全表

この 25 code 以外を admin body、wire `err`、delivery error、runtime close reason に追加しない。

| code | HTTP | producer / 意味 | connection / 永続効果 |
|---|---:|---|---|
| `unauthorized` | 401 | Bearer 欠落・不一致・空 expected | admin write 0 |
| `bad_request` | 400 | JSON、duplicate、必須型、値、未知 enum 不正 | hello/framing は close、running request は keep、write 0 |
| `subject_unknown` | 404 | subject exact-one 解決 0 件 | admin write 0 |
| `instance_unknown` | 404 | instance 無しまたは deleted | write 0 |
| `binding_unknown` | 404 | binding ID 無し | write 0 |
| `instance_conflict` | 409 | Instance PUT の既存値が非同値、または deleted ID 再利用 | write 0 |
| `revision_conflict` | 409 | expected revision 不一致 | write 0 |
| `instance_disabled` | 409 | disabled instance への hello / Binding PUT | write 0、hello は close |
| `binding_closed` | 409 | closed binding の再利用または遅延 said | write 0 |
| `binding_conflict` | 409 | Binding PUT の既存値が非同値 | write 0 |
| `address_in_use` | 409 | 同 instance/address の別 open binding | write 0 |
| `instance_active` | 409 | live 中の revision POST / instance DELETE。wire では二重 live hello | write 0、二重 hello は新 socket close |
| `instance_not_ready` | 409 | bind 未 ack binding の said | 記録 0、keep |
| `store_error` | 500 | DB lock/transaction/query/commit 失敗 | rollback。wire running 中は close |
| `too_large` | — | 1 MiB 超 frame | close、frame 効果 0 |
| `protocol_order` | — | hello 前 message、二回目 hello、hello timeout | close、message 効果 0 |
| `protocol_unsupported` | — | hello protocol が 2 以外 | close、registry 登録 0 |
| `revision_mismatch` | — | hello revision が active revision と不一致 | close、registry 登録 0 |
| `config_digest_mismatch` | — | hello digest が active config と不一致 | close、registry 登録 0 |
| `response_invalid` | — | pending 不一致、未知/消費済み ID、response shape 不正 | close、pending say は indeterminate |
| `unknown_message` | — | running 中の未知 `m` または逆方向 message | keep、write 0 |
| `bind_failed` | — | bind err/write failure/60 秒経過 | close、binding は未承認 |
| `not_connected` | — | reply handoff 前に live/acknowledged binding が無い | reply/delivery TX 0、say 0 |
| `external_rejected` | — | say を外部 API が非受理と確定 | delivery `failed`、再送 0 |
| `disconnect` | — | EOF、I/O failure、protocol close、ack 不明 | pending delivery `indeterminate`、再送 0 |

### 5.5 409 guard と dynamic Binding PUT

revision POST と instance DELETE は registry lock を取り、対象 instance が live なら 409 `instance_active`。live が無い場合も DB commit/rollback まで同じ lock を保持し、同時 hello を割り込ませない。

Binding PUT / DELETE は live 中も実行する。新規 PUT は DB commit を成功応答の境界とし、同じ instance が live なら commit 後に当該 binding の bind を exact 1 回 enqueue する。HTTP 201 は敷設成功を表し、bind acknowledgement を表さない。enqueue/write/ack が失敗した接続は `bind_failed` で close し、binding row は open のまま残す。次の hello が再び bind する。byte-equivalent PUT が既に pending または acknowledged なら新しい bind を作らない。

新規 Binding PUT の同一 transaction は、`session_id_for_binding(binding_id)` を使い、確認済み実関数 `insert_session_in_tx(tx, session_id, address, now_rfc3339)`、`insert_agent_session_in_tx(tx, agent_id, session_id)`、`gate_bindings` insert の順に実行する。session の theme は binding address、membership は subject に exact-one 対応する agent 1 行。3 write の一部失敗は全 rollback + `store_error`。既存 session を別 binding へ流用せず、session/membership を binding row に複製しない。

Binding DELETE は row を close し、live registry の acknowledged/pending set から同じ binding を除く。wire 通知は送らない。既に送信済みの say pending はその応答または接続 close まで決着させ、新規 said と新規 delivery は `binding_closed` で止める。

### 5.6 agent GET の subject_id

既存 `GET /api/agents/{id}` の成功 JSON に positive i64 `subject_id` を追加する。この endpoint に extgate Bearer と extgate error envelopeを広げない。`UNIQUE(subject_id)` を不変条件とし、agent 行または subject 写像が 0 件なら 404、exact 1 件なら 200 とする。

## 6. 永続モデル

### 6.1 v44 migration

ベース `3bdb3aab` からの v44 を次の内容で作り直す。transplant 未マージの旧 v44 SQL と migration test は履歴ごと置換し、v45 を作らない。新規 gate 表は下記 4 表だけ。

既存 `agents` には表を増やさず subject 列を足す。

```sql
ALTER TABLE agents ADD COLUMN subject_id INTEGER;

WITH ranked AS (
  SELECT agent_id, ROW_NUMBER() OVER (ORDER BY agent_id) AS n FROM agents
)
UPDATE agents
SET subject_id = (SELECT n FROM ranked WHERE ranked.agent_id = agents.agent_id);

CREATE UNIQUE INDEX idx_agents_subject_id ON agents(subject_id);

CREATE TRIGGER agents_subject_id_insert_guard
BEFORE INSERT ON agents
WHEN NEW.subject_id IS NOT NULL AND NEW.subject_id <= 0
BEGIN SELECT RAISE(ABORT, 'agents.subject_id must be positive'); END;

CREATE TRIGGER agents_subject_id_assign
AFTER INSERT ON agents
WHEN NEW.subject_id IS NULL
BEGIN
  UPDATE agents
  SET subject_id = (SELECT COALESCE(MAX(subject_id), 0) + 1 FROM agents WHERE agent_id <> NEW.agent_id)
  WHERE agent_id = NEW.agent_id;
END;

CREATE TRIGGER agents_subject_id_update_guard
BEFORE UPDATE OF subject_id ON agents
WHEN NEW.subject_id IS NULL OR NEW.subject_id <= 0
BEGIN SELECT RAISE(ABORT, 'agents.subject_id must be positive'); END;

CREATE TABLE gate_instances (
  instance_id   TEXT PRIMARY KEY,
  kind_id       TEXT NOT NULL CHECK(length(kind_id) > 0),
  subject_id    INTEGER NOT NULL REFERENCES agents(subject_id) ON DELETE RESTRICT,
  revision      INTEGER NOT NULL CHECK(revision > 0),
  enabled       INTEGER NOT NULL CHECK(enabled IN (0,1)),
  config_b64    TEXT NOT NULL,
  config_digest TEXT NOT NULL CHECK(length(config_digest) = 64),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  deleted_at    INTEGER
);

CREATE TABLE gate_bindings (
  binding_id TEXT PRIMARY KEY,
  instance_id TEXT NOT NULL REFERENCES gate_instances(instance_id) ON DELETE RESTRICT,
  address TEXT NOT NULL CHECK(length(address) > 0),
  created_at INTEGER NOT NULL,
  closed_at INTEGER
);

CREATE UNIQUE INDEX idx_gate_bindings_open_address
ON gate_bindings(instance_id, address) WHERE closed_at IS NULL;

CREATE TABLE external_origins (
  binding_id TEXT NOT NULL REFERENCES gate_bindings(binding_id) ON DELETE RESTRICT,
  origin TEXT NOT NULL CHECK(length(origin) > 0),
  seq INTEGER NOT NULL CHECK(seq > 0),
  PRIMARY KEY(binding_id, origin),
  UNIQUE(binding_id, seq)
);

CREATE TABLE deliveries (
  delivery_id TEXT PRIMARY KEY,
  binding_id TEXT NOT NULL REFERENCES gate_bindings(binding_id) ON DELETE RESTRICT,
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  state TEXT NOT NULL CHECK(state IN ('sending','delivered','failed','indeterminate')),
  error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  CHECK(COALESCE(json_type(payload_json), '') = 'object'),
  CHECK(COALESCE(json_type(payload_json, '$.text'), '') = 'text'),
  CHECK(COALESCE(length(json_extract(payload_json, '$.text')), 0) > 0),
  CHECK(
    (state IN ('sending','delivered') AND error IS NULL) OR
    (state = 'failed' AND error = 'external_rejected') OR
    (state = 'indeterminate' AND error IN ('disconnect','stale sending recovered after restart'))
  )
);
```

migration runner は上記全 SQL と `PRAGMA user_version=44` を同一 transaction で適用する。1 statement でも失敗したら全 rollback して startup を失敗させる。

### 6.2 origin transaction

`said` は `BEGIN IMMEDIATE` で次の順に処理する。

1. binding が同 instance に属し open かつ acknowledged であることを確認する。
2. `(binding_id,origin)` を検索する。既存なら transaction を write-zero commit し、保存 seq を返す。
3. `accept_inbound` を実 lookups で exact 1 回呼ぶ。
4. sole agent が admitted されなければ会話記録と origin insert を行わず commit し、`seq:null` を返す。
5. admitted なら `COALESCE(MAX(seq),0)+1` を同 binding の次 seq とし、会話 inbound 記録と origin insert を同じ transaction で行う。
6. commit 後だけ `on_run` が選んだターンを enqueue する。失敗は rollback + `store_error` で、`ok` は返さない。

### 6.3 delivery transaction と say exact 1 回

terminal result は `delivered`、`failed(error='external_rejected')`、`indeterminate` の 3 種。delivery 行に attempt、deadline、接続番号、remote origin を持たせない。

`DeliveryEffect::Text { body, .. }` の処理は次の exact 順序。

1. registry lock 下で instance の live と binding の acknowledged を得る。無ければ `not_connected`、reply/delivery write 0、say 0。
2. `BEGIN IMMEDIATE` で binding が open かつ同じ instance であることを再検査する。不成立は `binding_closed`、全 rollback、say 0。
3. 既存 `insert_session_log(&tx, SessionLogRow{log_type:"speech", source:TranscriptSource::External.reply(), ...})` による reply 記録と、`deliveries(state='sending',payload_json='{"text":body}')` insert を同じ transaction で行う。
4. commit 前は wire write 0。commit 失敗は両方 rollbackし `store_error`。
5. commit 後、同じ live の pending map へ `delivery_id` を先に登録し、say frame を socket writerへ 1 回だけ渡す。enqueue または write 失敗は delivery を `indeterminate(error='disconnect')` にして close。
6. matching `ok` は `sending→delivered`、matching `err(external_rejected)` は `sending→failed` を各 1 transaction で行い、pending を消す。
7. response 不一致または socket close は、その接続の全 pending を 1 transaction で `sending→indeterminate(error='disconnect')` にし、pending を空にする。
8. terminal 行への後続 response は pending 不在なので `response_invalid`。state を上書きしない。

`DeliveryEffect::NoReply|Empty|Failed` は say と delivery を作らない。Text の body が空なら `bad_request` を内部契約違反として runtime fail-loud にし、reply/delivery/say を 0 にする。delivery の自動再送、別 instance handoff、再接続後 handoff は全て 0 回。

### 6.4 startup recover

schema/migration 完了直後、HTTP bind と UDS listen より前に exact 1 回、次を `BEGIN IMMEDIATE` で実行する。

```sql
UPDATE deliveries
SET state='indeterminate',
    error='stale sending recovered after restart',
    updated_at=:startup_time
WHERE state='sending';
```

更新失敗は rollback + startup failure。対象 0 件は write 0 の成功。recover 後に残存 delivery を enqueue せず、startup で復元する process-local 接続情報は無い。

## 7. core 接続

### 7.1 確認済み実名

READ-ONLY で `/Volumes/2TB/openclaw/.claude-scratch/opencrab/worktrees/wt-transplant` の実コードを確認した。次の名前をそのまま使い、同じ意味の stub や別入口を作らない。

```rust
pub fn accept_inbound<T: Send + 'static>(
    items: &[InboundWork<'_>],
    owner_id: &str,
    agent_ids: &[String],
    lookups: &InboundLookups<'_>,
    watch: Option<WatchAccept<'_, T>>,
    take_hold: impl FnMut(usize) -> T,
    on_admitted: impl FnMut(usize, &AdmittedInbound),
    on_run: impl FnMut(usize, &AdmittedInbound, &[usize]),
) -> Result<(), InboundDrop>;

InboundLookups {
    resolve_caller,
    dm_allowed_any,
    dm_allowed,
    channel_whitelisted,
}
```

同じファイルの実名は `NormalizedInboundEvent`、`InboundWork`、`NormalizedInbound`、`AdmittedInbound`、`prepare_session_inbound_write`、`start_session_turn`、`run_session_turn`、`DeliveryEffect`、`delivery_effect`。`DeliveryEffect` は `Text { body, stopped_by_limit, tool_calls_made, iterations } | NoReply | Empty | Failed { error }`。

server 側で確認済みの実名は `resolve_caller_identity_with_owner`、`TRUSTED_PLATFORM_EXTGATE`、`TranscriptSource::External`、`session_id_for_binding`、`AppState.session_locks.spawn_serialized`、`RunRequest::with_image_urls`、`insert_session_log`。

### 7.2 said から accept_inbound への exact 写像

1 said につき `InboundWork` 1 件を作る。

```text
NormalizedInboundEvent.sender_id = said.author_id
NormalizedInboundEvent.channel_id = binding.address
NormalizedInboundEvent.guild_id = instance.kind_id
InboundWork.has_content = !said.text.is_empty() || !said.attachments.is_empty()
InboundWork.kind_label = "said"
InboundWork.author_key = said.author_id
owner_id = sole agent の Discord config `owner_discord_id`; 未設定は ""
agent_ids = [subject_id に exact-one 対応する agents.agent_id]
watch = None
take_hold = |_| ()
```

origin processor は DB mutex を 1 回取得して §6.2 の transaction を開始し、4 lookup closure と記録 closure は同じ transaction connection を capture する。closure 内で DB mutex を再取得しない。mutex 取得失敗は `store_error` で `accept_inbound` を呼ばない。`resolve_caller` は `resolve_caller_identity_with_owner(tx, TRUSTED_PLATFORM_EXTGATE, &[sender], agent_id, owner_id)` を呼び、query failure は `CallerIdentity::Agent`。`dm_allowed_any` / `dm_allowed` は owner 一致または `is_trusted_user(tx, TRUSTED_PLATFORM_EXTGATE, sender, agent_id)`、query failure は false。`channel_whitelisted` は当該 agent/instance/address の open binding exact 1 行だけ true、query failure は false。

`on_admitted` は sole agent が `admitted_agent_ids` に入った場合だけ `NormalizedInbound` を会話へ記録する。`NormalizedInbound` は `session_id_for_binding`、`sender_id=author_id`、`sender_name=""`、`channel_id=Some(address)`、`text=said.text`、`image_urls=https URL list`、`external_id=origin`。record failure は origin transaction 全体を失敗させる。

`on_run` は空 closure にしない。callback の `AdmittedInbound.admitted_agent_ids` に sole agent が含まれ、`on_admitted` の記録が成功した場合だけ、選ばれた index と read indices を保持する。origin transaction commit 後に `AppState.session_locks.spawn_serialized` へ投入する。task 内で `start_session_turn(..., TranscriptSource::External, ...)` を呼び、`RunRequest` へ `with_image_urls` で添付 URL を渡す。権限、trust、whitelist、ターン起動を gateway へ移さない。

### 7.3 実在 Discord/web 配線との関係

- Discord の `crates/discord/src/message_loop.rs` は `state.resolve_caller`、`state.dm_allowed_any`、`state.dm_allowed`、`state.is_channel_whitelisted_for_agent` を `InboundLookups` に渡し、`accept_inbound` の `on_admitted` と `on_run` で plan/run/read を分ける。後段は `start_session_turn(..., TranscriptSource::Discord, ...)` と `delivery_effect`。external はこの fail-closed 構造を踏襲する。
- web の `crates/web-gateway/src/http.rs::send_web_message` は `WEB_INBOUND_GUILD` を使い、3 permission lookup を常時 true、`on_run` を空にする。後段は `prepare_session_inbound_write` と `run_and_deliver_serialized` → `run_session_turn` → `delivery_effect`。external はこの常時許可配線と空 `on_run` を流用しない。
- 現 HEAD の `crates/server/src/main.rs` は `ExternalGateRuntime::prepare(..., String::new())` を呼んでおり socket を起動しない。V3 実装では §3.1 の新設設定値を渡す。既存設定 struct 内の同名 field はコード上未確認である。

### 7.4 DeliveryEffect 写像

| DeliveryEffect | external 動作 |
|---|---|
| `Text { body, .. }` | §6.3 の reply + sending 同一 TX、その後 say 1 回。 |
| `NoReply` | `record_agent_no_reply`。say 0、delivery 0。 |
| `Empty` | say 0、delivery 0。 |
| `Failed { error }` | error を token/API body に出さず server log。say 0、delivery 0。 |

## 8. 非目標

次は第1段で作らない。入力または実装依存が現れたら成功扱い、空 stub、固定値で埋めず、V3 の変更要求として fail-loud に止める。

- `catch_up`、cursor、checkpoint、履歴 read、履歴 page、single-flight を作らない。
- kind カタログ、schema 台帳、secret manifest、capability/effect 宣言、address form を作らない。
- `connection_epoch`、永続 counter、`gate_connections`、接続状態の admin 投影を作らない。
- `gate_routes`、purpose 行、binding 上の subject/session/place 複製を作らない。
- revision coordinator、`revision_invalidated`、失効 snapshot、専用 close cause を作らない。
- delivery の deadline、timeout worker、16 セル outcome 行列、attempt 列、observation、late/wrong 接続応答監査を作らない。
- 自動再送、別 instance fallback、manual retry API、delivery status API を作らない。
- `edited`、`retracted`、`reacted`、`ui_action`、`ready`、`failed`、`unbind`、`place_closed`、`source_checkpoint`、`read` message を作らない。
- activity の progress、kind、label、background と activity delivery を作らない。
- event の mentions、reply target、address 重複 field、say の kind/address、成功時 external origin 保存を作らない。
- unknown field 拒否、JSON field order 固定、UUIDv7 制限、全 payload schema 検証を作らない。
- core に external process の spawn/restart/stop、platform API、secret 配布を持たせない。
- 旧 REST 会話経路を再導入しない。hooks/intake を代替会話口にしない。
- 組み込み Discord/web/voice を別プロセス化しない。external E2E 緑を第1段の完成とする。
- TCP、stdio、shared instance、片方向 binding、tool/action、timed route、画像以外の添付を作らない。

## 9. 検収

conformance suite は少なくとも次を自動検証する。

### 9.1 framing / grammar

- 1,048,576 byte frame 成功、1,048,577 byte と LF 未着上限超過が `too_large` + close。
- invalid UTF-8、JSON、非 object、top-level/nested/payload/attachment duplicate が `bad_request` + close。
- hello/said の unknown field を core が無視し、say payload の unknown member を gateway が無視する。
- 必須 field 欠落、型違い、未知 enum が成功せず、hello と running の close/keep 差が表どおり。
- hello 前 said/response/逆方向 message と二回目 hello が `protocol_order` + close。
- pending 未知、ID 不一致、shape 違い、消費済み response が `response_invalid` + close。
- hello 10 秒、bind 60 秒、bind err/write failure の結果が表どおり。

### 9.2 hello / binding / admin

- protocol、instance、enabled、revision、digest、二重 live hello の各失敗と registry 登録 0。
- startup/restart の registry が空で、DB から接続情報を復元しない。
- open binding だけを hello 後に bind し、matching ack 後だけ said を受理する。
- dynamic Binding PUT 中も既存 acknowledged binding の said が通り、新 binding は ack 前 `instance_not_ready`。
- live 中の revision POST と instance DELETE は 409 `instance_active`、DB write 0。同時 hello と race して両方成功しない。
- live 中の Binding PUT/DELETE は成功し、DELETE 後の said と新規 delivery は止まる。
- 6 operation 以外の旧 extgate admin path が router に存在しない。
- Instance/Binding PUT の同値 200、非同値 conflict、open address unique、closed ID 非復活。
- `GET /api/agents/{id}` の subject exact 0/1 が 404/200。成功 JSON に positive i64 が入る。

### 9.3 Bearer

- token env を読んだ直後に process env から消し、Debug/log/error に値が出ない。
- expected token 空、header 欠落、scheme 違い、presented 空、短い/長い/同長不一致が全て byte-exact 401 一形。
- equal token だけ 6 operation へ到達する。比較 loop が最大長回実行され、長さによる早期 return が無い。
- extgate 外の既存 endpoint へこの Bearer を広げない。

### 9.4 dedup / core admission

- 同 binding/origin の再送が初回 seq を返し、`accept_inbound`、会話記録、turn、say を増やさない。
- 別 binding の同じ origin は別 seq=1 から始まる。
- admission で非記録の said だけ `seq:null`。store/validation error は `err`。
- Discord 型の実 lookups が呼ばれ、DB failure は最小権限/false。常時 true stub と空 `on_run` を mutation test で落とす。
- text-only と image-only said が `NormalizedInbound` へ写り、空 text + 空 attachments は記録 0。
- admitted said 1 件が `accept_inbound` exact 1 回、commit 後の `start_session_turn` exact 1 回になる。

### 9.5 delivery 3 結果

- reply log と `sending` delivery の片方を failure injection すると双方 rollback、say 0。
- DB commit 前 say 0、commit 後 socket write exact 1。enqueue/write failure後の再送 0。
- payload が exact `{"text":body}`、text nonempty。NoReply/Empty/Failed/空 body は say 0。
- matching ok が `delivered`、matching external rejection が `failed/external_rejected`、close/response-invalid が `indeterminate/disconnect`。
- ack 前 disconnect と ack 後 DB commit 前 crash を成功または確定拒否へ捏造しない。
- startup は全 `sending` だけを `indeterminate` + exact stale reason に更新し、listener より前に完了する。失敗時は HTTP/UDS 0。
- terminal response 2 件目は `response_invalid` で state 不変。delivery の自動再送 0。
- activity は started/ended だけで response/delivery 0。activity failure が say 件数を変えない。

### 9.6 migration / E2E

- `3bdb3aab` の user_version 43 相当 DB と fresh DB の双方が v44 へ到達し、新規 gate 表が exact 4 表。
- 4 表の PK/FK/CHECK/partial unique、agents subject backfill/自動採番/unique/positive guard を検証する。
- migration 中の各 statement failure が全 rollback し、user_version 43 のまま起動失敗になる。
- omoikane と instance PUT → Binding PUT → hello/bind → said → activity started/ended → say/ok の相互 E2E を緑にする。
- 切断 E2E で pending say が indeterminate、再接続後の同 delivery say が 0 件である。

## 10. omoikane 差分

八意への再交渉は次を一括変更として提示する。

| 旧契約 / G1–G4 前提 | V3 |
|---|---|
| 7 path / 14 operation | instance 4 + binding 2 の 6 operation。Binding GET と schema/kind/cursor 系 API は削除。 |
| admin object の unknown member 拒否、field order 固定 | duplicate は拒否、unknown は無視、field order は非意味。Bearer 401 は一形を維持。 |
| hello の種別宣言、origin/address 規則、effects/capabilities | hello は protocol/instance/revision/config digest だけ。 |
| hello 後の ready と多段 readiness | hello 成功直後 RUNNING。binding ごとの ack 集合だけを持つ。 |
| `event` + kind=`said` | direct `m="said"`。他 event 種は wire から削除。 |
| `effect` + kind=`say` + address | direct `m="say"`、binding_id、payload だけ。payload は `{"text":body}`、text nonempty。 |
| success response の delivered/origin、false branch | `ok` は配送成功だけ。外部 origin は返さない。確定非受理は `err(external_rejected)`。 |
| response に `m` 無し | response は `m="ok"|"err"` を必須にする。pending 種と shape を照合する。 |
| activity started/progress/ended + address/kind/label | binding_id/activity_id と started/ended だけ。response と delivery は作らない。 |
| origin 重複を null と解釈する G1 | 重複は初回 seq。null は core が記録しなかった said だけ。 |
| 接続情報を DB と admin GET に投影 | 接続は process memory だけ。restart は空。Instance GET に接続 field を返さない。 |
| 多数の gate 補助表 | gate_instances、gate_bindings、external_origins、deliveries の 4 表。subject は agents 列。 |
| revision 履歴/tombstone 行の追加 | active revision/config/digest/enabled/deleted_at を instance 1 行に保持。 |
| live revision/delete が旧 socket を失効 | 両操作を 409 で拒否。「gateway 停止→更新/削除→起動」が唯一の運用。 |
| live 中の全敷設変更を止める解釈 | Binding PUT/DELETE は live 中も成功し、PUT は binding 単位で bind する。 |
| prepared/sending と配送試行回数、期限、監査行 | reply と sending を同一 TX。say 1 回。成功/確定拒否/切断不明だけを残す。 |
| gateway failure code の自由文字列 | bind は `bind_failed`、say は `external_rejected`。detail は nullable だが code を増やさない。 |
| 添付の author 補助情報 | `Attachment={kind:"image",url:https}` だけ。core が `with_image_urls` へ渡す。 |
| subject 解決口の約束 | 既存 agent GET に subject_id を追加し、0 件を 404、1 件を 200 にする。`UNIQUE(subject_id)` により複数は到達不能。 |
| hello/ready/failed の `connection_epoch` | wire から削除。応答照合は同一 socket の pending だけで行い、epoch は存在しない。G1 の epoch 送出を削除する。 |
| catch-up wire（`catch_up` / `source_checkpoint` と CAS・single-flight） | wire・admin とも削除。過去分の再取得は行わない（gateway 内部のリプレイ機構は本契約の関知外）。 |
| 旧会話 HTTP との並走案 | 旧会話 HTTP は再導入しない。external UDS E2E を完成条件にする。 |
| 組み込みゲートも同じ process 境界へ移す案 | Discord/web/voice は現行 process 配置を維持する。 |

再交渉の合格は、omoikane 側の wire/admin client と本体 conformance の双方が本書の field、response `m`、同seq、6 operation、3 delivery result を同じ fixture で通すことである。
