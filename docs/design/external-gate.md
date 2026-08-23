# 外部 gateway kind 契約

この文書はexternal gatewayを任意の言語で実装するための自己完結した正本である。例は合成識別子だけを使う。[gateway overview](./gateway-overview.md)、[Discord gate](./discord-gate.md)、[Nostr gate](./nostr-gate.md) は背景とbuiltin例であり、external admin DTOやwire grammarの正本ではない。

## 1. 初期surfaceと所有境界

初期形は protocol 2 / prebound / external container / one instance-one subject / one conversation-one place / literal `['inbound','outbound']` / all immediate / hello `effects:['say']` / `capabilities:['open']` だけである。

- core/store: schema、kind、instance、revision、place、binding、membership、route、origin dedup、cursor CAS、admission、turn、delivery、delivery observation、deadline、startup recovery、control outcomeのtotal写像、revision invalidation outcomeの生成。
- plugd: connection epoch、LF-JSON parse/render、ordered bind/catch-up、socket close、opaque connection Arc authority付きwire outcome返却。delivery tableを直接読書きせず、`revision_invalidated` outcomeを生成しない。
- gateway: external API I/O、protocol変換、platform固有pagination/display。
- orchestrator: container/PID/restart、revision directory、config/secret file/mount/cleanup。

gatewayはDBを開かず、誰が考えるか、返答可否、debounce、retry、routeを決めない。coreはcontainerをspawn/restart/stopせず、platform固有配送を持たない。

## 2. 永続model

`gate_schemas`、`gate_kinds`、`gate_instances`、`gate_instance_revisions`、`gate_bindings`、`external_origin_scopes`、`gate_source_cursors`、`deliveries`、`delivery_observations` はstore-owned。`gate_connections` だけはplugd-owned observed epochで、設定ではない。

external instanceはnon-NULL immutable sole `owner_subject_id`を持つ。作成時 `active_revision=1,lifecycle=stopped`。external lifecycleは以後もstoppedで、readinessはactive revisionに属するlatest connection epochだけから投影する。external revisionはconfig/present/enabledを持ち `secret_set_id=NULL`。secret valueとprocess stateを持たない。

core/store startupはschema作成・migration完了後、runtime listener、hello受付、timer、plugd handoffを開始する前に `RecoverStaleRuntimeState` をexact一回完了する。このcommandが失敗したstartupはruntime portを開かず失敗する。

## 3. admin wire正本

全pathは既存operator認証内、JSON media typeは `application/json; charset=utf-8`。objectはunknown memberとduplicate memberを拒否する。UUIDはcanonical lowercase、binaryはRFC 4648 standard padded base64、digestは64 lowerhex。object field orderは以下の記載順、arrayは明示した順でserializeする。

全errorは、未認証を含め必ず次の一形である。`at` / `detail` は値が無くてもmemberを省略せずnullにする。

```json
{"error":{"code":"unauthorized","at":null,"detail":null}}
```

HTTP statusは次のtotal mapだけを使う。

| status | stable code |
|---|---|
| 401 | `unauthorized` |
| 400 | `bad_request`, `bad_schema_id`, `bad_kind_id`, `bad_instance_id`, `bad_binding_id`, `unknown_field`, `path_body_mismatch` |
| 404 | `schema_unknown`, `kind_unknown`, `subject_unknown`, `instance_unknown`, `binding_unknown` |
| 409 | `schema_conflict`, `kind_in_use`, `builtin_reserved`, `instance_conflict`, `instance_deleted`, `revision_conflict`, `instance_disabled`, `binding_closed`, `binding_conflict`, `address_in_use`, `instance_active`, `instance_not_ready`, `epoch_mismatch`, `catch_up_in_progress` |
| 422 | `schema_validation_failed`, `schema_role_mismatch`, `address_form_invalid`, `address_invalid`, `catch_up_contract_invalid`, `cursor_invalid`, `catch_up_unsupported` |
| 500 | `store_error` |

validation priorityは authentication → media/JSON object/duplicate/unknown shape → path/query grammar → referenced row existence → immutable/state conflict → schema/value validation → transaction/store。同段はrequest field順で最初の一件だけを返す。

### 3.1 exact DTO

