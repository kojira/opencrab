# Discord gate 非 voice slice 設計 v15（issue #742）

## 0. 結論、範囲、権威

この issue は、target gate model の上に Discord の非 voice slice を一つ追加する。設定の正本は DB の `gate_kinds` / `gate_instances` / `gate_instance_revisions` / `secret_values`、接続状態の正本は `gate_connections` である。1 credential = 1 instance = 1 process とし、同じ Discord kind の複数 instance を同時に動かす。

`opencrab-discord-gate` は Discord I/O と protocol 変換だけを持つ。Discord kind は **membership 駆動 ingress** を宣言し、自分が member である全channelのdispatchをcoreへforwardする。ただし forwarding は受信候補の提示であって admission ではない。core/store は guild のsubject override→place default→false、DM のsubject別trusted/owner、threadの非対応を先に判定し、eligible subjectが一人以上いる時だけdurable stateを実体化する。place、subject、policy、route、tool visibility、delivery、interaction、lifecycle は core/store が持つ。JSON 設定正本、Discord 専用 readiness HTTP、projection file、二重 supervisor、route fallback は作らない。

初期 profile は次で固定する。

| 項目 | この issue の値 |
|---|---|
| ingress | `said`, `ui_action` |
| effect | `say`, `react`, `ui` |
| gate operation | §8.1 の 6 件 |
| core capability | §8.2 の 5 件 |
| origin scope | `kind_address` |
| process | 1 bot token / 1 instance |

Message Create と component/modal interaction の既存変換元は `e6f0595:crates/discord/src/gateway.rs:444-668`、長文送信で最後の message ID だけを返す既存外形は同 `:188-213` である。

この issue は非 voice slice である。`join_voice_channel` / `leave_voice_channel`、voice ingress、STT/TTS は後続の Discord voice slice へ送る。**実装 PR 作成時に後続 issue を切り、その番号を本節・§9・§10へ戻す。番号を先に推測しない。** legacy Discord の退役には非 voice 11 件と voice 2 件を合わせた 13 tool の存在と機能確認が必要である。

次は含めない。

- Message Update/Delete、Reaction Add/Remove の Event 化。
- `quote` / `amend` / `retract`、複数 origin、part 永続化、reconcile。
- bot identity pin / TOFU / rotation。
- file spool、独自 rate-limit dispatcher、history backfill、full fake Discord。

権威の順は、(1) issue #742 の統括裁定、(2) 本書、(3) 改訂済み `TARGET-SCHEMA.json`、(4) 改訂済み `TARGET-MAPPING.json` / `GATE-CONFIG-MAP.json` / `ADMIN-PROJECTION.json`、(5) worktree / `e6f0595` の引用コード、(6) L1/L2 baseline とする。台帳差分を本書へ再掲しない。entity、field、transform、admin read/write set の物理契約は改訂済み台帳を直接参照する。

<!-- ledger-reference-manifest
{
  "entity_ids": [
    "agent_grants", "deliveries", "delivery_observations", "events", "expanded_gate_tools", "external_origin_scopes",
    "external_origins", "external_refs", "gate_bindings", "gate_connections", "gate_dedup_hits",
    "gate_instance_revisions", "gate_instances", "gate_kinds", "gate_operations",
    "gate_subject_identities", "grant_sets", "grant_source_provenance", "interactions", "interaction_responses", "legacy_history_archive",
    "legacy_unowned_source_rows", "memberships", "nostr_generated_keys", "place_default_policies",
    "place_source_refs", "place_subject_policies", "places",
    "schedules", "secret_sets", "secret_values", "subject_routes", "webhook_endpoints"
  ],
  "resource_ids": [
    "resource:engine_registry", "resource:operator_workspace", "resource:process_log_filter",
    "resource:workspace_tree", "subject_workspaces"
  ],
  "coverage_ids": ["canonical_database"],
  "staging_ids": [],
  "transform_ids": [
    "copy-sqlite-value-v1", "create-subject-public-id-v1", "discord-channel-policy-router-v1", "effective-config-v1",
    "gate-instance-and-subject-v1", "history-per-agent-router-v2",
    "gate-instance-id-v1", "logical-gate-binding-v1", "parse-utc-nanos-v1", "place-public-key-v1",
    "pending-interaction-router-v2", "resolve-external-principal-v1", "resolve-subject-public-id-v1",
    "resolve-or-create-config-place-v1", "secret-value-v1", "shared-gate-instance-v1",
    "sqlite-bool-v1"
  ]
}
-->

## 1. target model と不変条件

### 1.1 kind、instance、revision、secret、connection

| entity | 正本であるもの | 規則 |
|---|---|---|
| `gate_kinds` | protocol family | `kind_id`, `protocol_major`, `origin_scope`, `ingress_discovery`。Discordは`membership`、compatibility kindは`prebound`。live spec は §2.5 の型で比較する |
| `gate_instances` | credential/process/owner 単位 | UUID `instance_id`、`kind_id`、nullable owner、active revision、lifecycle |
| `gate_instance_revisions` | immutable schema-bound config と desired state | `present`、`enabled`、`config_schema_id/config_bytes/config_digest`。active revision だけを起動に使う |
| `secret_values` | secret bytes | token は `discord_bot_token`。通常 query/log/report に値を出さない |
| `gate_connections` | connection epoch と observed state | readiness の唯一の DB 正本 |

既存 transform `gate-instance-and-subject-v1` と `secret-value-v1` が dedicated Discord/Nostr の complete source rowを一度に分類する。owner が exact-one で解決でき、bool/config/secret を canonical に組み立てられる時だけ、`gate_instances`、revision 1、`secret_sets`、`secret_values` の全必須 fieldを出す。revision 1 は `present=true`、`created_at=snapshot captured-at` である。legacy dedicated config の `updated_at` は現行readerが解釈しないため、timestamp parseせず exact SQLite logical valueをschema-bound configの`legacy_updated_at`へ運ぶ。これがparse不能・非TEXTでもaggregate失効理由にしない。owner がNULL・不正・0件・複数件、または必須値が非canonicalなら、source row全体を `legacy_unowned_source_rows` へ一行だけ書く。ownerless dedicated instanceを作らず、後続rowのconverter処理は続ける。kind 固有値は `GATE-CONFIG-MAP.json` の schema-bound bytes に組み立て、token/key bytes は `secret_values` だけへ送る。`OPENCRAB_GATE_INSTANCES` と `$DEV_DIR/gate-instances.json` は作らない。

config 由来 shared instance は transform `shared-gate-instance-v1` だけが作る。入力は immutable effective config とそこから参照された token、locator は config source identity と normalized kind の組である。`gate_instances` は `kind_id=discord,label=shared:discord,owner_subject_id=NULL,active_revision=1,lifecycle=stopped`。revision 1は`present=true`、enabledをeffective configから、created_atをsource snapshot captured-atから取り、schema `gate-config/discord/v1` を使う。**shared参加subject集合の唯一の正本は、このconfig bytes内のeffective `agent_ids`**である。各distinct IDはrevision生成前にsubjectへexact-one解決し、重複はcollapse、未解決/曖昧ならshared assemblyをrejectする。channel config rowから参加subjectを足さない。保存済みbot IDが無いshared configの`self_external_id`はschema-valid NULLであり、偽値を作らない。secret set IDは同じinstance locator + revision 1から決め、`revision=1,scope=gate-instance:<instance_id>,created_at=snapshot captured-at`。valueは`name=discord_bot_token,at_rest_format=source-plaintext`で、revisionの`secret_set_id`をこのsetへ結ぶ。converter は毎回空 target へ全量を最初から生成し、incremental resume/upsert はしない。同一 source snapshot と同一 converter binary の再実行は同じ ID を作る。byte encoder の形式は設計契約にせず、実装の golden test で固定する。

active configには独立した3軸がある。`gate_instance_revisions.present` は公開configの存在、同`enabled`は永続化したdesired enabled、`gate_connections`は観測したrunningである。GETはactive revisionの`present`から`configured`を、`present && enabled`から`enabled`を、同じactive revisionのactive connection epochだけから`running`を射影する。process再起動で`present/enabled`は変わらず、connectionだけが変わる。

公開 `DELETE /api/agents/{id}/discord` と `DELETE /api/agents/{id}/nostr` は物理削除ではなくtombstone revision追加である。active `present=true`なら同じinstanceに次revisionを `present=false,enabled=false` で追加・選択し、instance lifecycleを`stopped`、active connectionをclosedにして`deleted=true`を返す。既に`present=false`ならwriteせず`deleted=false`。instance、旧revision、secret、identity、bindingは消さない。Discordだけはconfig変更と同じtransactionで、旧participantに属するmaterialize済みsubject/placeを§1.2の`ReconcileSubjectRoutes`へ渡し、別の有効なanchorがあれば再選択し、無ければrouteを削除する。Nostr routeは従来どおり保存する。PATCHは`present=false`を「設定なし」として既存errorを返し、reactivateしない。Discord PATCHはdesired enabledを保ち、trueなら新revisionへrestart、falseなら停止を保つ。PUTは同じinstanceへ`present=true`の次revisionを追加し、Discordはdesired enabled=trueとしてstart、Nostrはrequestのenabledを保存してtrueならstart・falseならstopする。startはactive `present=true`を要求し、config/secret bytesをcopyした`present=true,enabled=true`の次revisionを追加・選択する。instance不存在またはactive `present=false`は従来どおりno-config errorである。stopはactive `present=true`なら`present=true,enabled=false`の次revisionを追加・選択するが、instance不存在またはactive `present=false`ならrevision/lifecycle/connectionを含め**何も書かず既存success responseを返す idempotent success**である。start/stopと各process結果はlifecycle/connectionだけへ反映し、config三軸を混ぜずrouteを再整合しない。

`GateKindId` は `[a-z][a-z0-9_-]{0,63}`、`GateInstanceId` は canonical lowercase UUID とする。新規 runtime UUID は台帳共通規則どおり UUIDv7、migration UUID は transform `gate-instance-id-v1` を使う。実在 identifier は source、test、report の固定値へ書かない。

app起動時は、設定済みprotocol-1 gate（web / nostr）ごとに、transform `gate-instance-id-v1` のlocator `compat:<name>`からdeterministic UUIDを導出し、compatibility instanceをseedする。`gate_instances`は`instance_id=<derived UUID>,kind_id=name,label=name,owner_subject_id=NULL,active_revision=1`で、lifecycleは当該connectionの観測状態から導出する。revision 1は`present=true,enabled=true,config_schema_id=compat/v1,config_bytes=<empty bytes>`とそのdigestを持ち、secret setは持たない。同じ設定とbinaryでの再起動は同じUUIDと同じ行へ収束し、別instanceを増やさない。

### 1.2 binding と選択済み route

`gate_bindings` は `{binding_id,place_id,instance_id,address,label?,origin_scope_id,binding_metadata_schema_id,binding_metadata_bytes,binding_metadata_digest}` を持つ。subject、policy、route owner を同居させない。Discord 固有値は `gate-binding/discord/v1` metadata の exact shape `{address_kind: guild|dm|unknown, guild_id: string|null}` に入れ、generic row に platform 固有列を置かない。`address_kind` は typed field であり、`guild` はnon-NULL `guild_id`、`dm|unknown`はNULLだけを許す。threadはadmission前に棄却してbindingをmaterializeしないためmetadata enumに含めない。

選択済み route は次の一関係だけである。

```text
subject_routes(
  subject_id       integer_id NOT NULL,
  place_id         integer_id NOT NULL,
  kind_id          text NOT NULL,
  purpose          route_purpose NOT NULL,
  binding_id       uuid NOT NULL,
  PRIMARY KEY(subject_id,place_id,kind_id,purpose)
)
```

`binding_id` は同じ `place_id` を指し、その instance の kind は `kind_id` と一致しなければならない。`purpose` は既存 `route_purpose` 契約だけを使う。

- `inbound`: この subject はこの place の ingress Event で発火してよい。観測 binding との一致は要求しない。
- `outbound`: 通常 reply、`say`、typing の credential/address。
- `timed`: schedule/heartbeat fire の credential/address。
- `tool:<name>`: gate operation `<name>` の credential。`name` は `[A-Za-z0-9_.-]+`。

