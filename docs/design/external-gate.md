# External gate（gate socket）の取り決め

本書は core と gateway の間の取り決め（protocol 3）の正本である。実装は `crates/extgate/`（core 側）と `crates/gate-client/`（Rust の gateway 側）。両者が食い違ったらコードを正とし、本書を直す。

変更履歴: D-1099 で `provision`・`create_binding`・`bind` と、HTTP の敷設 admin（Bearer）を削除した。core は gateway の設定も binding も持たず、`hello` と会話のキーだけで動く。それより前の本文は git の履歴にある。D-1134 で道具の schema の印を `format` から `x-opencrab-*` に移した。

## 1. 役割

- **core** — gate socket を listen し、会話（セッション、記録、記憶、ターン、道具の実行、定期実行、モデル設定）を持つ。
- **gateway** — gate socket に connect し、外部サービスとの入出力を持つ別プロセス。接続先、bot、オーナーと権限（caller）、どの場所を聞くか、場所と会話のキーの対応、表示、設定画面は gateway が持つ。
- 資格情報は無い。gate socket を開けることだけが接続の条件。

## 2. 線

- core は設定 `[gate] listen_socket` の Unix domain socket を 1 本 listen する。空なら listen しない。相対パスと、既存の socket でないファイルは起動失敗。既存の socket は消してから listen する（`validate_listen_socket`）。socket のモードは `0660`。
- frame は UTF-8 の JSON object 1 個と LF。LF を含めて 1,048,576 byte 以下（`MAX_FRAME`）。超えたら `too_large` で close。
- 不正な UTF-8 / JSON、object 以外、重複したキーは `bad_request` で close。
- 知らない field は無視する。
- request の `id` は 1..128 byte の文字列。応答は同じ `id` の `ok` か `err`。`err` は `{id, m:"err", code, detail}`（`detail` は文字列か null）。
- `instance_id`・`provisional_id`・添付の `id` は小文字の正規形 UUID。

## 3. 接続の順番

1. 接続から 10 秒以内に `hello` を送る。来なければ `protocol_order` で close。
2. `hello` より前の他の frame は `protocol_order` で close。`hello` を 2 回送っても `protocol_order`。
3. `hello` が通ると `ok{id}` が返り、接続は動作中になる。同じ instance の接続は 1 本だけ。

### hello

`{id, m:"hello", protocol:3, instance_id, agent_id, operation_protocol:1, final_delivery, operations, run_env?}`

- `protocol` が 3、`operation_protocol` が 1 でなければ `protocol_unsupported`。
- instance が無ければ、`agent_id` のエージェントの instance として作る。別のエージェントの instance なら `instance_conflict`。知らないエージェントは `subject_unknown`。同じ instance がすでに接続中なら `instance_active`。
- `final_delivery` は `automatic` か `operation_driven`。`operation_driven` のときは、`dispatch=utterance` の道具が 1 つ以上要る。
- `operations` は道具の宣言の配列（§6）。
- `run_env`（任意）は、この instance の `execute_shell` の子プロセスに渡す環境変数（名前→文字列、最大 64 個、名前 128 byte・値 8192 byte まで、制御文字は不可）。core は中身を解釈しない。
- 失敗したら `err` を返して close する。

## 4. gateway → core

どの frame も `session`（gateway が決める会話のキー）を持つ。空でない、512 byte 以下、制御文字なし。core は形式を解釈しない。core は (instance, session) を自分の記録（`gate_bindings`）に解決する。既存の記録があればその会話、無ければ新しい会話を作る。前もって登録するものは無い（`crates/db/src/queries/gate_session.rs`）。

| `m` | 主な field | 応答 |
|---|---|---|
| `open` | `id, session, title?` | `ok{id, session_id}`。新しいキーなら会話を作る。何度送ってもよい |
| `said` | `id, session, origin, author_id, text, attachments` と任意の field | `ok{id, seq, session_id}` |
| `provisional` | `id, session, provisional_id, author_id, caller, model_profile:"realtime", text`、任意 `author_label, system_context, agent_prelude` | `ok{id}`。何も記録しない |
| `provisional_cancel` | `id, session, provisional_id` | `ok{id}`。何度送ってもよい |
| `interrupt` | `id, session, delivered?` | `ok{id}`。走っているターンを止める |
| `command` | `id, session, caller, name, args` | `ok{id, result}` か `err{id, code, message}` |
| `ok` / `err` | core からの `say` / `invoke` への応答 | — |