```text
Schema = {schema_id:string,role:instance_config|binding_metadata|secret_manifest|source_cursor,
          format:json-schema-2020-12|secret-manifest-v1|opaque-cursor-v1,
          document_b64:B64,document_digest:Digest,created_at:i64}

Kind = {kind_id:string,registration:builtin|external,protocol_major:u32,
        origin_scope:instance|kind_address,ingress_discovery:prebound|membership,
        address_form:string|null,config_schema_id:string|null,
        binding_metadata_schema_id:string|null,secret_manifest_schema_id:string|null,
        catch_up_mode:none|cursor|null,cursor_schema_id:string|null}

Connection = {state:stopped|connecting|active|closed|failed,revision:u64|null,epoch:u64|null}
SecretManifest = {required:string[],optional:string[]}  // each byte-sorted/distinct
Instance = {instance_id:UUID,kind_id:string,label:string,subject_id:positive-i64,
            active_revision:u64,present:boolean,enabled:boolean,config_b64:B64,
            config_digest:Digest,created_at:i64,connection:Connection,
            secret_manifest:SecretManifest}

CatchUpStart = null | {mode:"now"} | {mode:"beginning"} |
               {mode:"supplied",cursor_b64:B64}          // request
StoredStart  = null | {mode:"now"} | {mode:"beginning"} | {mode:"supplied"} // response
Binding = {binding_id:UUID,instance_id:UUID,address:string,label:string|null,
           binding_metadata_b64:B64,purposes:["inbound","outbound"],
           catch_up_start:StoredStart,place_public_key:string,subject_id:positive-i64,
           closed_at:i64|null,close_reason:string|null,cursor_digest:Digest|null}

Cursor = {binding_id:UUID,initial_mode:now|beginning|supplied,initialized:boolean,
          cursor_b64:B64|null,cursor_digest:Digest|null,size:u32,updated_at:i64|null}
```

`Connection` はactive revisionの最大epochを選ぶ。row無しはexact `{state:"stopped",revision:null,epoch:null}`。row有りはその `connecting|active|closed|failed` とnon-null revision/epoch。`Binding.closed_at` / `close_reason` は同時nullまたは同時non-null。cursor row無しは `initialized=false,cursor_b64=null,cursor_digest=null,size=0,updated_at=null`。row有りはdigest/positive size/updated_atがnon-nullで、`include_bytes=false` の時だけbytesをnullへredactする。

### 3.2 7 path / 14 operation

| operation | exact request | success | operation stable errors（共通 `unauthorized,bad_request,store_error` を含む） |
|---|---|---|---|
| `GET /api/gate-schemas/{schema_id}` | bodyなし | `200 Schema` | `bad_schema_id,schema_unknown` |
| `PUT /api/gate-schemas/{schema_id}` | `{role,format,document_b64}` | new `201 Schema`; byte同値 `200 Schema` | `bad_schema_id,unknown_field,schema_validation_failed,schema_conflict` |
| `GET /api/gate-kinds/{kind_id}` | bodyなし | `200 Kind` | `bad_kind_id,kind_unknown` |
| `PUT /api/gate-kinds/{kind_id}` | `{protocol_major:2,origin_scope,ingress_discovery:"prebound",address_form,config_schema_id,binding_metadata_schema_id,secret_manifest_schema_id,catch_up_mode,cursor_schema_id}` | new `201 Kind`; equivalent/pre-use replace `200 Kind` | `bad_kind_id,unknown_field,schema_unknown,schema_role_mismatch,address_form_invalid,catch_up_contract_invalid,builtin_reserved,kind_in_use` |
| `GET /api/gate-instances/{instance_id}` | bodyなし | `200 Instance` | `bad_instance_id,instance_unknown` |
| `PUT /api/gate-instances/{instance_id}` | path UUIDv7; `{kind_id,label,subject_id,enabled,config_b64}` | new `201 Instance`; aggregate同値 `200 Instance` | `bad_instance_id,unknown_field,kind_unknown,subject_unknown,instance_conflict,schema_validation_failed` |
| `DELETE /api/gate-instances/{instance_id}` | bodyなし | `200 {instance_id,deleted:boolean,revision:u64}` | `bad_instance_id,instance_unknown` |
| `POST /api/gate-instances/{instance_id}/revisions` | `{expected_active_revision,enabled,config_b64}` | `201 {instance_id,revision,config_digest,enabled}` | `bad_instance_id,unknown_field,instance_unknown,instance_deleted,revision_conflict,schema_validation_failed` |
| `GET /api/gate-bindings/{binding_id}` | bodyなし | `200 Binding` | `bad_binding_id,binding_unknown` |
| `PUT /api/gate-bindings/{binding_id}` | path UUIDv7; `{instance_id,address,label,binding_metadata_b64,catch_up_start}` | new `201 Binding`; byte同値 `200 Binding` | `bad_binding_id,unknown_field,instance_unknown,instance_disabled,address_invalid,schema_validation_failed,catch_up_contract_invalid,binding_closed,binding_conflict,address_in_use` |
| `DELETE /api/gate-bindings/{binding_id}` | bodyなし | `200 {binding_id,closed:true}` | `bad_binding_id,binding_unknown` |
| `GET /api/gate-bindings/{binding_id}/source-cursor?include_bytes=false|true` | bodyなし; default false | `200 Cursor` | `bad_binding_id,binding_unknown,catch_up_unsupported` |
| `PUT /api/gate-bindings/{binding_id}/source-cursor` | `{expected_connection_epoch:null,start:{mode:"now"}|{mode:"beginning"}|{mode:"supplied",cursor_b64}}` | `200 Cursor` as include_bytes=false | `bad_binding_id,unknown_field,binding_unknown,binding_closed,catch_up_unsupported,instance_active,cursor_invalid` |
| `POST /api/gate-bindings/{binding_id}/catch-up` | `{expected_connection_epoch:u64}` | `202 {binding_id,connection_epoch,accepted:true}` | `bad_binding_id,unknown_field,binding_unknown,binding_closed,catch_up_unsupported,instance_not_ready,epoch_mismatch,catch_up_in_progress` |