Discord 専用の `conversation|tool|activity` は作らない。既存契約の出典は `DESIGN-v8.md:105-110,176-190`、現 entity の型出典は `TARGET-SCHEMA.json:6865,6922` である。

placeをkeyに含める現物根拠は、同一channel複数botと同一bot複数channelを記録した `PRODUCTION-FACTS.md:30-32` である。本文・fixtureではその実在名やIDを使わない。

route は設定時点で選択済みの一行であり、候補順位も materialized owner も無い。turn、delivery、operation の開始時に `subject_routes` を一度読み、binding の instance、active revision、active connection epoch を `GateRoute` へ snapshot する。送信時に route を引き直さず、non-ready なら失敗する。同 kind の別 instanceへ fallbackしない。snapshot で途中差替えは防げるため、route 自体に revision や updated timestamp を持たせない。

route row の唯一の低水準writerは store の `SetSubjectRoute` である。Discord route eligibilityの正本は純粋関数 `RecomputeDiscordRouteAdmission(subject,place,kind,observed_author_external_id?)` 一つとする。この関数はcanonical membership、binding metadataの`address_kind`、place policy/default、materialize済みbinding、active gate config participant、現在のidentity/grant/owner principalを読む。(1) `role=participant` membershipが無い、(2) `address_kind=unknown`、(3) guildで`ResolvePlacePolicy`のwhitelistedがfalse、(4) DMで後述のcurrent trustがfalse、(5) active config上でそのsubjectをownerとするdedicatedまたは`agent_ids`に含むsharedのmaterialize済みbindingが0件、のいずれかならnon-eligibleを返す。threadはbinding自体が無いので入力にならない。unknown は route 0、membershipは履歴として保存し、non-eligibleならroute targetは必ず空集合である。

DMの現在判定は ingress と route再計算で同じ純粋関数 `IsCurrentDiscordDmPrincipal(instance_id,subject_id,external_id)` を使う。これは `(instance_id,external_id)` の現在の `gate_subject_identities` が解くprincipalに対し、subjectのactive `grant_sets` / `agent_grants` がowner/owner-equivalent/trustedとして許可するか、または当該instanceのactive `gate-config/discord/v1.owner_external_id` とbyte一致する時だけtrueである。`grant_source_provenance` と過去membershipはauthorization入力にしない。DM placeの永続principal候補は、`external_refs(direction=inbound)`を介してDiscord kindのbindingへ結ばれたlive `events.author_external_id` のdistinct non-NULL集合と、migration/runtime共通の `place_source_refs.metadata.dm_user_id` のnonempty stringを合併してbyte昇順・重複なしにする。初回DMのpre-admissionだけはEvent append前なので、同じ `RecomputeDiscordRouteAdmission` に `observed_author_external_id` を一時候補として渡す。eligibleになった`ObserveGateAddress`はrouteを作る前に、再利用するsource refがconfig-onlyなら `classification=config_only → live` を同transactionで行い、その後にobserved authorを同じsource refへ永続化する。このclassification遷移は**一方向・一回**で、以後のroute再計算は一時候補を使わない。候補のどれかについて、subjectが参加するmaterialize済みactive bindingのinstanceで同関数がtrueならDM eligibleである。

policy / tool visibility / gate config / schedule / trusted-user DELETE / owner principal変更の6 writer、runtime observation、migration後段passは全てこの同じ `RecomputeDiscordRouteAdmission` を先に一回呼び、eligibleの時だけ次の導出へ進む。trusted-user DELETEはidentity/grantを除く前に対象subjectのmaterialize済みDM tupleを列挙し、削除と同じtransactionで再整合する。owner principal変更は`CommitGateConfigRevision`の旧新participant和集合に対する既存再整合へ含め、revision変更と同じtransactionで失効させる。どちらもmembershipは変更しない。

Discordのpurpose集合は、migration/runtime共通の純粋関数 `DeriveDiscordRoutePurposes(eligible_subject,place,kind)` だけが導出する。Discordのdeclared operation集合は§8.1の固定6件を返す `DeclaredDiscordOperations()` 一つを権威とし、接続中か否かで変えない。返値は必ず `inbound,outbound` を含み、`ResolveHeartbeat(subject,place,kind).enabled=true` **または** `schedules` に `owner_subject_id=subject,place_id=place,enabled=true,kind IN (heartbeat,cron,every)` のfire可能な行が一件以上ある時だけ `timed` を含む。後者はdue時刻や`next_fire`ではなく有効な定義の存在を問う。そのsubjectへvisibleで`DeclaredDiscordOperations()`に含まれる各operationについてだけ `tool:<name>` を含み、重複のないbyte昇順集合を返す。導出入力は`subject_routes`を一切読まない。reader は ingress=`inbound`、reply/effect/typing=`outbound`、schedule=`timed`、gate operation=`tool:<name>` とする。

`ReconcileSubjectRoutes(subject,place,kind)` は増分binding patchではなく、その主キーprefixのroute集合を丸ごと再計算する一つのstore commandである。同transactionのcanonical stateから`RecomputeDiscordRouteAdmission(...,observed_author_external_id=NULL)`を一回呼び、non-eligibleならtargetを空にする。runtime observationもDM principal evidenceを先に永続化してからNULLで呼び、policy / tool visibility / gate config / schedule / trusted-user DELETE / owner principal変更 / migration後段passもNULLで呼ぶ。eligibleなら`DeriveDiscordRoutePurposes`を一回呼ぶ。anchor bindingは、そのsubjectをownerとするactive/present dedicated instanceのbindingがそのplaceにmaterialize済みならそれ、無ければactive shared revisionの`agent_ids`にsubjectを含むshared bindingとする。各層0件ならanchorなしとしてtargetを空にし、各層複数またはplace/kind不一致なら`route_anchor_ambiguous|route_anchor_invalid`でsource mutationを含むtransactionをrollbackする。targetは導出された各purposeを全て同じanchor `binding_id`へ結んだ集合であり、現在行をupsert/deleteして主キーprefixをexactに一致させる。binding列挙順、既存route、変更を起こしたwriter instanceは選択入力にしない。participant/instanceがgate configから外れた時も同じ再計算によりdedicated→shared再選択またはroute 0へ収束し、membershipは保存する。

policy、tool visibility、gate config、scheduleを変更する各writerは、それぞれ `Upsert/DeletePlaceSubjectPolicy`（該当place/subject）、`SetEffectiveToolVisibility`（該当subjectのmaterialize済みDiscord place）、`CommitGateConfigRevision`（該当instance bindingと新旧participant和集合）、`Create/Update/DeleteSchedule`（変更前後 `(owner_subject_id,place_id)` の和集合）のsource mutationと同じtransactionで、影響するmaterialize済み `(subject,place,kind)` をbyte昇順に一度ずつ列挙し、必ず `ReconcileSubjectRoutes` を一回呼ぶ。schedule createは新tuple、deleteは旧tuple、updateはowner/place/kind/enabledを含む旧新tuple双方を対象にする。bindingが無ければroute writeは0で、place/binding/membershipを先回りで作らない。start/stopはdesired `enabled`とlifecycle/connectionだけを更新し、`subject_routes`を読まない・書かない。

migrationは、会話履歴materializationが作った `place_source_refs(source_system='discord',classification!=config_only)` だけからscope/bindingをseedし、config-only placeと履歴に無いchannelはruntime lazy bindへ残す。`address_kind` の判定根拠はlegacy sourceの実在fieldだけに固定する。まず `sessions.metadata_json` のobjectで `source='discord'`、`channel_id`がaddressとbyte一致、`is_dm=false`、`guild_id`がnonempty stringの寄与、または `memory_sessions.session_id`（対応する`session_id`、`sessions.id`も同形式）を現行 `discord-{agent_id}-{guild_id}-{channel_id}` として右からparseしguild componentがnonempty numericかつchannelがaddress一致する寄与を **guild evidence** とする。一件でもguild evidenceがあれば`address_kind=guild`とし、nonempty guild IDが全寄与でbyte一致する時だけその値を`guild_id`へ入れる。不一致は`conflicting_binding_class`である。guild evidenceが0件の時だけ、同metadataで`is_dm=true`、または同session ID parseでguild componentがemptyかつchannel一致する寄与を **DM evidence** とし、一件以上なら`address_kind=dm,guild_id=NULL`とする。`dm_user_id`はnonempty stringの時だけcurrent-trust principal候補へ使い、DM分類自体の必須条件にはしない。どちらのevidenceも無ければ`address_kind=unknown,guild_id=NULL`とする。値やaddress形からこれ以外の推測をしない。

binding/scope分類中にrouteを出してはならない。最初に全source rowを分類し、policy/default、schedule、effective tool visibility、membership、instance/config participant、全bindingのcanonical集合とraw outcomeを確定する。その後の一回だけのroute passで、materialize済みbindingを持つ全migration `(subject,place,kind)` をbyte昇順に列挙し、各tupleへ`ReconcileSubjectRoutes`を一回適用する。classified guildはwhitelist、classified DMは現在の`IsCurrentDiscordDmPrincipal`、unknownはroute 0となる。これによりsource列挙順を変えてもroute集合と各`binding_id`は同一になる。分類失敗やanchor ambiguityは寄与集合を `legacy_unowned_source_rows` へ非空reason付きで置き、別tupleのpassは続ける。

runtimeはmigration transformを呼ばず、store command `ObserveGateAddress(instance_id,address,address_kind,author_external_id,guild_id?,label_update,observed_at)` を**既知/未知bindingの全dispatchで**一回呼ぶ。`address_kind` / `guild_id` / `label_update=present(string)|absent` は§2.2の同じ`event.discovery` carrierから取り、`guild`だけ`guild_id`必須、他はNULLである。command内部のpurpose導出は上の`DeclaredDiscordOperations()`を呼ぶ。`observed_at` はwire値ではなく、coreがLF終端までevent一行を受信した時に一度だけ採る**core受信時刻**であり、その同じ値をcommandと新規rowへ渡す。runtimeで新しく作るscope/binding UUIDはUUIDv7である。

commandは一transactionで次を順に行い、順序を入れ替えない。