### said

- `origin` は同じ外部の発言に毎回付ける空でない文字列。同じ会話で同じ `origin` は 1 回だけ記録され、2 回目以降は前の `seq` を返す。
- `seq` は会話ごとに 1 から振る番号。ターン待ちの列（会話ごとに 32 件）が満ちていたら記録せず `seq:null` を返す。
- 任意の field: `ref`（ほかの発言がこの発言を指すときのキー）、`author_label`（表示名、100 文字まで）、`caller`（`{role:"owner"|"agent"|"trusted_user"}` か `{role:"co_agent", agent_id}`、省略は `agent`）、`start_turn`（false なら記録だけ）、`system_context`（16,384 byte まで）、`reply_target`（この発言が指している相手のキー）、`live_inbound_scope`（`all` / `speaker`）、`model_profile`（`default` / `realtime`）、`agent_prelude`（この返事より前にエージェントがすでに話した言葉、64 文字まで）、`provisional_id`。
- `attachments` は必須の配列（無ければ `[]`）。要素は `{kind:"image", url}`（https の URL）か、`{kind:"file", id, name, media_type?, size, sha256, local_path}`（`local_path` は受信用の inbox root からの相対パス）。`text` と `attachments` が両方空なら `bad_request`。
- caller の判断は gateway の仕事。core は渡された caller をそのまま使う。

### provisional（先行生成）

確定前の realtime の発話について、先に返事の生成を始める。会話ごとに 4 件まで、10 秒を過ぎたものは中止する（`crates/extgate/src/provisional.rs`）。同じ `provisional_id` を持つ `said` の中身が一致すればその生成を使い（`promoted:true`）、そうでなければ中止して普通にターンを走らせる。どちらでも、core はターンの `say` より前に `provisional_result` を送る。

### command

オーナー（`caller.role:"owner"`）だけが使える。それ以外は `forbidden`。名前は `list_models`・`set_model`・`reset_model`・`list_session_models`・`set_session_model`・`reset_session_model`・`list_session_reasoning_efforts`・`set_session_reasoning_effort`・`reset_session_reasoning_effort`（`crates/extgate/src/commands.rs`）。会話のモデルを変えるのは `set_session_model`（`/model`）で、エージェントの `select_llm` も同じ設定を書き換える。

## 5. core → gateway

| `m` | 主な field | gateway の応答 |
|---|---|---|
| `say` | `id, session, payload{text, stream_id?}` | `ok{id}` か `err{id, code}` |
| `invoke` | `id, session, invocation_protocol:1, declaration_digest, operation, dispatch, effect, context{continuation_id}, payload` | `ok{id, result}` か `err{id, code}` |
| `partial` | `session, stream_id, text, provisional_id?` | 応答しない |
| `provisional_result` | `session, provisional_id, promoted` | 応答しない |
| `activity` | `session, activity_id, state, origin?, completed_target?`、`ended` では `silent_origins` | 応答しない |
| `tool_activity` | `session, activity_id, call_id, tool, status, depth, started_at, args`、任意 `sub_session_id, duration_ms, result, error` | 応答しない |
| `subtask_lifecycle` | `session, run_id, status`、任意 `sub_session_id, label, task, detail, duration_ms, result_text` | 応答しない |
| `turn_failed` | `session, origin` | 応答しない |

- `say` は、返信先を持たない地の文の発話。返信は、エージェントが返信の道具を呼んだときだけ `invoke` で起きる。
- `say` の結果: `ok`（`seq` を付けない）→ 届いた、`err{code:"external_rejected"}` → 外部に断られた、それ以外や接続切れ → 不明（`indeterminate`）。core は自動で送り直さない。
- id の無い通知（`partial` ほか）を知らない gateway は、黙って無視すればよい。
- 送るのは、その instance が接続中のときだけ。

