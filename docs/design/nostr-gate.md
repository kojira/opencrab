# Nostr gate 設計 v1（issue #749）

状態: 実装前の確定設計。対象 revision は `435cb5a`。本書はコードではなく、構造・不変条件・判断だけを固定する。

## 1. 結論と境界

Nostr は `kind_id=nostr`, protocol 2, `ingress_discovery=prebound`, `origin_scope=kind_address` の gate kind とする。1 agent credential/process を 1 `gate_instances`、その timeline を 1 `places`、設定済み watch をその場へ入る 1 `gate_bindings` とする。本番の 3 agent / 3 relay は、この同じ形の instance を 3 組作り、各 instance が 3 relay を束ねる構成であり、個数を schema に焼かない。

責務境界は `gateways-deliver-core-decides` で固定する。

- gate: relay 接続、Nostr frame・署名の検証、設定済み watch の機械的 match、raw Nostr event の配送、署名・発行だけを行う。
- core: place/binding 解決、dedup、owner/followee/standing 判定、即応判定、窓の作成・回収、turn 発火、出力権限、NO_REPLY、settled 文脈を決める。
- engine: core が選んだ同一権限 batch と明示的な継続文脈だけを受け、応答候補を返す。

現物にも「gate は note を運び、turn を起こすかは core」とある (`crates/nostr-gate/src/main.rs:6`, `crates/nostr-gate/src/main.rs:330`, `crates/nostr-gate/src/main.rs:620`)。現在の protocol 1 / single-relay / said 化 (`crates/nostr-gate/src/main.rs:121`, `crates/nostr-gate/src/main.rs:192`, `crates/nostr-gate/src/nostr.rs:410`) は移行元の実装であり、本設計の正本ではない。

## 2. 正本の行

### 2.1 kind・instance・connection

`gate_kinds` の Nostr 行は次で固定する。

| field | value |
|---|---|
| `kind_id` | `nostr` |
| `protocol_major` | `2` |
| `origin_scope` | `kind_address` |
| `ingress_discovery` | `prebound` |

`gate_instance_revisions.config_schema_id` は新規実装で `gate-config/nostr/v2` のみを active にできる。既存 `gate-config/nostr/v1` は lossless migration carrier であり、protocol-1 compatibility instance 用である。v1 から interval を推測して v2 を有効化してはならない。

`gate_connections` は relay ごとの socket 表ではなく gate process/revision の epoch である。active 条件は「protocol 2 hello が検証済みで、設定 relay の少なくとも 1 本で watch subscription が成立」である。1 本以上なら部分障害、0 本なら epoch を failed にする。relay 別状態は診断値であり正本にしない。

### 2.2 timeline place・binding・route

各 Nostr instance に次を exact-one で作る。

- `places`: Nostr timeline の場。config の relay/filter を place identity に含めない。
- `gate_bindings`: address は `timeline:<instance-id>`、metadata は `gate-binding/nostr/v1 = {mode:"timeline"}`。watch を変更しても binding/place は変えない。
- `external_origin_scopes`: mode は `kind_address`、kind は `nostr`、address は上の timeline address。
- `subject_routes`: agent subject / timeline place / `nostr` の `inbound`, `outbound`, `timed`, および宣言された `tool:<name>` を同じ binding に向ける。存在しない route を別 instance へ fallback しない。
- `place_default_policies`: Nostr 行は `place-policy/nostr/v1` を持ち、core の層別 debounce を決める。

bind は住所から filter を再解釈しない。core は active revision を解決して gate へ timeline bind を送り、gate はその revision の watch を全 relay に張る。現在の address parser が author/kind だけを解釈する箇所 (`crates/nostr-gate/src/nostr.rs:353`) は置換対象である。

## 3. watch 契約（#744）

`gate-config/nostr/v2` の watch は次の exact shape である。

```text
watch = {
  match: "any",
  mention_self: true,
  authors: [canonical-pubkey...],
  keywords: [nonempty-string...],
  kinds: [u32...]
}
```

不変条件:

1. `kinds` は global AND、`mention_self` / authors / keywords は OR (`match=any`) である。active v2 は `mention_self=true` を必須とし、`match` の他値は reject する。
2. mention は event の `p` tag が active revision の `self_external_id` と一致すること。kind に意味のない節は false である。
3. author は canonical lowercase 64-hex pubkey の byte equality。
4. keyword は event `content` に対する case-sensitive、Unicode normalization なしの substring。空文字を reject する。relay protocol に keyword predicate を期待せず、gate が受信後に機械的に絞る。
5. 設定した `kinds` 以外は配送しない。暗号化 DM kind は本 watch の対象外として reject する。
6. self-authored event も gate では落とさず配送する。core が既存 outbound origin との dedup/self-loop 判定を行う。
7. EOSE は配送完了通知であり、発火・窓 flush ではない。