1. active instance/revisionと`observed_participants`を読む。dedicatedはowner一人、sharedはactive revisionの`config_bytes.agent_ids`をsubjectへexact-one解決し重複collapseする。解決0/複数は`participant_resolution_error`でrollbackする。
2. Discord source refをread-only lookupする。1件または既知bindingなら、そのplaceで既に`purpose=inbound` routeを持つsubjectを加えた和集合を`admission_candidates`とする。0件では`observed_participants`だけであり、guild policyはhard defaultまで解く。複数なら`binding_ambiguous`。live threadは常にeligible 0。live guildは候補subjectごとに `ResolvePlacePolicy` のsubject override→place default→hard default `whitelisted=false` を解き、trueだけeligible。live DMは候補subjectごとに観測instanceと`author_external_id`を同じ`IsCurrentDiscordDmPrincipal`へ渡してtrueだけeligibleとし、channel policyでDMを開けない。既知bindingの保存済み`address_kind=guild|dm`とlive kindが違えば`binding_metadata_conflict`、保存値unknownだけはlive guild/DM規則でadmissionする。
3. eligible 0なら `rejected` を返し、Eventをappendせず、place/source ref/scope/binding/membership/route/dedupを含むdurable writeを一件も行わない。
4. eligibleが一人以上なら、source ref 1件を再利用し、0件なら`classification=live`のplace/source refを`observed_at`で各一件作る。再利用行が`classification=config_only`なら、同じ`ObserveGateAddress` transactionで`classification=live`へ遷移させる。これはそのaddressの最初のadmitted live観測だけで起こる**一方向・一回**の遷移であり、既にliveまたは他classificationならclassification write 0とする。admitted DMでは、この遷移後に新規/既存を問わず同じsource refの `place_source_refs.metadata.dm_user_id` をexact `author_external_id`へ設定する。metadataがNULLならobjectを作り、既存objectなら他のmemberを保持して`dm_user_id`だけを置換する。既存source refの`updated_at`等は保存し、config-only再利用時はclassificationだけを例外として変更する。既存metadataがobject以外なら独立outcome `source_ref_metadata_shape_conflict` として全transactionをrollbackし、別表・wrapper・raw fallbackを作らない。この時はclassification遷移を含む全writeが戻り **rollback / Event 0** である。このclassification/evidence writeは、後述のmembershipと`ReconcileSubjectRoutes`より先、かつEvent appendより前に、この`ObserveGateAddress` transactionで行う。guildではprincipal fieldを書かないが、config-only再利用時のclassification遷移は同じく行う。scope/bindingが未知ならUUIDv7で各一件作り、binding metadataはlive kindをそのまま`address_kind=guild|dm`、guildだけobserved `guild_id`、DMはNULLとする。既存bindingはplaceが違えば`binding_metadata_conflict`でrollbackする。保存済み`address_kind=unknown`は最初のeligible live guild/DM観測で対応するkindへ一回だけ遷移し、guildなら同時にnon-NULL guild IDを入れる。保存済みkindとlive kindが違う場合、またはguildの保存値と観測値がともにnon-NULLで異なる場合は`binding_metadata_conflict`、同kind・同guild IDはbinding metadata write 0である。threadはstep 3で終わるのでunknownを含むbindingを更新しない。enrich時はcanonical `binding_metadata_bytes`を再生成し、そのSHA-256で`binding_metadata_digest`を更新する唯一のruntime writerをこの`ObserveGateAddress` transactionに固定し、片方だけの更新を許さない。`label`はequivalence/conflict対象にせず、`label_update`がpresentの時だけ同じtransactionでその値へ更新し、absent（dispatchに値なし、cache/HTTP取得失敗を含む）なら既存値を保持してwrite 0とする。step 5のmembership確定後、step 6で同じtransaction内の`ReconcileSubjectRoutes`を必ず実行するため、source-ref classification、DM principal evidence、unknown→guild|dm enrichment、route再整合は不可分である。後続stepを含むtransactionが失敗すればsource-ref classification/metadata、binding metadata/digest、label、membership、routeを全てrollbackする。
5. transaction開始後、Event appendより前に、そのplaceのlatest seqを一度読む。各eligible subjectへ `EnsureAdmittedDiscordMembership` を呼ぶ。この関数だけがDiscord dispatch由来membershipを書く。不存在なら `role=participant,joined_at=observed_at,shared_seen_seq=transaction-start latest place seq` で一件insertする。既存participantなら全field write 0、既存observerならroleだけparticipantへ上げjoined_at/cursorは保存する。これが既知binding dispatchを含むmembership作成の唯一のwriterである。設定変更後のroute admission再評価は`RecomputeDiscordRouteAdmission`だけが行い、membershipは変更しない。
6. `eligible ∩ observed_participants` の各subjectについて `(subject,place,kind)` 全体へ `ReconcileSubjectRoutes(subject,place,kind)` を一回呼ぶ。この内部のroute admissionは `observed_author_external_id=NULL` とし、step 4で永続化済みのDM principal evidenceだけを読む。今回dedicated bindingが初めてmaterializeされたownerはdedicated anchorへ、sharedだけならshared anchorへ全purposeがexactに一致し、既存purposeごとの部分更新はしない。
7. route再整合後、eligible集合を現在`purpose=inbound`を持つsubjectと交差した`admitted_fanout_subjects`を返す。`ready(place,binding,admitted_fanout_subjects)` の後だけcallerがdedup/Event append/fan-outへ進む。DBのUNIQUE競合は再読し、placeとguild IDへstep 4のNULL enrichment/non-NULL conflict規則を再適用し、labelは競合にせずpresent値だけ更新、absentなら保持して既存rowへ収束する。その他のstore errorはtransaction全rollbackの`store_error`でEvent 0とする。

command outcomeは `rejected` / `ready` / `binding_ambiguous` / `binding_metadata_conflict` / `source_ref_metadata_shape_conflict` / `participant_resolution_error` / `store_error` / `concurrent_equivalent` の8件で固定する。`source_ref_metadata_shape_conflict`はgeneric `store_error`へ畳まない独立outcomeであり、選択したsource refの既存metadataがnon-NULL objectでない時に、classification遷移を含むtransaction全体を **rollback / Event 0** とする。

新参加者へ過去のshared Eventを遡及開示しないことを製品判断とする。このため初回membershipの`shared_seen_seq`は0固定やEvent append後のseqではなく、上記transaction開始時のlatest seqである。初回拒否後にpolicy/trustが変わった場合も、設定writerはmembershipを作らず、次のdispatchで同じcommandが再評価して一度だけ成立させる。

### 1.3 subject policy は place scoped、binding 非依存

policy は次の一関係だけである。

```text
place_subject_policies(
  place_id, kind_id, subject_id,
  readable, writable, whitelisted, admission,
  heartbeat_enabled, heartbeat_interval_secs?, heartbeat_instructions,
  instructions_revision, source_row?, source_updated_at,
  PRIMARY KEY(place_id,kind_id,subject_id)
)
```

既存channel rowのpolicy値とheartbeat 3値をstrict typed field（boolean、nullable i64、text）として同じ行へ置く。migrationでは`admission=open`,`instructions_revision=0`、runtime updateではrevisionを単調増加させる。channel heartbeatを持たないkindの新規writeは`heartbeat_enabled=false,heartbeat_interval_secs=NULL,heartbeat_instructions=''`を明示して作る。同一 `(place,kind,address)` の shared/dedicated binding が何本あっても、policy lookup は一行だけである。event を最初に観測した instance によって結果は変わらず、複製 drift も起きない。

subject を持たない `discord_channel_config.agent_id=''` は `place_default_policies` に一 source row のまま運ぶ。

```text
place_default_policies(
  default_id       uuid PRIMARY KEY,
  kind_id          text NOT NULL,
  place_id         integer_id NULL,
  resolution       enum(active,ambiguous_place,
                        invalid_runtime_fields,conflicting_default) NOT NULL,
  source_row       schema(discord-channel-config-source-row-v1) NOT NULL,
  source_updated_at utc_nanos NULL,
  UNIQUE(place_id,kind_id) WHERE resolution='active'
)
```

`discord-channel-config-source-row-v1` は source の 11 column を ordinal 順の named objectとして持ち、各値は `TARGET-MAPPING.json` の `copy-sqlite-value-v1` が定義する canonical SQLite value である。**ordinalの唯一の権威はfrozen source snapshotに対する`PRAGMA table_info`で、mapping/transformとのjoinはcolumn nameで行う。** fresh DDL順やmappingに記録されたordinalをhard-codeしない。migration fixtureも同じPRAGMA/name joinで作る。global rowは11値を `place_default_policies.source_row`、known rowは同じ11値を `place_subject_policies.source_row` に保持する。現 mapping の根拠は transform `discord-channel-policy-router-v1` と11 entry `db:discord_channel_config.*`、現 source DDL は `e6f0595:crates/db/src/schema/sql.rs:509-523` である。

global row の channel→place 解決は次で固定する。

| 解決結果 | row outcome | runtime |
|---|---|---|
| exactly 1 place、かつ同じ `(place,kind)` の global row も一つ | `place_id` を入れ `active` | policy/default heartbeat に参加 |
| 0 place | `resolve-or-create-config-place-v1` で config-only place/source ref を作り `active` | policy/default heartbeat に参加 |
| 複数 place | `place_id=NULL`, `ambiguous_place` | 不参加。選ばない |
| 複数 source row が同じ `(place,kind)` へ解決 | 全該当 rowを `place_id=NULL`, `conflicting_default` | 不参加。勝者を選ばない |
| bool等の非canonical storage class | 該当logical classを失敗として会計し、complete source rowを最大一件raw。他の独立classはcanonical可 | 該当classだけ不参加 |

`source_row` の11値はすべてcanonical SQLite valueとしてopaqueに保持する。`readable/writable/whitelisted/heartbeat_enabled` はSQLite INTEGER 0=false、nonzero=trueとする。これはdecoderがnonzero=true、列挙SQL predicateが`=1`という現物の二経路を**nonzero=trueへ統一する裁定**であり、「現物の全readerと一致」を根拠にしない。別storage class、非NULL destinationに必要なNULLだけが該当policy logical classのnoncanonical outcomeである。`heartbeat_interval_secs` はNULLまたは任意のsigned i64を生値のまま運び、負値をraw化もu64化もしない。`heartbeat_instructions` はTEXTとする。

channelの未読`updated_at`は機能成立条件にしない。snapshotごとにmetadataのcaptured-atを一つの定数として確定し、config-only `places.created_at`、`place_source_refs.updated_at`、known subjectの非NULL `place_subject_policies.source_updated_at`は、source値がTEXTで`parse-utc-nanos-v1`成功ならその値、NULL/非TEXT/parse不能/overflowならcaptured-at定数を使う。runtimeはtransaction captured-atを使う。parse失敗だけをpolicy/raw理由にせず、exact source値はdefault/subject policyの`source_row`に残す。interaction等、source reader自身が時刻を機能入力にする別transformのparse failure規則は変えない。dedicated configの読まれない`updated_at`もparseせずschema-bound raw valueとして運ぶ。

policy 解決は次の一関数 `ResolvePlacePolicy(subject,place,kind)` に固定する。

1. `place_subject_policies(place,kind,subject)`。
2. active `place_default_policies(place,kind)` の typed `readable/writable/whitelisted`。
3. hard default `readable=true,writable=true,whitelisted=false,admission=open,instructions_revision=0`。

既存の subject→global→hard default は `e6f0595:crates/db/src/queries/channel_config.rs:248-300` と `e6f0595:crates/discord/src/gateway_actions/discord_ops.rs:111-153` にある。legacy に無い `admission` は `open` とし、whitelist と同一視しない。

Discord ingress admissionはgeneric policy field `admission` ではなく、§1.2 `ObserveGateAddress` の一規則である。guildは上の解決順で得た`whitelisted`だけを使い、未設定はfalse、DMはtrusted/ownerだけ、threadは常に拒否する。`readable/writable/admission=open`のどれもこの拒否を上書きしない。

heartbeat は `ResolveHeartbeat(subject,place,kind)` で解く。

1. subject/place の明示 `schedules(kind=heartbeat)` があればそれを使う。
2. 無く、`place_subject_policies`があればそのheartbeat 3値を使う。
3. 無く、active `place_default_policies(place,kind)` があれば、admitted participantについてその source row の `heartbeat_enabled`、`heartbeat_interval_secs`、`heartbeat_instructions` を使う。既存`timed` routeの有無は読まない。interval NULL は運用既定、instructions `''` は subject既定instructionを意味する。
4. default row 自体が無ければ place heartbeat は absent（暗黙に enabled にしない）。

global row を既存 subjectへ展開しない。後からadmitted participantが増えた時も、subject override→active place defaultを直接解いて同じ単一defaultを使う。`ResolveHeartbeat`と`DeriveDiscordRoutePurposes`は既存`timed` routeを入力にしない。既存 heartbeat reader/writer の出典は `e6f0595:crates/db/src/queries/channel_config.rs:68-97,195-218` である。

### 1.4 未 binding channel の config

公開 `discord_channel_config` は conversation binding を要求しない。既存経路が任意 `channel_id/guild_id` を upsertする出典は `e6f0595:crates/discord/src/gateway_actions/discord_ops.rs:158-280` である。

runtime と migration は同じ store operation `resolve-or-create-config-place-v1` を使い、次の順で行う。

1. `(source_system='discord',source_address=channel_id)` の `place_source_refs` を lookupする。
2. 0 件なら deterministic config-only place と source ref を作る。1 件ならその place を使う。複数だけを `channel_place_ambiguous` とし、runtime は書かず、migration は寄与集合を raw にする。
3. `place_subject_policies(place_id,'discord',subject_id)` をupsertする。heartbeat引数は同じrowのheartbeat 3 fieldを、未指定値は既存row（無ければlegacy既定 true/NULL/empty）を保つpatch semanticsで更新する。
4. `gate_bindings`、membership は作らない。既存bindingが無ければ`subject_routes`も作らない。既存bindingがあるpolicy更新では、同transactionの末尾に§1.2の `RecomputeDiscordRouteAdmission` → `ReconcileSubjectRoutes` だけを実行する。後の最初のadmitted live観測は同じ source ref の place を使い、`ObserveGateAddress` transaction内でその行だけを`classification=config_only → live`へ遷移させる。

これにより未 binding channel を設定できる一方、設定操作が ingress や会話 route を暗黙に開かない。