Instance PUTは `lifecycle=stopped,active_revision=1` とrevision 1の全non-null値を一transactionで作る。Binding PUT requestに `purposes` は存在しない。serverが必ずsole subjectのrouteを `inbound`、次に `outbound` の二行作り、responseもliteral順を返す。片方向は作れない。

DELETE instanceはtombstone revisionをappendし、open binding/placeをclose、selecting routeを削除し、旧revisionの全 `connecting|active` epochを同transactionでcloseする。Revision POSTも旧revisionの全 `connecting|active` epochを同transactionでcloseする。commit後にplugdが対応する全Arcをcloseする。既にtombstoneのDELETEはwrite-zero `deleted=false`。

## 4. config/secret delivery

orchestratorはInstance GETからcanonical config/digest/revisionとmanifestだけを読み、revision専用directoryを作る。

```text
<revision-dir>/config.json       canonical bytes, regular, 0444
<revision-dir>/secrets/          directory, 0700
<revision-dir>/secrets/<name>    binary value, regular, 0400
```

required欠落、unknown name、symlink、subdirectory、non-regular fileは起動失敗。directoryはread-only mountし、pathだけを `OPENCRAB_GATE_SOCKET`, `OPENCRAB_GATE_INSTANCE_ID`, `OPENCRAB_GATE_REVISION`, `OPENCRAB_GATE_CONFIG_PATH`, `OPENCRAB_GATE_SECRETS_DIR` で渡す。secret value、config bytes、master keyをargv、env value、log、status、admin errorへ複製しない。

stage/fsync directory → revision POST → 全旧epoch失効commit → new mount/container → hello/ready/bind → old directory cleanupの順。失敗時に旧revisionへ暗黙rollbackしない。gatewayはconfig file SHA-256 lowerhexを `hello.config_digest` に入れる。

## 5. protocol 2 grammar

transportはUnix domain socket。UTF-8、一行一JSON object + LF、LF込み最大1048576 bytes。duplicate memberを拒否する。request `id` は1〜128 byteのnonempty string、`m` は下記literal。top-levelと下記exact nested objectのunknown memberを拒否する。

共通型:

```text
UUID = canonical lowercase UUID
Digest = 64 lowercase hex
B64 = RFC4648 standard padded base64
Author = {id:string,display?:string}
Content = {text?:string}
Attachment = {kind:"image",url:string,origin_author?:string}
Action = {surface_id:string,component_id:string,action_name:string,
          context:JSON|null,responder_id:string}
WireErr = {code:string,at:string|null,detail:string|null}
ConnectionArcAuthority = opaque Arc identity; in-process only, non-serializable,
                         non-persistent, equality is Arc::ptr_eq only
```

message union:

| direction / `m` | exact fields and value grammar | success `ok` |
|---|---|---|
| gate→core `hello` | `{id,m,protocol:2,kind_id,instance_id:UUID,revision:u64,config_digest:Digest,origin_scope:instance|kind_address,address_form:string,ingress_discovery:"prebound",effects:["say"],capabilities:["open"]}` | `{protocol:2,connection_epoch:u64}` |
| gate→core `ready` | `{id,m,connection_epoch:u64}` | `{}` |
| gate→core `failed` | `{id,m,connection_epoch:u64,code:string}` | `{}` then close |
| core→gate `bind` / `unbind` | `{id,m,binding_id:UUID,address:string}` | `{}` |
| gate→core `event` | `{id,m,kind:said|edited|retracted|reacted|ui_action,address:string,binding_id:UUID,author:Author,content?:Content,mentions?:string[],reply_to?:string,origin:string,target?:string,symbol?:string,removed?:boolean,action?:Action,attachments?:Attachment[]}` | `{seq:i64|null,binding_id:UUID}` |
| gate→core `source_checkpoint` | `{id,m,binding_id:UUID,expected_cursor_digest:Digest|null,cursor_b64:B64}` | `{cursor_digest:Digest,updated_at:i64}` |
| gate→core `place_closed` | `{id,m,binding_id:UUID,address:string,reason:deleted|archived|left|unavailable}` | `{closed:true}` |
| core→gate `catch_up` | `{id,m,binding_id:UUID,address:string,start:{mode:"now"}|{mode:"beginning"}|{mode:"cursor",cursor_b64:B64,cursor_digest:Digest}}` | `{}`; checkpointは別request |
| gate→core `read` | `{id,m,address:string,from?:i64,limit?:u32}` | `{events:{seq:i64,kind:string,author:Author,content:Content,reply_to?:i64,origin?:string}[],next?:i64}` |
| core→gate `effect` | `{id:UUID,m,binding_id:UUID,address:string,kind:"say",payload:JSON}`; `id` is byte-equal canonical text of `delivery_id` | `{delivered:true,origin:string}` xor `{delivered:false}` |
| core→gate `activity` | `{m,address:string,activity_id:string,state:started|progress|ended,kind?:turn|background,label?:string}` | responseなし |
| response | exact `{id,ok:JSON}` xor `{id,err:WireErr}` | pending request ID一件に対応 |