これは旧 watch の authors / keywords / kinds / mention-self / match-any という外形を復元する。ただし旧資料は根拠だけであり (`6bf4976^:docs/nostaro-interface.md:154`, `6bf4976^:docs/nostaro-interface.md:184`)、宣言の正本は `gate-config/nostr/v2` である。

## 4. ingress の型

gate→core の protocol 2 event は通常の envelope に `native_schema_id=nostr-event/v1` と NIP-01 event の `id,pubkey,created_at,kind,tags,content,sig` を lossless に載せる。gate は署名/id を検証するが semantic event kind を選ばない。core は次へ写す。

| native kind | canonical `events.kind` | target / content |
|---|---|---|
| 0 | `profiled` | NIP-01 profile JSON を schema-bound bytes として保持。parse 失敗でも raw content を失わない |
| 1 | `said` | text。NIP-10 explicit `reply` marker、なければ legacy unmarked e-tag の末尾を reply target とする |
| 6 | `boosted` | NIP-18 target e-tag と埋込み event bytes を保持 |
| 7 | `reacted` | target e-tag と reaction content を保持 |
| その他の watch kind | `native` | numeric kind、tags、content を `nostr-event/v1` のまま保持 |

全 p-tag は mentions に解決し、解決不能な external id も native payload から消さない。kind 6/7 を reply/said に寄せず、kind 0 を空 said にしない。現状が全 event を said、最初の e-tag を reply にする事実 (`crates/nostr-gate/src/nostr.rs:410`, `crates/nostr-gate/src/nostr.rs:426`) が #746 の置換点である。canonical enum の既存 `boosted` / `reacted` は `crates/port/src/lib.rs:71` を参照する。

dedup 成功後の 1 event append と後述 `subject_event_inputs` 作成は同一 transaction とする。不正署名、id 不一致、watch 不一致、unknown binding は event 0 / input 0 で fail-closed とする。

## 5. owner・followee・standing の正本

### 5.1 owner

owner external id の正本は active `gate-config/nostr/v2.owner_external_id`。core は同じ instance の `gate_subject_identities` に exact-one で解決し、その subject の standing が Owner であることも確認する。空、未解決、複数、standing 不一致は owner 不在として fail-closed であり、別 instance や表示名から補完しない。

### 5.2 followees

followees の正本は `gate_principal_sets` の `(instance_id,set_name=followees)` 現行行である。source は self pubkey が author の verified kind-3 contact-list event。timeline watch とは別に、gate は各 relay へ `{authors:[self],kinds:[3],limit:1}` の内部 control subscription を張り、raw verified event を core へ渡す。core は source ordering `(created_at,event_id)` が現在行より新しい場合だけ、p-tag を canonicalize / sort / distinct して全置換する。kind 3 自体は timeline event/input を作らない。

relay 障害や refresh 失敗時は最後の成功行を保持する。まだ成功行が無い場合の集合は空であり、全員許可には倒さない。旧実装にも owner を config、followees を kind 3 から取って refresh failure で保持する根拠がある (`6bf4976^:crates/social-runtime/src/manager.rs:54`, `6bf4976^:crates/social-runtime/src/manager.rs:743`, `6bf4976^:crates/social-runtime/src/manager.rs:778`)。

### 5.3 classification snapshot

各受信 event は append 時点で 1 回だけ次を snapshot する。

- `authority_layer`: existing `Standing` の `owner | trusted | unknown`。owner は上記 owner、followee は少なくとも trusted、明示 grant がより強ければ既存 standing 規則に従う。
- `is_owner_or_followee`: author が上記 owner または当時の followee set に含まれるか。

後の grant/contact-list 変更で、既存 input の layer や即応性を遡及変更しない。同一 turn に異なる standing を混ぜないという現行不変条件 (`crates/social-runtime/src/lib.rs:1348`) を維持する。

## 6. 二層入力と固定窓

`place-policy/nostr/v1` は次の exact shape で、3 層すべてを必須にする。

```text
{
  debounce_ms: {
    owner: positive-u64,
    trusted: positive-u64,
    unknown: positive-u64
  }
}
```

値に既定はない。active 化前に運用者が全値を明示する。任意値とは層ごとに独立した正の整数値であり、0 を「即時」の別表現にしない。

### 6.1 即応層

次の AND のときだけ immediate input とする。

- `is_owner_or_followee=true`
- shape が (a) self external id への mention、(b) 当該 subject の outbound `external_refs` への reply、(c) 同じ outbound ref への kind-7 reaction のいずれか