config-only `places` は `parent_id/inherit_* = NULL`、`policy=hard-default`、`public_key` は `place-public-key-v1` の `config:discord:<channel-address>`、`created_at` とsource ref `updated_at`は直前のparse-if-possible/captured-at規則で必ず非NULLにする。migrationのinteger IDはsource-backed placeの後でconverterのgolden-tested `(source_system,source_address)` 順に採番し、runtimeはUNIQUE source-ref transaction内で次IDを採る。`place_source_refs.classification=config_only` である間だけsession専用fieldは全てNULL、`source_id`はexact source-address bytesであり、`(source_system,source_address)`はUNIQUEである。admitted初観測で`classification=live`へ遷移した後、このconfig-only用の全session field NULL制約は適用しない。source refは同じ一行を使い、別のlive行を追加しない。

### 1.5 instance-scoped identity

identity lookup は `gate_subject_identities(instance_id,external_id)` だけを使う。entity は `{instance_id,external_id,subject_id,display_name?}`、主キーは `(instance_id,external_id)` とする。kindはinstanceから分かり、bot/owner external IDはrevisionのschema-bound config bytesに入る。sharedのbot IDはREST current-userで得るため、保存値を捏造しない。

`resolve-external-principal-v1` の migration phase は次で固定する。

1. 先に全 source から `gate_instances` を作り終える。
2. platform ごとに、**その migration run が作った**全 instanceのうち `gate_instances.kind_id=normalized(platform)` の集合を確定し、UUID byte順にsortする。dedicated、config由来shared、worktree compatibility instanceをすべて含む。runtime既存 instance と別 platformは含めない。
3. source principal の subject public IDを既存規則 `external:<platform>:<base64url(raw external id)>` で一度だけ resolve/createする。
4. sorted instanceごとに `(instance_id,external_id)->same subject_id` を一行生成する。生成結果は source row digest + instance UUID で再実行同一となる。
5. 同じ keyへの全寄与が同じ subjectなら一行へ畳む。subjectが衝突する時は全寄与を `legacy_unowned_source_rows` へ `reason=identity_conflict` で置き、identityを作らない。display名だけが異なる時は canonical rowの `display_name=NULL`、各原値は raw sourceに残す。

この全instance展開は `TARGET-SCHEMA.json` / `TARGET-MAPPING.json` の同一 normalized contractと `GATE-CONFIG-MAP.json` のcardinalityが正本であり、exactly-one instance規則は無い。

**migration 後に追加した instanceへ既存 identityを暗黙複製しない。** Discord/Nostr config PUTがそのkindのinstanceを新設するtransactionは、文書化されたcommand効果として次の二由来を列挙する。(a) `grant_source_provenance`/grant aggregateの公開principal、(b) 同kind既存instanceの`gate_subject_identities`（worktree移行由来を含む）。両集合を`(external_id,subject_id)`でcollapseし、external_id一件が複数subjectへ解けたらinstance/config/identity全writeをrollbackする。canonical集合を`(external_id bytes,subject_id)`順にsortし、新instanceへ `RegisterGateIdentity(instance_id,external_id,subject_id,display_name?)` を一回ずつ呼ぶ。display名が一致しなければNULLを選び原値は既存provenanceに残す。新しいkind registryは作らない。既存instanceのPUTではこの列挙writeを繰り返さない。下記trusted-user commandも同じ明示APIを呼ぶ。これはinstance新設commandの明示した一効果である。

公開trusted-user 4 operationは全て `gate_instances` を読む。GETはidentityを `(normalized kind,external_id,subject_id)` でgroupして公開一行へ畳み、instanceごとのcopyを公開row数へ数えない。同groupのdisplay名がbyte一致ならその値、異なればNULLを返し、原値はprovenanceに残す。POSTは選択kindの現在の全instanceへ同一identityを一transactionで作る（0件はoperation error、複数は正常）。PATCHは現在の全instanceへ同じidentityをupsert/updateしてgrant aggregateを一度だけ更新する。DELETEは、削除前に対象subjectのmaterialize済みDiscord DM tupleをbyte昇順に列挙し、現在の全instanceから対応identityとgrant aggregateを除いた後、同じtransactionで各tupleへ`RecomputeDiscordRouteAdmission`→`ReconcileSubjectRoutes`を一回適用する。membershipは履歴として保存し、別のcurrent trusted/owner principalがそのplaceに無ければroute 0にする。これにより公開 1 trusted-user と instanceごとの複数identityの対応をmigration/runtimeで同じにする。

### 1.6 route snapshot

```text
GateRoute {
  subject_id,
  place_id,
  kind_id,
  instance_id,
  binding_id,
  address,
  connection_epoch,
  revision,
  purpose
}
```

通常 reply/typing は binding の address を使う。public tool が任意 `channel_id` を受ける場合だけ、binding は credential snapshot、`address` は検証済み引数とする（§8.3）。

## 2. protocol 2

### 2.1 framing と protocol-1 adapter

protocol 2 は local stream 上の UTF-8 JSON、1 object/LF 1 行、1 行最大 1 MiB とする。根拠は `crates/plugd/src/lib.rs:1-5,27-28,889-940`。protocol 1 の web/nostr hello source は `crates/web-gate/src/main.rs:360-380` と `crates/nostr-gate/src/main.rs:192-208` である。

protocol-1 connection は ingress直後に `kind_id=hello.name`、DB割当済み compatibility instance、`origin_scope=instance`、`protocol=1` へ一度だけ変換する。adapterは`hello.name`から同じlocator `compat:<name>`と`gate-instance-id-v1`でUUIDを導出し、store queryで`instance_id=<derived UUID>,kind_id=hello.name,active_revision=1`をexact-oneに解決する。0件は`instance_unknown`でfail-loudとし、このquery/hello処理はinstanceを自動作成しない。主キー不変条件に反する複数結果も接続失敗である。以後の port/plugd/runtime/store は kind/instance型だけを扱う。web/nostr の routeも §1.2 の `subject_routes` を読む。

### 2.2 message

`?` は任意、`[]` は array。top-level unknown field は requestごとに `unknown_field` とする。

| 方向/message | field | 成功応答 |
|---|---|---|
| gate→core `hello` | `id,m,protocol,kind_id,instance_id,revision,origin_scope,address_form,ingress_discovery,tools,effects,capabilities,actions?` | `ok:{protocol:2,connection_epoch:u64}` |
| gate→core `ready` | `id,m,connection_epoch` | `ok:{}` |
| gate→core `failed` | `id,m,connection_epoch,code` | `ok:{}` 後 close |
| core→gate `bind`/`unbind` | `id,m,address` | `ok:{}`。`ingress_discovery=prebound`だけに送る |
| gate→core `event` | `id,m,kind,address,author,content?,mentions?,reply_to?,origin?,attachments?,action?,discovery?` | `ok:{seq:i64|null}` |
| gate→core `read` | `id,m,address,from?,limit?` | `ok:{events,next?}` |
| core→gate `effect` | `id,m,address,kind,payload,target?,verb?` | `ok:{delivered,origin?}` |
| core→gate `tool` | `id,m,name,args` | `ok:{result:<JSON text>}` |
| core→gate `activity` | `m,address,activity_id,kind?,state,label?` | 応答なし |
| response | `id,ok` または `id,err` | request対応 |

hello検査成功で core は `connecting` epochを作る。Discord REST current-user + Gateway READY後の `ready` でのみ `active` にする。runner由来の非secret boot errorは child がhello後に `failed(code)` として報告する。coreだけが connection/lifecycleを書く（§5）。

UI payload は既存 `A2uiComponent` / `A2uiUserAction`（`e6f0595:crates/core/src/a2ui.rs:7-123`）を使う。

`event.discovery`はmembership kindで必須、prebound kindでは禁止する同一carrierであり、wire/portのexact shapeを次に固定する。port型は`GateEvent.discovery: Option<MembershipDiscovery>`、内側は`MembershipDiscovery { address_kind, guild_id, label }`である。

```text
discovery = {
  address_kind: "guild" | "dm" | "thread",  // 必須
  guild_id?: string,                           // guildだけ必須かつnonempty、dm/threadでは禁止
  label?: string                               // 任意。値がpresentの時だけstore更新
}
```

`discovery`内のunknown field、`address_kind`欠落/enum外、guildの`guild_id`欠落/空文字、dm/threadの`guild_id`存在、presentな`guild_id`/`label`の非stringはrequest単位のvalidation errorでEvent/store write 0、connection維持とする。`null`をpresentな値としては認めない。label省略は`absent`でありNULL代入ではない。

Message CreateとButton/Select/Modalの全producerはDiscord adapterの純粋な一入口 `ResolveDiscordDiscovery(dispatch,cache,http)` を呼ぶ。source priorityは、dispatch自身のchannel object/type/name/guild ID fieldがpresentならそれ、次にSerenity cache、最後にDiscord HTTP channel lookupである。dispatch fieldだけでtypeが確定しても、不足するguild IDまたはlabelは同じcache→HTTP順で補う。interactionのoptional channelが欠落しても同じlookupへ進み、addressやguild ID有無だけからtypeを推測しない。labelはdispatch/cache/HTTPの最初のpresent stringをcarrierへ入れ、全sourceで値なしまたはlookup失敗なら省略する。省略時、`ObserveGateAddress`は既存labelを保持する。

Serenity 0.12.5 `ChannelType` の写像は次で全variantを固定する。

| Serenity variant | `address_kind` |
|---|---|
| `Private`, `GroupDm` | `dm` |
| `NewsThread`, `PublicThread`, `PrivateThread` | `thread` |
| `Text`, `Voice`, `Category`, `News`, `Stage`, `Directory`, `Forum` | `guild` |
| `Unknown(_)` および将来のnon-exhaustive variant | 解決失敗 |

cacheとHTTPを尽くしてもtypeを確定できない、type確定に必要なlookupが失敗する、またはunknown/新variantならfail-closedで当該dispatchをdropする。labelだけのlookup失敗はdrop理由ではなくlabel absentである。wire eventをcoreへ送らないためEvent/store writeは0、connectionは維持し、instance/address/dispatch kindとstable reason `discord_discovery_unresolved` をlogする。interactionはDiscordが要求するdefer/modal等のmechanical ACKを先に完了してからdropし、Message Createのようにplatform ACKが無いdispatchには追加送信しない。guild写像ではdispatch自身を優先して得たnonempty guild ID、無ければ解決済みguild channelのguild IDを必須とし、最後まで無ければ同じfail-closed outcomeとする。`observed_at`はcarrierに含めず、coreが完全なevent行を受信した時のcore受信時刻を採る。

```text
ui create  = {mode:"create",interaction_id,surface_id,components:A2uiComponent[]}
ui disable = {mode:"disable",interaction_id,message_id,timeout:boolean}
event.action = {surface_id,component_id,action_name,context:JSON|null,responder_id}
```

### 2.3 validation

worktree v1の根拠は `crates/plugd/src/lib.rs:307-330,333-438,441-600,696-732,823-862,986-1111`。v11も次を固定する。

- top-levelはstrict。tools/actionsのunknown nested fieldは無視し、wire順とduplicateを保持する。effects/capabilitiesはset化する。
- authorはobject + string id必須、contentはobjectでなければ空、mentionsはstring array、attachmentsはunknown fieldも拒否する。
- `said` は nonempty origin、`ui_action` は nonempty origin/actionを必須とする。
- membership kindのeventは上記`discovery`を必須、prebound kindでは禁止し、nested exact shapeとkind依存の`guild_id`条件をtyped `GateEvent`構築前に検査する。
- `kind_id` grammar違反は `bad_kind_id`、canonical UUID違反は `bad_instance_id`、error後close。
- prebound kindのbind/unbind addressはkind regex、tool名は宣言済みoperation、argsはobject。membership kindへcoreはbind/unbindを送らず、受信したmembership kind用bind/unbind responseはunknown request IDとして無視する。activity違反行は捨てconnection維持。
- responseはstring id必須。unknown/expired idは無視。`ok`と`err`両方なら既存どおり`ok`、双方無しは`response_invalid`、非string error codeは`unknown`。
- UI payloadのmode別必須fieldと型を検査する。一般 edit APIは足さない。

### 2.4 error/close