`event.origin` はnonemptyでlive/catch-up同一stable ID。prebound external eventの `binding_id` は必須で、connection instance/revision/epoch、open binding generation、addressと一致させる。`source_checkpoint` はaddress fieldを持たず、address一致検査も `address_mismatch` errorもない。bindingのinstance/open generation、cursor schema/size、expected digest CASだけを検査する。`place_closed` はaddressを持つためbinding addressも検査する。

eventのkind別required/forbiddenは次である。表にないoptional fieldはforbidden。

| kind | required | optional |
|---|---|---|
| `said` | `content.text` またはnonempty `attachments` の少なくとも一方、`origin` | `mentions,reply_to,attachments` |
| `edited` | `content.text,origin,target` | `mentions,reply_to,attachments` |
| `retracted` | `origin,target,removed:true` | なし |
| `reacted` | `origin,target,symbol,removed:boolean` | なし |
| `ui_action` | `origin,action` | なし |

全string identity/address/origin/target/symbolはnonempty。`Author.id` はnonempty、`Content` は少なくとも一member、attachment URLはabsolute `https` URL。`read.from` はpositive、`limit` は1..1000。`failed.code` はnonempty。helloの `effects` と `capabilities` はsetではなく記載順・長さも含むexact literalである。`tools` / `actions` fieldはunknown fieldであり、空・追加・逆順・重複specも `kind_spec_mismatch` でcloseする。期待specは永続catalogから導出せず、この初期protocol literalそのものである。

activityは `started` が `kind` 必須・`label` optional、`progress` が `label` 必須・`kind` forbidden、`ended` が `kind,label` forbidden。違反activityは `invalid_field` としてresponse無しで捨て、connectionをkeepする。他messageのfield/type違反はerror responseを返す。

validation priorityは framing/size → request/response union → top-level field → nested field → connection state → instance/revision/epoch → binding generation/address（fieldがあるmessageだけ）→ schema/value → store/CAS。

| violation | stable outcome | connection | durable effect |
|---|---|---|---|
| invalid UTF-8/JSON/non-object/duplicate member | response可能なら `bad_request` | close | incomplete sendingは既存failed/indeterminate規則 |
| >1048576 bytes | `too_large` | close | 同上 |
| hello timeout、hello前別message、二回目hello | `protocol_order` | close | new active epoch 0 |
| hello protocol/kind/instance/revision/config/spec mismatch | `protocol_unsupported|bad_kind_id|bad_instance_id|instance_unknown|instance_disabled|revision_mismatch|config_digest_mismatch|kind_declaration_mismatch|kind_spec_mismatch|instance_active` | close | new active epoch 0 |
| malformed response union for a known pending effect ID | `response_invalid` | close | 同Arc上の全pendingへ `protocol_error` outcomeを一回ずつ返す。期限前current sendingだけ `error=protocol_error`、期限到達後はmatrixにより `error=timeout` |
| malformed/unknown response ID | `response_invalid` | close | malformed/unknown ID自身のdelivery/observation write 0。同Arc上の既知sendingへ `protocol_error` outcomeを一回ずつ返す |
| ready/bind完了前 event/effect | `instance_not_ready` | keep | request write 0 |
| hello `tools`/`actions` field、またはcore→gate `tool` | `unknown_field|unknown_message` | helloはclose、post-readyはkeep | write 0 |
| post-ready unknown message/field/value | `unknown_message|unknown_field|missing_field|invalid_field|unknown_enum` | keep | request write 0 |
| binding absent/closed/wrong generation/address | `binding_unknown|binding_closed|binding_generation_mismatch|address_mismatch` | keep | Event/cursor write 0 |
| checkpoint unsupported/schema/digest/order | `catch_up_unsupported|cursor_invalid|cursor_digest_mismatch|checkpoint_out_of_order` | keep | cursor不変 |
| bind error/60s timeout | `bind_failed` | failed then close | provision保持、catch-up 0 |
| explicit `failed` / socket close | reported code / disconnect | close | cursor不変、single-flight解放。同じpendingへprotocol_error後またはintentional revision close後のdisconnectを重ねない。期限到達後はmatrixにより `error=timeout` |
| store failure after ready | `store_error` | close | transaction rollback |