owner/followee の通常投稿、profile、boost、他人宛 reply/reaction は即応ではなく、その standing 層の debounce に入る。trusted であっても followee でない author は即応しない。

### 6.2 debounce 層

`subject_event_inputs` は event と対象 agent の関係を持つ永続 inbox である。key は `(place_id,subject_id,event_seq)`、主な値は `authority_layer`, `delivery_class`, `window_opened_at`, `window_due_at`, `state`, `activity_id` である。

窓は `(place,subject,authority_layer)` ごとの fixed, non-extending window とする。

1. 層に open window が無い最初の core 受信時刻を `opened_at`、`opened_at + debounce_ms[layer]` を `due_at` とする。
2. due 前の同層 event は同じ due に加える。到着のたびに延長しない。
3. due でその窓の pending 行だけを seq 順に 1 turn として claim する。他層を混ぜない。
4. immediate event は同じ層で先行 pending の event も seq 順に同じ turn へ claim し、その窓を閉じる。他層の窓には触れない。これで即応しつつ同一発言を後から二重処理しない。
5. engine slot が埋まっていれば永続 pending/claimed 状態を保持する。restart は過期 due を即再装填し、terminal activity の無い claimed 行を pending へ戻す。
6. turn が terminal になったときだけ claimed 行を consumed にする。NO_REPLY も terminal success であり同じ batch を再実行しない。

現在の「最初の event が窓を張り、既存予定を動かさない」根拠は `crates/social-runtime/src/lib.rs:1584`、due fire は `crates/social-runtime/src/lib.rs:4501`。ただし現物は place 共通 1 窓なので、層別の正本は本項と `subject_event_inputs` である。

### 6.3 初回履歴

接続直後に relay が返す履歴も live と同じ core 受信時刻規則で窓へ入れる。初回だけ即 flush、EOSE flush、件数閾値 flush は禁止する。大量の初回 batch は 1 回の平均的な応答に寄るという性質を受容し、特別な要約器を gate に置かない。履歴 replay でも origin dedup 後に新規のものだけが input になる。

## 7. 複数 relay と dedup（#738）

`gate-config/nostr/v2.relays` は 1..N の canonical WSS URL、重複なし。例は `wss://relay-a.example`, `wss://relay-b.example`, `wss://relay-c.example` とする。

- 各 relay worker は同一 watch を独立に subscribe / reconnect する。1 本の切断で他を再接続しない。
- 同じ signed event id が複数 relay から届いても、`external_origins` の `(origin_scope_id,external_id)` で 1 append に畳む。relay URL を origin scope や external id に含めない。
- 既存 external id と異なる verified bytes が来た場合は invariant violation として診断し、新 event/input を作らない。
- publish は 1 回だけ署名し、同一 event bytes/id を全 relay へ送る。1 relay 以上の明示 accept で succeeded、全 relay の明示 reject で failed、accept なしで timeout/disconnect を含むと indeterminate。indeterminate を自動再署名・再送しない。
- 発行 event id は `external_refs` に 1 回記録し、relay 数を public result の意味にしない。

現在の single relay loop と再接続 (`crates/nostr-gate/src/main.rs:548`) および core の origin transaction (`crates/social-runtime/src/lib.rs:1133`) が差替え seam である。

## 8. outbound と個別 issue

### #745 kind 0 発行

公開 tool を増やさず、台帳 `nostr_run` operation の `profile` action を protocol 2 Nostr gate が実装する。core が route/権限と typed args を確定し、gate は kind 0 event を署名して §7 の relay pool へ発行する。gate は「誰が profile を変更できるか」を判断しない。成功 origin は他の発行と同じ `external_refs` に保存する。

### #747 NO_REPLY fail-closed（engine/core 契約）

NO_REPLY の semantic result は delivery 0 で terminal。空・parse ambiguity・engine failure を発話へ補完せず、gate へ effect を送らない。明示 NO_REPLY の batch は consumed にして自動 retry しない。これは Nostr gate 実装ではなく engine/core の受理契約だが、即応 E2E の受入条件に含める。

### #748 settled 文脈（core 契約）

background activity は origin input range、origin standing、accepted tool call 名、args の bounded typed representation、activity id を保持する。settled turn は `result` だけでなくこの origin request + accepted tool call + result を明示的に再構成し、間に大量 event が入っても read cursor により origin を失わない。settled は origin standing を継承し、Unknown/System へ落とさない。