| 条件 | wire/connection | store/caller |
|---|---|---|
| 1 MiB超 | `too_large` 後close | 未完sendingは`indeterminate` |
| invalid UTF-8/JSON/non-object | 応答なしclose | 同上 |
| hello 10秒超、hello前の別message、二回目hello | close | connection `failed` |
| undeclared/disabled/kind/revision/spec不一致、active重複 |安定code後close | 当該instanceだけ`failed` |
| ready前のevent/effect/tool/activity | `instance_not_ready` | 副作用なし |
| ready後の未知m | `unknown_message@m`、keep | request failure |
| validation/unbound/ambiguous | error、keep | Eventなし。`unbound/not_bound`はprebound kindだけ |
| membership kindの`read` | `err:{code:"membership_read_unsupported",at:"read"}`、keep | store lookupなし、Event/writeなし |
| Discord request受理後に結果不明 | `indeterminate` | 自動再送なし |

### 2.5 kind spec

```text
GateKindSpec {
  kind_id, origin_scope, address_form, ingress_discovery,
  tools: Vec<ToolDef>, effects: BTreeSet<EffectKind>,
  capabilities: BTreeSet<Capability>, actions: Vec<ActionDef>
}
```

`ingress_discovery` は `prebound|membership`。Discordは`membership`、protocol-1 compatibility kindは`prebound`で固定する。core→gateのwire `bind/unbind`、接続/再接続時の`rebind`、binding未登録eventの`not_bound`は**prebound kind専用**である。membership kindではDiscord上のmembershipが購読境界なので、この三つを送らず、binding rowはcore/storeの選択済み状態としてだけ持つ。membership eventはbinding 0件でも`not_bound`へ変換せず、必ず§1.2 `ObserveGateAddress`のadmissionへ渡す。membership kindの`read`はaddress/bindingの有無にかかわらずstore lookup前にstable error `membership_read_unsupported`で拒否してconnectionを維持し、Discord adapterは`read`を送らない。Discord gateはREADY後、自botがmemberであるchannelのdispatchをconfig row有無にかかわらず全件forwardする。

parse後の型へ `PartialEq + Eq` を実装して比較する。tools/actionsはwire順・duplicate保持、effects/capabilitiesはset。canonical JSON、sort、digest、SHA-256は作らない。最初のactive instanceがkind spec/tool indexをmemory registryへ登録し、最後のactive instance切断で消す。既存箇所は `crates/plugd/src/lib.rs:405-438,646-655` と `crates/social-runtime/src/lib.rs:816-868`。

## 3. ingress、identity、dedup、policy

coreは `ingress_discovery=prebound`だけ `(instance_id,address)` のbinding 0件を`not_bound`にする。Discordの`membership`はbindingの有無にかかわらず全dispatchを§1.2 `ObserveGateAddress`へ渡す。commandがadmissionを先に確定し、eligible 0ならEventと全durable writeが0、eligibleがいればplace/source ref/scope/binding/membership/routeを一transactionでensureした後にだけ同じdispatchを処理する。新規writeではDBの `UNIQUE(instance_id,address)` とstore transactionが一件性を保証する。legacy duplicate/異常行は §6 のraw entityへ入りruntime bindingにならないため、live lookupに複数件は存在しない。もしDB破損で複数なら`binding_ambiguous`としてEventを作らない。

Discordは `external_origin_scopes.mode=kind_address`、dedup keyは `(kind_id,address,origin)`。最初の観測だけを Event / external origin / external ref と同じ transactionでappendし、後続instanceは同じseqをackする。author/content差は`origin_conflict`で最初を変えない。

dedup 後の place Event は、`ObserveGateAddress` が今回返した`admitted_fanout_subjects`だけへ各一回fan-outする。この集合は観測instanceのparticipantだけでなく、そのplace/kindに既にinbound routeを持つ全subjectを同じsubject別admissionへ通して作るため、観測bindingと各subjectの選択済みbindingの一致を条件にしない。`AppendEvent` は競合を含め `inserted(seq)|existing(seq)` のどちらか一つを返し、`inserted` の呼出元だけが主キーで一意なroute行をsubject ID順に一回ずつenqueueする。`existing` を受けた後続instanceは同じseqをackするだけでfan-outしない。`AppendEvent`はroute再整合writerではない。admitted DMではprincipal evidenceとrouteが先行する`ObserveGateAddress` transactionへ共にcommit済みなので、別writerと`AppendEvent`の実行順や両者間のprocess crashでroute結果を変えない。別instanceからの同じmessageでも既存route subjectのadmission集合は同じ入力へ解け、外部instanceの到着順は発火回数を変えない。

authorは `(instance_id,author.id)` でidentityを引く。policyはbindingでなく `ResolvePlacePolicy(subject,place,kind)` を一度だけ解く。この二つにより、shared/dedicatedのどれが先に同じmessageを観測してもauthorとpolicy結果が同じになる。

| Discord input | 変換 |
|---|---|
| Message Create | `said`; message ID→origin、user ID→author.id、本文+既存添付注記→content。自botだけ除外し他botは通す（`e6f0595:crates/discord/src/gateway.rs:312-327,455-499`） |
| image attachment | `{kind,url,origin_author}`。非画像は本文注記だけ（`crates/port/src/lib.rs:214-248`、同gateway `:330-440`） |
| Button/Select/Modal | Discord ACK/modal response後、`ui_action`へ写す（同gateway `:522-655`） |

UI actionは `interactions.binding_id` の instance と `source_address` も照合する。NULL binding、別instance/address、非pending、期限後はEventを作らない。

## 4. effect、operation、queue

### 4.1 delivery/operation

新規 effect は既存 target `deliveries` の厳格状態機械を使う（entity id、`TARGET-SCHEMA.json:1214`）。

```text
prepared -> sending -> delivered | failed | indeterminate
```

enqueue前のvalidation/no active epochは`failed`、外部APIへ渡した後のtimeout/drop/crashは`indeterminate`。terminalは変えず、late/wrong-epoch ackは`delivery_observations`へ記録する。retry/reconcile/fallbackはしない。sayは長文分割後の最後のmessage IDだけをorigin/refにし、reactはoriginなし。UI createだけmessage IDを内部返却し、公開`send_ui`結果には含めない。

gate operationは`gate_operations`の同型状態機械を使う。GETのdefinite error/timeoutは`failed`、外部mutation受理後だけ`indeterminate`を許す。

### 4.2 activity

turn開始時に `purpose=outbound` の `GateRoute` 一つを `Notice::Activity*` に載せ、そのinstance/addressだけへtypingを出す。routeなし/non-readyなら送らない。typing failureはEvent/effect/deliveryを作らない。置換元は `crates/port/src/lib.rs:548-572` と `crates/plugd/src/lib.rs:763-819`。

### 4.3 queue/deadline

Discord ingress queueはmessage 256、interaction 64、`send().await` backpressure（`e6f0595:crates/discord/src/gateway.rs:103-125`）。writer queueは各方向256。event/responseは空き待ち、effect/tool/bindはcaller deadline内だけ待つ、activityは`try_send`で一件drop。未送信Eventのbackfillはしない。

deadlineはhello 10秒、prebound kindのbind/unbind 60秒、effect 300秒（`crates/plugd/src/lib.rs:21-28`）。toolは所属activityのdeadlineだけ（同`:823-861`）。membership kindにbind deadlineは無い。gate独自retry deadlineは足さない。

## 5. process、secret、readiness、launcher

### 5.1 runner は selector + exec

`crates/app/src/bin/opencrab-gate-runner.rs` を小さなRust binaryとして追加する。前例は `crates/app/src/bin/opencrab-lock-fd.rs:1-64` と `dev.sh:61-75,240-265`。

`dev.sh` はrunnerへ DB path、instance UUID、core socket、adapter binaryという非secret selectorだけをargvで渡す。runnerはDBをread-onlyで一回読み、active revision/config/secretを解決する。`source-plaintext` はbytes、`enc:v1` は `OPENCRAB_SECRET_MASTER_KEY` で復号、`opaque` はboot error。master key/envelopeは `e6f0595:crates/core/src/secret_box.rs:21-90` と同じである。

成功時はchild envへinstance/revision/socket/tokenを設定し、master keyを除去して **`exec`** する。runnerは常駐せず、PIDはそのままgate childになる。token/master-key bufferはexec直前にzeroizeし、secretをargv/stdout/stderr/temp/statusへ出さない。

config/secret解決失敗でもrunner自身がDBを書かない。tokenなしと非secret `OPENCRAB_GATE_BOOT_ERROR_CODE` を設定して同じchildへexecする。childはcoreへhelloし、`failed(code)`を送り終了する。DB自体を開けない、adapterをexecできない場合だけhello前終了となり、statusは`starting/no_connection`とprocess exitを表示する。

### 5.2 writer は core 一つ

`gate_instances.lifecycle` と `gate_connections` のwriterはcore/store一つだけである。runner、gate child、`dev.sh` はDBを書かない。

1. core start planがenabled active revisionを`starting`にする。
2. child hello受理でcoreがinstanceごとの`MAX(epoch)+1`を採番し`connecting`。
3. Discord token検査→REST current-user→Gateway Identify/READY後の`ready`で`active`かつlifecycle=`running`。
4. boot/fatal errorの`failed(code)`、socket close、明示stopをcoreが`failed|closed`と`stopped`へ書く。
5. reconnect READYは新しいcore connection/epochを作る。

readinessは「active revisionが`present=true,enabled=true`で、そのrevisionのlatest epochがactive」だけ。`present=false` tombstoneや`enabled=false` configは起動対象でない。Discord専用HTTP、port、nonce、ready tokenは作らない。`MESSAGE_CONTENT`拒否、REST 401、fatal Gateway closeは当該instance failureで、contentなし縮退はしない。intentsは `GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT`。既存voice込み値の出典は `e6f0595:crates/discord/src/gateway.rs:142-148`。

### 5.3 projection file は廃止

`$DEV_DIR/gate-connections.json` を作らない。`opencrab gate-status --db <path>` はread-only helperで、enabled instanceと各active revisionのlatest connectionを一つのsnapshot transactionでqueryする。token、external bot/owner ID、address、運用labelは表示しない。

`dev.sh status` はhelper結果と既存PID ownershipを照合する。判定は次で固定する。

| 状態 | exit/表示 |
|---|---|
| core PIDなし | 2 / `stopped` |
| coreあり、enabled 0件 | 0 / `running (healthy)` |
| 全enabled instanceがactive revision/active epoch/errorなし、PID生存 | 0 / `running (healthy)` |
| 一つでもstarting/failed/closed/no_connection/revision mismatch/PIDなし | 1 / `running (degraded)` + instance UUID/revision/state/stable code |
| DB query不能 | 1 / `running (degraded): status unavailable` |

`start` は10秒deadline（`dev.sh:374-403,519-619`）までhelperをpollし、healthy=0、degraded=1、core不可=2。instance一つの失敗でhealthy sibling/coreを止めない。監視とPID回収は既存`dev.sh` supervisorだけが行い、自動restartしない。`restart`だけが全componentをstop→start、`stop`は個別PIDを止める。core exit時はsupervisorが全gate PIDを個別停止する。

## 6. store と migration

新構造storeのschema作成は、converterの実行有無と独立して、runtime read setが参照する全canonical entityを空表として作成する。runtime readerが存在するentityを「converterが行を生成しない」ことを理由にDDLから省略しない。

### 6.1 source と row outcome

移行は immutable snapshot の全 source row を一度ずつ分類する。対象は `agent_discord_config`、`discord_channel_config`、`trusted_users`、`pending_interactions` と、worktree v1 の次の **6表**、`channels/external_refs/deliveries/subject_identities/dedup_hits/expanded_tools` である。`TARGET-MAPPING.json` の442 entryは減らさない。

runtime entityへ入るのは、そのlogical class contributionについてstorage、親/link、同じtarget logical keyへの全寄与がcanonicalな場合だけである。exact-oneの会計単位は**source rowではなくlogical class contribution**である。一source rowが複数classへ寄与するsource種別では、一classの失敗はそのclassだけを失敗/raw outcomeとし、独立classのcanonical row/linkを作れる。失敗classが一つ以上あるsource rowはcomplete raw copyを`legacy_unowned_source_rows`へ最大一件だけ書き、複数失敗classでも物理copyを増やさない。このraw copyと独立classのcanonical outputの併存はexact-one違反でない。`discord_channel_config`は一rowにつきdefaultまたはsubject-policyの一classだけである。tombstone subject、synthetic parent、winner、fallbackは作らない。