## 6. state、revision invalidation、catch-up

```text
CONNECTED --valid hello--> SYNCHRONIZING(epoch=connecting)
SYNCHRONIZING --valid ready--> BINDING
BINDING --all open binding ack in binding_id byte order--> ACTIVE
ACTIVE --new provision--> BINDING(epoch remains connecting until ack)
any --fatal|failed|socket close|revision invalidation--> CLOSED
```

ready後も全bind ack前はnot ready。bindはbinding ID byte orderで一件ずつack待ち。全ack後だけstate=active、その後cursor-capable bindingを同順でautomatic catch-upする。

revision POST / instance DELETE transactionは、hello後ready前、ready後各bind待ち、active、active→connecting再同期中のどこでも、旧revisionに属する全 `connecting|active` epochをclosedにする。coreのrevision invalidation coordinatorはtransaction内で `invalidated_at` を一回取得し、旧Arcに属するprocess-local pendingをsnapshotする。commit時点で全旧socketのwriter authorityが消え、後続messageはEvent/write 0である。

`BeginExternalDelivery` のactive authority検査、successful `sending` commitとin-flight registry登録、全 `RecordExternalDeliveryOutcome` callback、revision/deleteのauthority除去・pending snapshotは同じcore coordination guardで直列化する。通常callbackはguard permitを取得してからRecordを呼び、revision invalidation coordinatorは取得済みpermitを各Record呼出しへ渡すため再取得せず、再入deadlockを作らない。Begin側が先にcommitしたdeliveryはsnapshotにexact一回含まれ、revision/delete側が先にcommitした時はBegin側がpre-wire `failed(error=not_connected)` になり、`sending` とhandoffを作らない。transaction rollback時はregistry/snapshotも公開しない。

revision/delete commit後、core revision invalidation coordinatorだけがsnapshot各件へ `received_at=invalidated_at` の `revision_invalidated` outcomeをexact一回生成し、同じexpected Arcとtupleで `RecordExternalDeliveryOutcome` へ渡す。coordinatorは全snapshot outcomeが `Applied|AlreadyTerminal` になるまでcoordination guardを保持し、その後plugdへtyped intentional closeを渡して対応Arcを資源解放させ、最後にguardを解放する。そのcloseから `disconnect` / `protocol_error` outcomeを生成させない。これによりcommit後/outcome前へresponse、timeout、disconnect、protocol callbackは割り込まない。plugd closeは永続safetyの条件でもoutcome producerでもない。commit後にprocess crashした時だけoutcome fan-outは未完になり得るが、残存 `sending` は既定の `RecoverStaleRuntimeState` が回収し、revision outcomeを再生成しない。

automatic/manual catch-upは `(binding_id,connection_epoch)` single-flight。重複manualは `catch_up_in_progress`。gatewayはpage順eventのdefinitive ack後だけcheckpointを送る。gap、fetch/event failure、timeout、disconnect、out-of-order、CAS mismatchはcursorを進めずsingle-flightを解放する。

## 7. binding generation、close、delivery

Binding PUTはfresh place、`scope_id=binding_id` のorigin scope、open binding、sole-subject membership、literal inbound/outbound二routeを一transactionで作る。`UNIQUE(instance_id,address) WHERE closed_at IS NULL`。同address再利用はnew UUIDv7 binding/place/scopeで、closed generationをreopen/repointしない。

authoritative external deletion/archive/leave/unavailable確認時だけ `place_closed` を送る。disconnect、list欠落、fetch failure、cursor gapからcloseを推測しない。closeはbinding/place/routeだけを変え、membership/origin/cursor/event/deliveryを保持する。

outboundの唯一の永続ownerはcore/storeで、状態は `prepared -> sending -> delivered|failed|indeterminate`。自動再送と別instance fallbackはなく、`attempt` は0または1だけである。external effectの時間正本はこの文書で宣言する固定値 `EXTERNAL_EFFECT_TIMEOUT_SECS=300`、すなわち `EXTERNAL_EFFECT_TIMEOUT_NANOS=300_000_000_000` である。設定値ではなく、builtin Discordから継承もしない。

reply effectと `prepared` deliveryは一transactionで作る。coreが選択済みoutbound routeのexact `binding_id` をcopyし、そのbindingから `instance_id` を固定する。`prepared` は `attempt=0`、`revision/connection_epoch/deadline/remote_origin/error=NULL` である。addressだけから別bindingを選び直さない。