## 6. 道具（operation）の宣言

`hello` の `operations` は道具の配列（`name` の昇順・重複なし、最大 256 件）。

- 各要素: `name`（`[A-Za-z][A-Za-z0-9_.-]`、128 byte まで）、`description`、`input_schema`、`output_schema` と `callback_schema`（null 可）、`authorization.allowed_callers`（`owner` / `co_agent` / `trusted` / `guest` の昇順）、`dispatch`（`inline` / `background` / `utterance`）、`sub_engine`（`not_exposed` / `blocked` / `allowed`）、`sharing`（`agent_bound` / `conversation_bound`）、`effect`（`read_only` / `state_change` / `utterance`）。
- `dispatch=utterance` と `effect=utterance` は、両方そろうときだけ有効。
- core の組み込みの道具と同じ名前は `bad_request`（`is_reserved_tool_name`）。それ以外の宣言の誤りは `operation_declaration_invalid`（`crates/extgate/src/operations.rs`）。

schema に使えるキーは `type`・`required`・`properties`・`enum`・`items`・`description`・`format` と、core 用の印 2 つだけ（`ALLOWED_SCHEMA_KEYWORDS`）。深さ 32・ノード 1024 まで。

- `"x-opencrab-short-ref": true` — top-level の欄。core が会話の短縮参照（`e12` など）を実際の ID に置き換えてから `invoke` に載せる（`crates/extgate/src/ops_projection.rs`）。
- `"x-opencrab-workspace-file": true` — top-level の string 欄、または array 欄の `items`。core がエージェントのワークスペースのファイルを outbox にコピーし、`{id, name, media_type, size, sha256, local_path}`（`local_path` は outbox root からの相対パス）に置き換えて渡す（`crates/extgate/src/outbound_files.rs`、[design-generic-outbound-attachments.md](../design-generic-outbound-attachments.md)）。

印の値は boolean。`format` には JSON Schema 標準の値だけを書く。未知の `format` は LLM プロバイダーが道具の定義ごと拒否する（`invalid_json_schema`）。core は道具の schema をそのまま LLM に渡す。

## 7. error code

`ErrorCode`（`crates/extgate/src/error.rs`）。今の wire で core が返すのは主に次のもの。

| code | いつ |
|---|---|
| `bad_request` | frame の形の誤り、組み込みの道具と同じ名前 |
| `too_large` | frame が上限を超えた |
| `protocol_order` | `hello` より前の frame、2 回目の `hello`、10 秒以内に `hello` が来ない |
| `protocol_unsupported` | `protocol` / `operation_protocol` が違う |
| `subject_unknown` | `hello` の `agent_id` を知らない |
| `instance_conflict` | instance が別のエージェントのもの |
| `instance_active` | 同じ instance がすでに接続中 |
| `instance_unknown` | instance が無い、または消されている |
| `store_error` | core の DB の失敗（close する） |
| `unknown_message` | 知らない `m`（id があるときだけ応答し、接続は続ける） |
| `response_invalid` | 待っていない `id` への応答、`say` への `ok` に `seq` が付いている |
| `operation_declaration_invalid` | 道具の宣言の誤り |
| `external_rejected` | gateway が `say` に返す「外部に断られた」（core は配送を `failed` にする） |
| `disconnect` | 接続が切れた |

列挙には、削除した敷設の仕組みの code（`unauthorized`・`binding_unknown`・`bind_failed`・`revision_*`・`config_digest_mismatch` など）も残っている。今の wire では使わない。

## 8. Rust の gateway 側

`crates/gate-client` が `hello` の組み立て、frame の読み書き、`say` / `invoke` への応答を持つ。接続が切れたら 200 ms から 8 秒まで間隔を延ばしながらつなぎ直し、`hello` から始め直す（`crates/gate-client/src/client/transport.rs`）。core が再起動しても gateway は止めなくてよい。