`discord-channel-policy-router-v1` は migration/runtime 共通の `resolve-or-create-config-place-v1` を呼ぶ。empty agentは `place_default_policies`、known non-empty agentは `place_subject_policies`、unknown non-empty agentはpolicy contribution失敗となる。0 placeは config-only placeを作るので成功、1 placeは再利用、複数だけが該当contributionのraw/operation errorである。11列のexact source valueはraw/default `source_row`または共通migration provenanceへopaqueに保全する。**`discord_channel_config`のsource rowはpolicy/default classだけに直接寄与し、binding、membership、route rowのsource contributionにはならない。** runtime policy commandはこの分類とは別のcommand効果として、既存subject/place/kindだけを `RecomputeDiscordRouteAdmission` → `ReconcileSubjectRoutes` で再整合する。11 mapping entryに個別`row_outcomes`を重複記載せず、この共通transformとdestination `write_condition`だけを正本にする。

`secret-value-v1` は移行時に既存`enc:v1:` bytesをopaqueとしてbyte-for-byte運び、decrypt/authenticate/parse/master-key要求をしない。legacy plaintext Nostr keyを新規envelope化する場合だけ暗号化を行う。migration reportはstored bytesのfamily/locator/length/SHA-256だけで、plaintext digest/HMACを作らない。

policy contributorの規則と件数は次で固定する。

1. known subject rowは `(place_id,'discord',subject_id)` のsubject-policy classへ一件だけ寄与する。
2. global rowは `(place_id,'discord')` のdefault classへ一件だけ寄与する。
3. required bool/text/storageまたはsubject/place解決がnoncanonicalなら、そのpolicy classだけを失敗としcomplete source rowを最大一件rawへ置く。
4. bindingの入力は、migrationでは会話履歴place、runtimeではDiscord membership dispatchだけである。channel config rowの存在・不存在・agent_idはbinding件数に影響しない。

### 6.2 migration-only logical binding class

migration binding writerは transform `logical-gate-binding-v1` 一つである。入力はworktree `channels`または会話履歴placeからのmigration seedであり、`discord_channel_config`ではない。source rowごとではなく `(instance_id,address)` ごとに全寄与を先にgroupし、binding IDもこのlogical keyから決める。shared/dedicatedのmigration seedが同じaddressを持てばinstanceごとの別bindingとして併存する。このtransformはimmutable snapshot/source row/raw carrier用の**migration専用**であり、runtime `ObserveGateAddress`は呼ばない。

binding classのmigration寄与同士で比較するのはbinding固有の `place_id`、`binding_metadata_bytes`（Discordはtyped `address_kind`とnullable guild ID）、`label` だけである。canonical時はkind-address `external_origin_scopes` の全必須fieldと、`gate_bindings` の全必須fieldを一意に出す。migration分類は§1.2の実在 `sessions.metadata_json` / `memory_sessions.session_id` evidence規則だけを使い、guild evidence、次にDM evidence、どちらも無いunknownの順でexact shape `{address_kind,guild_id}` を作る。履歴にlabelが無ければ`label=NULL`を正規値とし、偽値を作らない。同じbinding key内のmigration寄与で値が違えば勝者を選ばず全件を失敗会計し、scope/binding/routeを作らない。このmigration foldはruntime immutabilityを意味しない。runtimeでは§1.2の`ObserveGateAddress`だけが`address_kind=unknown`を最初のeligible live guild/DMへ一回enrichし、同じtransactionで`ReconcileSubjectRoutes`を行う。確定済みkindの不一致またはguild ID不一致だけをconflictにする。labelはruntime-mutableで観測ごとに更新し、conflict比較へ入れない。metadata bytes/digest/labelのruntime更新は同じcommand transaction以外から行わない。

default classは `(place_id,kind_id)`、subject policy classは `(place_id,kind_id,subject_id)` で分類する。policy 3値とheartbeat 3値は同じlogical keyへの複数寄与だけをequivalence/conflict判定し、defaultとsubject間、または異なるsubject間の値の差を衝突にしない。一classの衝突はそのclassだけをraw化し、別policy classや会話履歴由来bindingを無効にしない。

revision/bindingのkind固有形は `GATE-CONFIG-MAP.json` の schema-bound bytesだけである。revisionは `config_schema_id/config_bytes/config_digest`、bindingは `binding_metadata_schema_id/binding_metadata_bytes/binding_metadata_digest` を使う。Discord/Nostr/webが互いの不要fieldを捏造するtyped union、generic `parent_address` は無い。

### 6.3 unified raw carrier

非canonical gate/UI/unowned rowの永続先は次の一表だけである。

```text
legacy_unowned_source_rows(
  source_db    text NOT NULL,
  source_table text NOT NULL,
  source_key   schema(sqlite-key-v1) NOT NULL,
  row_values   schema(sqlite-row-v1) NOT NULL,
  reason       text NOT NULL CHECK(reason <> ''),
  PRIMARY KEY(source_db,source_table,source_key)
)
```

`row_values` は frozen source snapshot の `PRAGMA table_info` ordinal順に、SQLiteのNULL/INTEGER/REAL/TEXT/BLOBとexact bytes/IEEE bitsを保持する。mappingとのjoinとtransform選択はcolumn nameで行い、fresh DDLやledger ordinalを実行時権威にしない。fixtureも同じ規則である。`source_key` は一意な非NULL source PK tupleをそのまま使い、それが無い行はsource SQLite rowid locatorを使う。したがってsource id=NULLの複数行もraw carrier上で別行になり、runtime IDへcanonical化されない。raw carrierは同じsource keyにつき最大一行で、複数class failureのreasonを一つにまとめる。raw carrierにproposed link、runtime link、domain別reason enum、個別digestを置かない。adminは件数とsource locator/reasonを監査できるが、runtime readerは0である。

代表 reason は `unknown_owner`、`multiple_place_matches`、`noncanonical_storage`、`conflicting_binding_class`、`identity_conflict`、`unknown_enum`、`unresolved_parent`、`null_source_key`、`noncanonical_interaction` である。reasonは非空ならよく、設計がclosed enumを増殖させない。

### 6.4 worktree v1 outcome

| source | canonical outcome | raw outcome |
|---|---|---|
| `channels` | typed parentがexact-oneでlogical classがequivalentならscope+binding | class内の一件でも不正/衝突ならclass全件raw。scope/route/policyなし |
| `external_refs` | parent scope/binding/place/seqがexact-one、direction=`in|out`なら `external_refs`、inはoriginも作る | row全体raw |
| `deliveries` | なし。v1はconnection epoch/revisionを持たない | 全row raw。strict `deliveries`へ入れずretryしない |
| `subject_identities` | subjectとmigration-created instance集合が確定すれば、各instanceへ同じsubjectで展開 | 0 instance、親不明、subject衝突はrow全体raw |
| `dedup_hits` | typed parent/scopeがexact-oneなら `gate_dedup_hits`。完全重複もrowごとに保持 | row全体raw |
| `expanded_tools` | place/subject/kindがexact-oneなら `expanded_gate_tools` | row全体raw |

`external_origin_scopes` のkind/addressまたはinstance/address一意性は維持する。同じscope候補が複数placeへ解けるときは関係する寄与集合を全部rawへ送り、どのplaceも選ばない。strict runtime entityのenum/NOT NULL/UNIQUEをlegacy rowのために弱めない。

### 6.5 pending interactions

`pending-interaction-router-v2` がcomplete source rowを分類する。source `id` がNULLの旧UI rowは、他fieldが合法でも無条件に全rowをrawへ `reason=null_source_key` で送り、raw carrierの`source_key`だけにsource SQLite rowid locatorを使う。複数NULL rowをrank、rowid、内容digestでruntime interactionへcanonical化しない。

非NULL idの行だけ、owner/place exact-one、storage、payload、targetが要求するtime、status、response 3列を検査する。`owner_only`はINTEGER 0=false/nonzero=trueである。

canonical IDはtarget空DBでsource keyのtagged bytes昇順、同値はsource SQLite rowid昇順に並べ、1からdense positive integerを割り当てる。occupied IDまたは同key異digestはcomplete-row rawである。`deadline = created_at + timeout_secs * 1_000_000_000` をsigned-i64 checked arithmeticで求め、overflow/parse失敗はcomplete-row rawとする。`payload` はexact `schema(interaction)` object `{surface_id,components,owner_only}` で、`components` はvalidated `a2ui_components_json` valueである。responded rowの `interaction_responses.interaction_id` は、この割当済み `interactions.id` と同じ値にして結合する。

| status / response | canonical outcome |
|---|---|
| `pending` / 3列全NULL | `interactions(state=pending,binding_id=NULL)` |
| `responded` / 3列全non-NULLかつresponse/responder/time valid | `interactions(state=responded,binding_id=NULL)` + 記録済み`interaction_responses` |
| `timeout|expired` / 3列全NULL | `interactions(state=expired,binding_id=NULL)` |
| `timeout|expired` / 3列全non-NULLかつresponse/responder/time valid | `interactions(state=expired,binding_id=NULL)` + 記録済み`interaction_responses` |

responseを作る場合、`responder_id` exact bytesを`responder_external_id`へ置く。exact UTF-8 `system` は`responder_kind=system,responder_subject_id=NULL`である。それ以外は`surface`をnormalized kindへ変換し、そのkindのmigration-created全instanceをUUID byte順に列挙して各`(instance_id,responder_id)` identityを引く。1件以上が同じ一subjectへcollapseすれば`responder_kind=subject`、0件は正常な`responder_kind=unknown,responder_subject_id=NULL`、異なるsubjectが複数ならconflict rawである。legacy rowはbinding NULLなのでbindingからinstanceを推測しない。

上表以外、owner/place 0/複数、unknown status、response 3列のNULL/non-NULL混在、必要timestampのparse failure、responder subject conflict、target collisionはrow全体rawでcanonical rowを作らない。通常timeout taskが作る`status=timeout`かつresponse 3列全non-NULLは正常canonical形であり、expired stateとresponse記録を両立させる。legacy canonical rowの`binding_id=NULL`はlive action対象外である。strict live state/response contractは弱めない。

### 6.6 full regeneration と transaction

converterは毎回empty targetへsnapshot全量を最初から生成する。incremental resume、既存targetへのupsert、途中checkpoint再開は実装しない。同一source snapshotを同一converter binaryで再実行した時だけ同じpersistent IDを要求する。UUID/name/SQLite valueのbyte級encodingは本書に固定せず、実装のgolden testが同一binary内の安定性を担う。

preflightはsource keyと件数に加え、各source rowが生成するlogical class contribution manifestで、会話履歴/worktree logical binding衝突、orphan parent、unknown enum、identity conflict、channel→place複数、UI outcome、atypical storage、ID collisionをreportする。これらはraw outcomeが作れる限りmigration停止条件ではない。target write error、source read error、logical class contributionごとのcanonical/failed exact-one違反、失敗source rowのraw copyが0件または2件以上、raw values不一致、class contribution件数照合失敗だけがrollback条件である。canonical classと同じsource rowのraw copyの併存は違反に数えない。

順序は、empty target確認 → schema作成 → shared/dedicated/compat instance生成 → history materializationとconfig-place解決 → 全sourceのclassify（`discord_channel_config` policy/default、schedule、effective tool visibility、membership、会話履歴/worktree由来binding/scope）→ canonical集合/raw source copy確定 → logical class contribution exact-one/件数とraw最大一件照合 → **全migration `(subject,place,kind)` への一回の`ReconcileSubjectRoutes`後段pass** → route集合と選択`binding_id`照合 → commit とする。binding分類transformはrouteを同時生成しない。config-only/unseen channelのbindingは生成せずruntime lazy materializationへ残す。失敗時はtarget transactionをrollbackし、sourceを変更しない。同一snapshotのsource列挙順は分類集合にも後段pass結果にも影響しない。

### 6.7 schema catalog とchecker契約

`TARGET-SCHEMA.json.entities` は実装するpersistent entityだけである。top-level `catalogs` は `persistent/resource/coverage/staging` を区別し、checkerは同じIDの複数kind所属、非persistent destinationのkind省略、coverage/stagingのruntime read/writeを拒否する。