socketごとにplugdは一つのopaque `Arc<ConnectionArcAuthority>` を作る。authorityは `(instance_id,revision,connection_epoch)` を保持するが、同じ値を持つ別Arcは別authorityである。同じallocationを保つ `Arc::clone` だけを許し、tupleからの再構築、serialize、DB保存、wire送信は禁止する。同一性は `Arc::ptr_eq` だけで比較する。coreのconnection registryはactive epochとそのexact Arcを一対一で保持する。

`BeginExternalDelivery(delivery_id, arc_authority)` はcore/store commandである。一つのimmediate transaction内でdelivery、選択済みroute、`gate_bindings` を `binding_id` でexact joinし、bindingがopenで同じplace/instance/address generationであること、authorityのtupleとactive revisionのactive connectionがexact-oneであること、registryのauthorityと `Arc::ptr_eq` であることを検査する。binding closed、route changed、未接続、authority不一致ならwire I/O 0のまま `prepared -> failed`、`error=binding_closed|route_changed|not_connected|connection_authority_mismatch` とする。

検査成功時、storeはtransaction時刻 `claimed_at` を一回だけ取得し、signed-i64 checked加算 `deadline=claimed_at+300_000_000_000` を行う。overflowなら同じtransactionで `prepared -> failed(error=deadline_overflow)` としwire I/Oは0。成功時だけ `attempt=1`、revision、connection_epoch、deadlineをsnapshotして `prepared -> sending` にし、commit後に同じopaque authorityを含む次のimmutable commandをplugdへ渡す。

```text
ExternalWireDelivery = {delivery_id:UUID,binding_id:UUID,instance_id:UUID,
                        revision:u64,connection_epoch:u64,address:string,
                        kind:"say",payload:JSON,deadline:i64,
                        arc_authority:Arc<ConnectionArcAuthority>}
ExternalWireOutcome  = {delivery_id:UUID,binding_id:UUID,instance_id:UUID,
                        revision:u64,connection_epoch:u64,received_at:i64,
                        arc_authority:Arc<ConnectionArcAuthority>,
                        result:delivered(origin)|rejected|error(WireErr)|timeout|
                               disconnect|protocol_error|revision_invalidated,
                        response_digest:Digest|null}
```

plugdはcommandの `delivery_id` をcanonical UUID textにしてeffect `id` に使い、commandの `binding_id/address` をそのままrenderする。commandを受けるsocket authorityは `Arc::ptr_eq(command.arc_authority, owned_arc)` を必須とし、別Arcへrerouteしない。plugdは状態を遷移せず、受け取ったexact authorityをwire由来の `ExternalWireOutcome` へ戻す。gatewayの `err` は「external APIが受理していないと確定」の時だけ許す。受理前後が不明なら成功/拒否/errorを捏造せずsocketを閉じ、`disconnect` とする。`revision_invalidated` variantはcore revision invalidation coordinatorだけが生成し、plugdが生成することは禁止する。

`received_at` はresponseの完全なLF frameをplugdが受理した時、またはtimeout/disconnect/protocol failureを観測した時に一回取得するcore processのUTC nanosecondsである。`revision_invalidated` だけはrevision/delete transactionが一回取得した `invalidated_at` を使う。期限内response/controlは厳密に `received_at < deadline` だけで、`received_at == deadline` を含む `received_at >= deadline` はdeadline到達済みである。

protocol違反でArcをcloseする時、plugdはclose理由を失う前に、そのArc上の全pending deliveryへexact一回 `protocol_error` outcomeを返す。そのpendingへclose由来の `disconnect` を重ねない。明示failed、EOF、I/O dropだけが `disconnect` である。typed revision invalidation closeはcoreが `revision_invalidated` を生成し、plugd control outcomeは0件である。gateway `err:WireErr`、`protocol_error`、`disconnect`、`revision_invalidated` は互いに代用しない。

`response_digest` は `delivered|rejected|error(WireErr)` のfull response frameでnon-NULL、`timeout|disconnect|protocol_error|revision_invalidated` ではNULLである。

`RecordExternalDeliveryOutcome(expected_arc_authority,outcome)` だけが次を一つのimmediate transactionで適用する。expected authorityは `BeginExternalDelivery` がin-flight registryへ置いたexact Arcで、outcome authorityとの比較は `Arc::ptr_eq` だけである。戻り値はexact `Applied`、`AlreadyTerminal`、`ContractViolation(EarlyTimeout|UnknownDelivery|AuthorityMismatch)` のいずれかである。`ContractViolation` はdelivery/observation write 0でruntimeをfail-loudにする。registryはterminal/timeout後もvalid responseを一意分類できるよう、responseを一件受理するか、そのArcのread loopとclose callbackを全てdrainするまで `delivery_id -> expected Arc` を保持する。process crash後のregistry再構築はせず、startup recoveryが残存sendingをterminal化する。