今日の隔離観測で、決着 event 自体は発火した一方、次 context が元 request/tool call を含まず NO_REPLY になった (`../testaro/DEBUG-NOREPLY-REPORT.md:12`, `../testaro/DEBUG-NOREPLY-REPORT.md:18`, `../testaro/DEBUG-NOREPLY-REPORT.md:24`, `../testaro/DEBUG-NOREPLY-REPORT.md:158`)。現物の cursor 前進は `crates/social-runtime/src/lib.rs:2233`, `crates/social-runtime/src/lib.rs:2397`、最小 settled append は `crates/social-runtime/src/lib.rs:3832`。この契約の実装は #748 側だが、これ無しを Nostr 即応完了とは判定しない。

## 9. protocol 2 差分

Discord v15 の protocol 2 hello/bind/effect/result/error の envelope をそのまま使う。Nostr 固有差分だけを次に限定する。

- hello: `kind_id=nostr`, instance/revision、`origin_scope=kind_address`, `ingress_discovery=prebound`。
- bind metadata: `gate-binding/nostr/v1`。
- inbound native carrier: `nostr-event/v1`。relay URL は carrier に入れても診断値であり identity ではない。
- control fact: watch とは別の self/kind-3/limit-1 subscription で得た verified contact-list を core が `gate_principal_sets` へ反映できる形で運ぶ。通常 place event として発火させない。
- operations: 既存 Nostr effects/tools に profile(kind 0) を含む。unknown enum/schema は近似せず reject。

protocol 1 `name=nostr` は v15 compatibility seed のみ。新 Nostr v2 instance を hello から自動作成せず、active revision と exact-one で照合する。

## 10. 隔離受入試験

実 relay、実鍵、実ユーザー識別子を使わない。#746 の synthetic relay 群と mock LLM を使い、3 agent instance × 3 relay の topology を作る。

1. **watch**: mention/author/keyword の各単独一致と OR、一致なし、kind global AND、case-sensitive keyword、EOSE 非発火。
2. **型**: signed kind 0/1/6/7 と未知 watch kind を流し、`profiled/said/boosted/reacted/native`、target、raw bytes が一致し said へ潰れない。
3. **dedup**: 同一 event を 3 relay から順序違い・再接続 replay で送り、`events=1`, `external_origins=1`, `subject_event_inputs=1`, turn 最大 1。
4. **層別窓**: Owner/Trusted/Unknown に異なる interval を指定し、固定 due、非延長、層を混ぜないことを fake clock で検証する。
5. **初回**: 多数の replay を EOSE 前後に送り、定常と同じ 1 fixed window、EOSE flush なし、mock LLM call 1 を検証する。
6. **即応**: owner と followee の self 宛 reply/mention/reaction は clock advance なしで claim。通常投稿と non-followee trusted の同形 event は各窓まで claim しない。
7. **snapshot**: event 後に followee/grant を変更しても既存 input layer が変わらない。refresh failure は前集合保持、初回 failure は空集合。
8. **crash**: open window と claimed-before-activity を各点で再起動し、due と exactly-once consumption が復元される。
9. **publish pool**: kind 0 を一度署名し 3 relay に同一 id。2 accept/1 reject は success、全 reject は failed、timeout 混在は indeterminate。
10. **#747**: mock LLM の NO_REPLY / invalid / failure で external delivery 0。明示 NO_REPLY の input は再実行しない。
11. **#748**: tool result 前に多数 event を interleave しても、settled context に synthetic original request/tool call/result が揃い、origin standing を保つ。

## 11. 未決と禁止事項

未決は本番の `owner/trusted/unknown` 各 `debounce_ms` 値だけである。要件から数値は導けないため推測しない。schema、窓算法、設定箇所は確定しており、実装者は値を parameter として書けるが、運用者が 3 値を与えるまで active v2 revision へ切り替えられない。設定面は既存 Nostr PUT と `configure_nostr` が同じ core command を呼び、v2 の watch/relay/owner と 3 interval を 1 transaction で置換する。

禁止事項:

- relay ごとに place/binding/origin scope を増やすこと。
- gate 内で standing、owner/followee 即応、窓、turn、NO_REPLY を決めること。
- kind 0/6/7/未知を said へ近似すること。
- 起動 backlog、EOSE、件数で窓を早期 flush すること。
- owner/followee 解決失敗を全許可、display name、別 instance identity で補完すること。
- v1 config から debounce interval を捏造して v2 を active にすること。

<!-- ledger-reference-manifest
{"entity_ids":["events","external_origin_scopes","external_origins","external_refs","gate_bindings","gate_connections","gate_instance_revisions","gate_instances","gate_kinds","gate_principal_sets","gate_subject_identities","place_default_policies","places","subject_event_inputs","subject_routes"],"resource_ids":[],"coverage_ids":[],"staging_ids":[],"transform_ids":[]}
-->