- `canonical_database.all_mapped_columns` はcoverage markerで、table/store APIではない。
- `resource:engine_registry`、`resource:operator_workspace`、`resource:process_log_filter`、`resource:workspace_tree` はresource catalogで、persistent rowではない。
- `subject_workspaces` はfilesystem output resourceである。source root、`{target-data-root}/agents/{resolved-subject-public-id}/workspace`、manifest linkを物理契約として持ち、exact bytes/type/mode/mtime/symlink textを一度だけcopyする。nested coverage entryはmanifest存在だけを検査し、二重copyしない。
- `legacy_history_materialization` staging entityは廃止する。`history-per-agent-router-v2` はcomplete source rowを入力に、`legacy_history_archive` と選択された最終runtime materializationへ直接書く。
- generated `{npub}.nsec` のsecret bytesは `secret_values.value` だけへ書く。`nostr_generated_keys.source_record` はsource locatorとSHA-256だけを持ち、plaintext/envelopeを含めない。

schema/mapping双方に同じtransform IDがある場合、各transformの`contract`に加え、top-level `algorithm` / `input_types` / `output_types` / `failure_conditions` は全てnon-empty string arrayで、prose whitespace normalize後に同一でなければならない。checkerはこれらの意味契約、source declared type/nullability → transform input/outcome → destination type/nullabilityの包含、row-emitting transformの全non-NULL field coverage、`GATE-CONFIG-MAP` assembly cardinalityとの一致を検査する。`resolve-or-create-config-place-v1`は`emits_rows=true`で`places`/`place_source_refs`の全fieldを`row_outputs`へ宣言し、`emission_cases`を `source_ref_match_count == 0` なら各entity一行insert、`== 1`なら両entity write 0と構造化して持つ。さらに11 channel entryの旧`row_outcomes`禁止、config→binding destination禁止、未読updated_atのparse必須化禁止、admission/membership/purpose/wire lifecycle/runtime command/identity/stop裁定を検査する。contractに無い暗黙のpartial row、interpreted NULL、cardinality補完は禁止する。

## 7. live UI route/lifecycle

live `interactions.binding_id` はUIを送ったcredential binding、`source_address`は実際のchannel。UI stateは`pending -> responded|expired`だけ、外部I/Oはdelivery stateだけ。

`send_ui` は既存順序 `e6f0595:crates/actions/src/a2ui.rs:117-342` を維持する。

1. session/args/componentsを検査。
2. interaction pendingとUI create delivery preparedを同transactionで作る。
3. `renderer.rs:31-528` / `form_modal.rs:1-112` 相当を移植しCreate Message。
4. message IDを`source_message_id`へ書く。書戻し失敗をpublic successからfailureへ変えない（`a2ui.rs:249-257`）。公開成功は`interaction_id,surface_id,status,message`だけ（同`:333-342`）。
5. render/API failure/unknownはdeliveryだけ`failed|indeterminate`、interactionはdeadlineまでpending。

responseは `(id,pending,binding,address,instance)`を照合してCAS、response insertを同transactionで一度だけ行い、その後disable delivery。timeoutはCASでexpired、既存timeout eventを会話へ戻してdisable+timeout文言（`a2ui.rs:294-329`、`renderer.rs:516-527`）。disable failureでterminal stateを戻さない。

late/競合/mismatch/NULL bindingは`ui_route_unknown|ui_route_mismatch|interaction_not_pending`。B3-06の「message ID書戻し失敗後にdisableをどう表示するか」は構造主経路を変えない後続issue分類のままとし、このissueでは`ui_message_unknown` delivery failureとして記録しterminal UI stateを戻さない。

## 8. 非 voice tool 11件

公開name/description/input schema/class/result/errorは `TOOL-DISPOSITION.json` とL1/L2 baselineを正本とし転記しない。

### 8.1 gate operation 6件

`discord_list_guilds`, `discord_list_channels`, `discord_create_channel`, `discord_create_webhook`, `discord_add_reaction`, `discord_send_file`。prepared argsと外形は `e6f0595:crates/discord/src/gateway_actions/mod.rs:203-373`、`discord_ops.rs:17-156,285-797`。file検査は同`:680-743`。各operationは `purpose=tool:<public-name>` のsnapshot一つを使う。

`discord_list_channels` のpolicy joinはchannel→placeをsource refで解き、§1.3を使う。0件はhard default、複数はpolicyを推測せず当該channelを`policy_ambiguous`としてtool errorにする。

### 8.2 core capability 5件

| capability | 固定経路 |
|---|---|
| `discord_channel_config` | §1.4 transaction。binding不要、route/bindingは作らない |
| `ensure_subtask_webhook` | `webhook_endpoints(owner_subject_id,kind,scope,tool_name)` lookup/upsert。必要時だけ内部create |
| `ensure_webhook` | 同上、既存family/default差はbaseline |
| `request_peer_review` | coreが宛先/分割を決め同じoutbound snapshotへ順送。途中失敗は送信済み件数をpublic errorへ |
| `send_ui` | §7だけを通る |

既存境界は `subtask_webhook.rs:103-260`、`peer_review.rs:279-422`、`a2ui.rs:117-342`。webhook作成後のstore failureは「外部作成済み・保存失敗」のfailedで、同call再作成なし。

### 8.3 任意 channel address

public toolの`channel_id`は通常binding addressと一致しなくてよい。`tool:<name>` routeのbindingはcredential instanceだけを選び、`GateRoute.address`は検証済み引数。新しいingress bindingを自動作成しない。`send_ui`はcredential bindingとsource addressを別々に保存する。list guild/create channelのようにchannel addressを持たないoperationも同じcredential snapshotを使う。

### 8.4 実装seam

seamの契約は情報量である。`GateRoute`等のfield集合と各境界が読み書きする情報量を保てばよく、Rustのexact signature（所有/参照、`Option`、引数の畳み方、trait名）は実装の自由とする。

| 対象 | 変更 |
|---|---|
| `crates/port` | kind/instance/GateRoute、typed spec、ready/failed、UI payload |
| `crates/plugd` | protocol adapter/parser、spec refcount、instance epoch/request |
| `crates/social-runtime` | `subject_routes` snapshot、dedup、policy、typing |
| `crates/store` | §1/§6/§7 entity/API/migration、single lifecycle writer |
| `crates/app` | runner selector+exec、read-only status helper |
| `dev.sh` | DB列挙runner PID監督、helperによるcommand UX。projection/probeなし |
| workspace | `opencrab-discord-gate`、renderer/form modal移植 |

## 9. acceptance

full fake Discordは作らない。translation/operation adapter fake、合成credential/process + fake core E2E、migration fixtureで検査する。