valid responseのbranch分類順は (1) malformed/unknown ID、(2) known IDだがbinding/instance/revision/epochまたはArc pointer不一致、(3) tuple/Arc一致かつ `received_at >= deadline`、(4) tuple/Arc一致だが既にterminal、(5) `received_at < deadline` のcurrent `sending` である。(2) はstate/deadlineにかかわらず `wrong_epoch_response`。(3) はまず `sending -> indeterminate(error=timeout)` CASを行い、その後同じtransactionで `late_response` をappendする。timeout workerとresponseが競合してもstore serializationとこの順序により、等号時のcanonical winnerは必ずtimeoutである。(4) はstate不変の `late_response`。一responseを二種類のobservationへ記録しない。

matching control outcomeのbranch分類順は (1) unknown deliveryまたはtuple/Arc不一致を `ContractViolation(UnknownDelivery|AuthorityMismatch)`、(2) terminalを `AlreadyTerminal`、(3) current `sending` かつ `received_at >= deadline` をcauseにかかわらずcanonical timeout、(4) current `sending` かつ `received_at < deadline` をcause別に処理、である。(2) は二件目のexact idempotent successで、delivery/observation write 0、runtime fail 0。(4) のtimeoutだけは `ContractViolation(EarlyTimeout)` でwrite 0/fail-loud、他三causeはexact errorへterminal化する。

| current row / time | `timeout` | `disconnect` | `protocol_error` | `revision_invalidated` |
|---|---|---|---|---|
| current `sending`, `received_at < deadline` | `ContractViolation(EarlyTimeout)`、write 0、fail-loud | `Applied`: `indeterminate(error=disconnect)` | `Applied`: `indeterminate(error=protocol_error)` | `Applied`: `indeterminate(error=revision_invalidated)` |
| current `sending`, `received_at >= deadline` | `Applied`: `indeterminate(error=timeout)` | `Applied`: `indeterminate(error=timeout)` | `Applied`: `indeterminate(error=timeout)` | `Applied`: `indeterminate(error=timeout)` |
| terminal, `received_at < deadline` | `AlreadyTerminal`、write 0 | `AlreadyTerminal`、write 0 | `AlreadyTerminal`、write 0 | `AlreadyTerminal`、write 0 |
| terminal, `received_at >= deadline` | `AlreadyTerminal`、write 0 | `AlreadyTerminal`、write 0 | `AlreadyTerminal`、write 0 | `AlreadyTerminal`、write 0 |

同一pendingのmatching callbackはstore transaction取得順で直列化する。期限前のdisconnect/protocol_error/revision_invalidated同士は最初にcommitした一件だけがterminal winnerで、二件目以降は `AlreadyTerminal`。deadline以後にcurrent `sending` を最初に得たcontrol callbackはvariantを問わずtimeout winnerで、後続は `AlreadyTerminal`。期限前timeoutはwinnerにならない。producer側のprotocol-close disconnect抑止とintentional revision-close control抑止は重複を通常経路で減らすが、競合safeの根拠はこのtotal matrixとterminal no-opである。

| port result / wire branch | exact current-row条件 | 永続結果 |
|---|---|---|
| `delivered(origin)` | `received_at < deadline`、current `sending`、tuple/Arc一致、origin nonempty | `delivered`、`remote_origin=origin`、`error=NULL` |
| `rejected` | 同上、originなし | `failed`、`remote_origin=NULL`、`error=not_delivered` |
| `error(WireErr)` | 同上 | `failed`、`remote_origin=NULL`、`error` にcanonical WireErrを保持 |
| `timeout` | 同じcurrent `sending` tuple/Arc、`received_at >= deadline` | `indeterminate`、`remote_origin=NULL`、`error=timeout` |
| `disconnect` | 同じcurrent `sending` tuple/Arc、`received_at < deadline` | `indeterminate`、`remote_origin=NULL`、`error=disconnect` |
| `protocol_error` | 同じcurrent `sending` tuple/Arc、`received_at < deadline` | `indeterminate`、`remote_origin=NULL`、`error=protocol_error` |
| `revision_invalidated` | 同じcurrent `sending` tuple/Arc、`received_at < deadline` | `indeterminate`、`remote_origin=NULL`、`error=revision_invalidated` |
| 任意のcontrol outcome | 同じcurrent `sending` tuple/Arc、`received_at >= deadline` | causeを上書きして `indeterminate`、`remote_origin=NULL`、`error=timeout` |
| 任意のcontrol outcome | 同じterminal tuple/Arc、時刻不問 | `AlreadyTerminal` success、delivery/observation write 0、runtime fail 0 |
| `timeout` かつ `received_at < deadline` | internal port contract violation | delivery/observation write 0、runtime fail-loud |
| deadline到達後またはterminal後のvalid response | `delivery_id` とtuple/Arcは一致 | timeout CASを先にした後、またはstate不変で `delivery_observations.kind=late_response` をappend |
| revision/epoch/Arc pointerがsnapshot authorityと不一致のvalid response | `delivery_id` は既知 | state不変、`delivery_observations.kind=wrong_epoch_response` をappend。正authorityのpendingは別途timeout可能 |
| unknownまたはtuple/Arc不一致の `timeout|disconnect|protocol_error|revision_invalidated` | response bytesなし | `ContractViolation(UnknownDelivery|AuthorityMismatch)`、delivery/observation write 0、runtime fail-loud |

observationはobserved revision/epoch、outcome `delivered|rejected|error`、exact response bytesのdigest、`received_at` を持つ。control outcomeはobservationを作らない。valid late/wrong-epoch `delivered:true` のnonempty originは `delivery_observations.remote_origin` にだけ保持し、canonical `deliveries.remote_origin` を変更しない。初期effectは `say` だけなので、期限内の `delivered:true` でorigin欠落/空は成功ではなく `protocol_error` としてArcをcloseし、同Arcの全pendingへprotocol_error outcomeを返す。その永続結果は各pendingのdeadline/current stateをtotal matrixへ通して決める。unknown/malformed response ID自身はdelivery row/observationを書かない。

`RecoverStaleRuntimeState` はcore/storeのstartup commandである。schema作成・migration完了後、runtime listener、hello、timer、plugd handoffより前に一回だけ、`BEGIN IMMEDIATE` transaction開始時点の `deliveries WHERE state='sending'` 全行を走査する。これは既存のstale nonterminal gate epoch回収と同じcommand・同じtransactionであり、deliveryだけの二つ目のstartup passを作らない。deadline、kind、instance、revision、epochで絞らず、全行をwire I/O 0のまま `indeterminate`、`remote_origin=NULL`、exact `error="stale sending recovered after restart"` にする。observationは追加せず、preparedとterminalは変更しない。compatibility projectionが存在する間は同じtransactionで対応rowも既存public failed projectionへ収束させる。途中失敗はepochとdeliveryを含む全回収をrollbackしてstartup自体を失敗させる。commit前のlistener開始、残存sendingの再handoff、自動再送、hello側repairは禁止する。二回目startupは対象0でwrite 0である。

## 8. purpose、input、添付

external purposeは常にbyte-exact `['inbound','outbound']`。片方向、`timed`、`tool:<name>` は初期surfaceにない。inputは全てimmediateで、gatewayはauthority/debounce/timer/window/policyを持たない。

external helloは常にbyte-exact `effects:['say']` / `capabilities:['open']`。tool/action declarationはfield自体が無く、core→gate `tool` messageも無い。dynamic specはこのgrammarへ追加せず、永続正本・offline admin投影・route生成・互換性を同時に定義する後続issueで扱う。

添付はURL参照 `image` だけ。byte/base64/multipart/local path/unknown kindを送らない。activityはbest-effort表示でdeliveryを作らない。NO_REPLY、settled context、permission、route selectionはcore契約である。

## 9. 実装checklist

- adminでschema→kind→instance→bindingを先に登録した
- binding requestにpurposesを送らず、responseがliteral二purposeである
- revision config/secret directoryをread-only注入しhello digestを照合した
- ready後ordered bind完了までeventを送らない
- event/checkpoint/closeに正しいbinding generationを付けた
- checkpointへaddressを追加しない
- helloにliteral以外のeffect/capabilityやtools/actionsを追加しない
- effect `id` にdelivery_idを使い、binding/revision/epoch/opaque Arc authorityを選び直さない
- external正本の300秒をtransaction時刻へchecked加算し、等号をtimeout CASへ倒した
- delivery outcomeはprotocol_errorとdisconnectを区別して必ずcoreのstate commandへ返し、plugdからdelivery tableを書かない
- control outcomeは16セルmatrixへ通し、deadline到達後は全causeをtimeout、terminal二件目はAlreadyTerminalにする
- revision/deleteだけがrevision_invalidated outcomeを生成し、intentional plugd closeからdisconnect/protocol_errorを返さない
- runtime port開始前にRecoverStaleRuntimeStateを完了し、全残存sendingを再送しない
- cursorはcore CAS後だけ進んだと扱う
- revision/delete後の全旧socketを無効化する
- closeを推測せずaddress reuseをnew generationにする
- core判断とgateway配送を互いの側へ移さない
- secret、実在識別子、private service値をcode/test/log/reportに入れない
- この文書で表現できない現物は補完せず持ち帰る

片方向、shared instance、managed runner、dynamic effect/tool/action spec、timed/tool route、generic debounce、TCP、画像以外の添付は、実在要求が出た時の別issueである。初期surfaceの未決ではない。