1. protocol-1 web/nostr fixtureがwire無変更でprebound compatibility instanceへ接続し、bind/unbind/rebind/not_boundを従来どおり使う。membership Discord fixtureはこれらを一度も送受せず、binding 0件のeventもnot_boundにせず`ObserveGateAddress`へ渡す。Message CreateとButton/Select/Modalが全て`ResolveDiscordDiscovery`を通り、通常guild、`Private`/`GroupDm`、`NewsThread`/`PublicThread`/`PrivateThread`、interaction channel欠落からdispatch→cache→HTTP順に同じcarrierを作る。lookup failureと`Unknown(_)`はwire送信/Event 0でlogし、interactionのmechanical ACKは行いconnectionを維持する。labelのpresent値だけ更新し、dispatch値なしとlookup失敗の両方で既存labelを保持する。guild ID必須条件、optional label、unknown/null/type違反をexact validationし、wireに`observed_at`が無くcommandがcore受信時刻を使うことを確認する。membership kindから`read`を一件送るprotocol validation testはstoreを呼ばず`membership_read_unsupported`を返してconnectionを維持し、Discord adapterが`read`を送らない。route reader/writerが`subject_routes`だけである。
2. 合成fixtureからdedicated 3 + shared 1 instance/revision/secretができる。各revision 1は`present=true`。各dedicatedはinstance/revision/secret-set/secret-valueの全必須fieldと`agent_ids=[]`、sharedはdeterministic locator、owner NULL、secret set/value linkage、effective configのdistinct `agent_ids`をconfig bytesに持ち、同じbinaryで全量再実行すると同じIDになる。shared agent IDの未解決/曖昧はassembly error、重複は一subjectへcollapseする。dedicatedの任意文字列/非TEXT`updated_at`はraw化せずexact SQLite valueでconfig bytesへ入り、created_atはsnapshot captured-atである。ownerがNULL/0件/複数件のdedicated source rowはcomplete-row raw、ownerless instance 0件で、次のsource rowは処理される。
3. 同一addressのglobal/subject A/B/C config rowが互いに異なるpolicy/heartbeatを持っても、default一行とsubject policy三行ができ、`gate_bindings`/`subject_routes`/membershipは0件である。config rowだけからshared/dedicated binding contributorを作らない。同じpolicy key内の値が異なるfixtureではそのpolicy contributionだけがfailedとなりraw source copyは各row最大一件、別policy classは残る。11 entryに`row_outcomes`は無く、共通transformとdestination `write_condition`だけでcontribution exact-one件数とraw copy件数を照合する。
4. 同じspecの3instanceは接続し、順序/duplicateを含めEqで異なる4本目だけspec mismatch。digest処理なし。
5. 1instance切断後もtoolsが見え、最後の1本でspec/indexが消える。
6. whitelist=trueの同じ合成guild messageをshared/dedicatedのどれから先に受けてもEventは1件で、今回のadmissionでeligibleかつ同じplaceにinbound routeを持つsubject A/B/Cが各1回だけ発火する。観測bindingとsubjectの選択済みbindingは一致不要。trusted principalとworktree identityはmigration-created全Discord instanceへ一行ずつ展開され、全行同じsubject。unknown non-empty agentのconfig rowは`legacy_unowned_source_rows`へ全量入りruntimeには出ない。
7. subject A/B/Cのsay/tool/typingは `DeriveDiscordRoutePurposes` が作った選択済みsnapshot一つだけ。non-ready時fallbackなし。resolved heartbeat=falseかつenabled schedule 0ならtimed 0、trueへのpolicy更新でtimed 1、visibleな宣言済みoperationだけ`tool:<name>` 1、hidden/undeclaredは0である。policy/tool visibility/gate config/schedule/trusted-user DELETE/owner principal変更の6 writerは毎回`RecomputeDiscordRouteAdmission`→`ReconcileSubjectRoutes`の順に同transactionで呼ぶ。admitted guildをwhitelist=falseへ更新するとroute 0、membershipは保存され、その後schedule / tool visibility / gate configを変更してもroute 0のままである。global defaultだけを持つmigration済みmembershipとglobal defaultだけのlazy participantは、既存timed routeなしから直接defaultを解いてtimed 1になる。別fixtureはheartbeat/defaultをfalseにして、materialize済みbinding上のschedule create→disable→enable→deleteを実行し、timedが1→0→1→0へexactに収束する。shared bindingとsubject dedicated bindingが同じplaceにあるfixtureでは、schedule createとhidden→visible tool変更で新規purposeを増やし、全purposeの`binding_id`がdedicatedになることを検査する。dedicated participant/instanceをconfigから外すとsharedへ全purposeを再選択し、sharedも外すとroute 0になる。件数だけでなく各行のbinding_idを検査し、binding列挙順を反転しても同一である。migration fixtureは実在field形の合成 `sessions.metadata_json` / `memory_sessions.session_id` evidenceを使うclassified **migrated guild**、**migrated dm**、evidence無しunknown、global default、subject override、enabled/disabled cron/every、visible/hidden operation、shared+dedicated bindingを同一snapshotへ入れ、source列挙順を正順/逆順に変えた二回でroute主キー集合と選択binding_idが完全一致する。移行済みenabled cron/everyは分類済みguild fixtureで各々timed 1、disabled化または削除後は0である。migrated guildはwhitelist、migrated dmはcurrent trustでeligible、unknownは全purpose route 0を検査する。binding未materializeのschedule操作はroute 0件、place/binding/membership 0件のままである。
8. **同じ合成subject/instanceがplace A/address Aとplace B/address Bにbinding/routeを持ち、各placeの通常say/typingがそれぞれのaddressだけへ出る。**
9. 任意channel ID toolは同じcredentialから指定addressへ送り、ingress bindingを増やさない。
10. equivalent bindingのどれが先に同一messageを観測しても`ResolvePlacePolicy`結果が同じ。policy複製rowは存在しない。
11. admission/lazy fixtureを次の順で検査する。(a) 未設定guild、untrusted DM、**live thread**の各dispatchはEventを含む**全persistent entityのrow差分が0**で、thread metadata/bindingは作られない。(b) 未binding合成guildへwhitelist=trueの`discord_channel_config`を置くとconfig-only place/policyだけができ、binding/route/membershipは0。(c) shared instanceの次dispatchはconfig bytesの`agent_ids`だけをobserved participant候補にsubject別admissionし、eligible subjectについて同じplaceへUUIDv7 scope/binding、`role=participant,joined_at=observed_at,shared_seen_seq=transaction開始時latest seq`のmembership、導出済みrouteを各一度だけ作る。これでturn/context、say/typing、heartbeat、visibleな6 operationのcredential snapshotまで通る。shared list外subjectとwhitelist=false subjectはmembership/route/Eventを得ない。(d) 別fixtureは未bindingで初回拒否→policy更新→次dispatchの順に進み、membershipが一度だけ成立して再dispatchでもjoined_at/cursor/row数が不変。さらにmigration seed済みの既知bindingからmembershipだけを欠いたfixtureも、dispatch時に同じ`EnsureAdmittedDiscordMembership`を一回通って同じ結果になる。(e) migration seed済み`address_kind=unknown,guild_id=NULL,label=NULL` bindingへ最初のlive guild dispatchを通す **unknown→enrichment** fixtureでは、同じ`ObserveGateAddress` transactionで`address_kind=guild`とguild IDへ一回だけenrichし、metadata bytes/digest更新と`ReconcileSubjectRoutes`を不可分に行ってEventを処理する。別のunknown seedへのtrusted live DMも同様に`address_kind=dm,guild_id=NULL`へ一回だけ遷移してrouteを作る。同じkind/guild IDの再観測はmetadata write 0、確定kindの不一致または別non-NULL guild IDだけがconflictでEvent 0になる。その後のchannel renameはlabelだけを最新観測値へ更新してEventを処理し、metadata digestは不変である。続くlabel省略dispatchとlabel lookup失敗dispatchはいずれも既存labelを保持する。(f) dedicated PUT直後は未観測placeへbinding/routeを作らず、dedicatedがそのplaceを初観測したtransactionだけで、`ReconcileSubjectRoutes`が全purposeをdedicated bindingへexactに揃える。
12. global/known source rowは11列全てopaqueに一致する。channel→place 1件は既存IDを返して`places`/`place_source_refs` write 0、0件は各entity一行だけinsert、複数だけは該当contribution rawであり、structured `emission_cases` と実数が一致する。`updated_at`がparse可能ならその時刻、NULL/非TEXT/parse不能/overflowならsnapshot metadata captured-at定数がconfig-only place/source ref/subject policyの非NULL時刻になり、place `created_at` とsource ref `updated_at`は同値、parse失敗だけではrawにならない。bool fixtureの0はfalse、2と-1は統一裁定によりtrue、heartbeat intervalの負i64は生値のままcanonicalで、policy3値とheartbeat enabled/interval/instructionの解決が§1.3どおり。
13. worktree v1 fixtureのduplicate `(gate,address)` across places、orphan ref/delivery/identity、unknown direction/state、完全重複dedup hits、expanded tools orphanを停止せずcanonicalまたは単一raw carrierへexact-oneで移す。origin scope衝突集合は全raw、runtime row 0件。
14. TEXT列のBLOB/INTEGER、INTEGER列のTEXT/REALを含むSQLite fixtureがbyte/IEEE bit一致でrawへ入り、runtime enum/NOT NULLを弱めない。
15. pending UI fixtureでpending+NULL response、responded完全response、timeout+全NULL、runtime通常形のtimeout+response 3列全non-NULL、NULL混在response、orphan owner/session、unknown statusを§6.5各outcomeへ分ける。通常timeoutはexpired interactionとresponseの両方を持ち、`system`はkind=system/subject=NULL。known responderは同kind全instance identityが同一subjectへcollapseし、0 matchはkind=unknown/subject=NULL、異なるsubjectはraw。source id=NULLの合法行を複数含め、全件raw・canonical 0件・全source列/件数一致を検査する。canonical rowはtagged source key/rowid順のdense positive ID、checked deadline、exact `{surface_id,components,owner_only}` payloadを持ち、response FKは同じIDへ結合する。deadline overflowはcomplete-row raw。canonical legacy rowはbinding NULLでaction拒否。
16. Button/Select/Modalはbinding instance+source address照合。response/timeout一度だけterminal、disable failureはdeliveryだけ。公開send_uiにmessage IDなし。
17. migrationの既存`enc:v1` fixtureはmaster key無し/bad keyでもdecrypt検証せずbytes一致でcanonicalとなり、reportはstored-byte SHA-256だけでplaintext HMAC無し。legacy plaintextのenvelope化とruntime runnerの`source-plaintext`/`enc:v1`、bad master key、missing secretは別々に検査し、runnerはDB write/spawn監視せずexecする。generated `.nsec` fixtureはsecret bytesが `secret_values.value` の一箇所だけにあり、`nostr_generated_keys.source_record` はlocator/digestだけで、secretはargv/config/provenance/report/log/statusへ出ない。
18. core以外からlifecycle/connection writeを試すtestは失敗する。configured instanceに対し `GET(configured=true,enabled=true)` → stop → `GET(true,false)` → process restart → `GET(true,false,running=false)` → start → `GET(true,true,running=true)` を確認する。全段でbinding/route/identity/secretは不変である。`gate-connections.json`は存在せず、read-only DB helperだけでstart/status/restartの0/1/2とdegraded/unknown/no_connectionを表示する。
19. say長文は最後のorigin一件、reactはoriginなし、sending後timeoutはindeterminate、自動再送0。
20. queue/deadlineは§4.3だけ。source/test/config/reportは実在bot/channel/server/subject identifierを含まない。
21. legacy Discord退役時、実装PRで作成した番号付きvoice issueを参照し、L1/L2で13 tool（非voice11+voice2）の存在と機能を確認する。11件だけでは退役不可。
22. trusted-user POST後、合成shared/dedicated全instanceから同じprincipal/subjectへ解決し、GETは公開一行だけを返す。続いて同kindのconfig PUTで新instanceを作ると、そのtransactionがgrant/provenance由来principalを列挙・登録し、新instanceから同principalを最初に観測しても同じsubjectへ解決する。別fixtureではworktree `subject_identities`だけからmigrationされたidentityを既存同kind instanceから`(external_id,subject_id)` collapseして新instanceへ登録し、同じ初観測結果を得る。両由来のexact duplicateは一回、subject衝突は全transaction rollback。PATCHは全instance copyとgrantを更新する。DM authorizationは ingress とroute再計算の両方が `IsCurrentDiscordDmPrincipal` を使う。**trusted DM成立 → trusted-user DELETE → route 0 → schedule/tool/config変更後もroute 0 → 再 trust 後の次 dispatchで復帰**を一fixtureで検査し、DELETE後もmembershipは履歴として同値保存する。owner principal変更も同transactionで旧principalだけのDM routeを0にし、新principalの次dispatchで復帰する。instance数の複数をerrorにせず、新registryは無い。
23. Discord/Nostrそれぞれで `GET configured` → stop → GET → process restart → start → GET → DELETE → GET(unconfigured) → stop(idempotent success, write 0) → PATCH(no-config error) → 再DELETE(`deleted=false`) → PUT(`present=true`) → start の状態列を既存response projectionで確認する。別fixtureでinstance不存在のstopも同じsuccess/write 0である。初回DELETEは`deleted=true`。active revisionの`present/enabled`とconnection由来`running`が各段で期待値になり、stop no-opを含む全過程でbinding/identity/secret historyは不変である。Discord DELETEだけは同transactionの`ReconcileSubjectRoutes`でdedicatedからsharedへ再選択またはroute 0、再PUTでmaterialize済みdedicatedへexact setを戻す。Nostr routeは不変である。
24. 合成trusted DMの同じ初回dispatchを使い、`Observe → writer → AppendEvent` と `Observe → AppendEvent → writer` の二順序を独立した空DB fixtureで実行する。`ObserveGateAddress` commit直後には `place_source_refs.metadata.dm_user_id=author_external_id`、membership、全purpose routeが同時に存在し、既存source refの他fieldは不変である。writerはschedule/policy/tool visibilityの各代表一件を用いて `observed_author_external_id=NULL` で再整合する。両順序の最終canonical stateでroute主キー集合と各`binding_id`が完全一致する。さらにtransaction commit前、Observe commit後かつAppendEvent前、AppendEvent後かつwriter前の各 **crash boundary** をfault injectionし、commit前は全Observe writeがrollback、commit後は再起動したwriterが永続principal evidenceだけで同じrouteを再導出し、dispatch replay/dedup完了後の **最終route一致** を確認する。既存routeからprincipal候補を復元する実装はfixtureで拒否する。
25. **config-only trusted DM** fixtureは、合成channel configから`classification=config_only`かつ全session field NULLのsource refを先に作り、そのaddressの最初のadmitted DM dispatchを24と同じ二順序および各 **crash boundary** で実行する。Observe commitでは同じsource-ref行が`classification=config_only → live`へ一方向・一回だけ遷移し、同transactionで`metadata.dm_user_id`、membership、binding、全purpose routeが成立する。config-onlyの全session field NULL制約はlive遷移後には適用せず、`updated_at`、`source_id`、place ID等のclassification以外の既存fieldは不変とする。commit前crashはclassificationを含む全writeをrollbackしconfig_onlyへ戻し、commit後crashはlive分類と永続principal evidenceだけから再起動writerが同じroute主キー集合と`binding_id`へ収束する。再dispatchではclassification write 0、source-ref追加0である。
26. 既存metadataがnon-NULL objectでない合成source refへadmitted DMを通し、独立outcome `source_ref_metadata_shape_conflict`、transaction全体の **rollback / Event 0**、classification/membership/binding/route差分0を検査する。generic `store_error`への畳み込み、wrapper、別表、raw fallbackは失敗とする。

実装中に本書のentity/field/state/route/outcomeで表現できない現物が出たら補完せず、**未決**として持ち帰る。

## 10. 後続 issue / 未決

- Discord voice slice。**実装PR作成時にissueを切り、番号を本書へ戻す。**
- Message Update/Delete/Reaction ingress、Quote/Amend/Retract、multi-origin/part/reconcile。
- bot identity pin/TOFU/rotation。
- SDK不足が実測された場合の独自rate-limit/retry dispatcher。
- core切断とDiscord Resume協調、history backfill、full fake Discord。
- file pathをsecurity boundaryにする必要が生じた場合のgate共通transfer service。
- B3-06: message ID書戻し失敗後のdisable表示詳細（本issueはdelivery=`ui_message_unknown`まで）。

## 設計 v14 からの変更点（v15）

- configured web / nostrごとのprotocol-1 compatibility instanceを、app起動時に`gate-instance-id-v1` locator `compat:<name>`でdeterministic seedする。revision 1は`compat/v1`・空config bytes・enabledで、v1 helloは同じ導出を使うexact-one store queryにより解決し、0件を`instance_unknown`として自動作成しない。
- worktree v1 migration対象が6表であることを§6.1で明記した。
- runtime read setのcanonical entityはconverterと独立に新構造store schemaが空表作成することを固定した。
- seamはfield集合/read-write情報量が契約であり、Rustのexact signatureは実装自由であることを固定した。

## 設計 v13 からの変更点（v14）

- admitted初観測が既存config-only source refを再利用する場合、同じ`ObserveGateAddress` transactionで`classification=config_only → live`へ一方向・一回だけ遷移する。classification以外の既存fieldは保存し、transaction失敗時はclassificationもrollbackする。
- `classification=config_only`の全session field NULL制約は、その分類である間だけ適用し、live遷移後には適用しない。
- `source_ref_metadata_shape_conflict`をgeneric `store_error`と別のstable outcomeとして列挙し、transaction全体を`rollback / Event 0`に固定した。
- v13の永続principal evidence、二順序/crash復帰、trusted-user DELETE read closureは変更していない。**設計 v12**で確定したtyped address kind（旧称 **unknown→observed** の一回enrichmentを含む）、current trust、transactional invalidationも変更していない。
