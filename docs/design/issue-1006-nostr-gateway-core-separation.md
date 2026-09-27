# D-1006: Gateway/Core 分離設計（全gateway共通、Nostr #1006適用）

状態: draft。実装中。main merge 禁止。人間QC OKまで main へ入れない。

関連正本: `docs/design/external-gate.md`。本設計は external gate V3 の「gateway は外部サービスとの I/O と wire 変換を所有する別プロセス」「core は gateway の振る舞いを警察しない」という境界を、全gateway共通の実装境界として固定し直す。Nostr #1006 は最初に露呈した具体例であり、Nostrだけの特別対応で済ませない。ただし #1006 のlive QC対象は `くらぶ` Nostr QC であり、Discord gateway のlive挙動変更はこの案件の必須QCには含めない。

## 目的

OpenCrab の gateway 全般を、core に個別 gateway 依存を残さない形へ戻す。今回の QC 障害では、Nostr の受信許可情報が core DB の `gate_instances.config_b64` 内 `access` に無いことで、gateway が投稿を受信しても admission で落としていた。これは復旧上は `access` を補うことで直せるが、分離設計としては不適切である。

本設計の目的は、受信前・配送前に gateway が必要とする外部サービス固有情報を gateway 側の責務に戻し、core は gateway 種別ごとの詳細 config / authority を所有しない状態へ進めることである。

## 現状の問題

現行実装では、generic external gate の表である `gate_instances.config_b64` が、gateway側 runtime config の置き場として使われている。Nostr では `crates/nostr/src/gate_provision.rs` が Nostr instance config を生成し、core DB の `gate_instances.config_b64` へ保存している。この config には少なくとも以下の Nostr 固有情報が含まれる。

- relay list
- self pubkey
- bot display name
- watch/session 設定
- `access.owner`
- `access.trusted_users`
- `access.followees`
- `access.co_agents`

このうち `access.*` は gateway が受信イベントを core へ渡す前に判断するための情報であり、core が個別 gateway config として保持・生成するべきではない。これはNostrに限らない。Discordであれば channel/guild/member/role/webhook/token 周辺、将来の別gatewayであればその外部サービス固有の routing/admission/delivery authority も同じく core の責務ではない。

core に置くと、core 側 projection 漏れがそのまま gateway admission failure になる。また、gateway 分離後も core schema/API が各gatewayの許可モデルに結合し続ける。

## 方針

### 分離原則

core は gateway 種別ごとの意味を知らない。`kind_id` は routing/lifecycle 用の opaque な分類であって、core が Nostr pubkey、Discord channel、relay、watch filter、allow list、webhook、外部roleなどの意味を解釈する根拠ではない。

core は以下だけを扱う。

1. gateway instance の存在、kind、subject、revision、enabled/deleted の lifecycle
2. binding/address など、core が配送・紐付けのために必要な抽象情報
3. gateway config の opaque revision/digest 参照。ただし中身を core が生成・解釈しない

gateway 固有の runtime config と admission/delivery authority は gateway 側が扱う。

1. 外部サービス接続先、監視条件、投稿先、認証材料は gateway 管理面の config
2. 外部ユーザー・外部チャンネル・外部ロール等の許可集合は gateway 側の allow/routing store
3. gateway 起動時は gateway 側が自分の store/config を読んで fail-loud に検証する
4. core は gateway固有 authority を作らない、保存しない、正規化しない、差分判定しない

### 禁止する状態

以下は本設計では不合格とする。

- core DB の `gate_instances.config_b64` が gateway固有 admission/delivery authority を含み、それを gateway 判断の正として扱う。
- gateway別crateが allow/routing/secret/service-specific config を生成し、その byte 列を core DB の revision 対象にする。
- migration が core の gateway-specific config を権威として destination gateway config を再構成する。
- QC 復旧時の手動 `access` 投影を恒久仕様としてテスト固定する。

### 許容する一時状態

以下は移行中だけ許容する。

- 既存 runtime に旧 gateway-specific `config_b64` が残っている。
- gateway-side store への移行前に、起動済みQCを止めないための暫定 config が残る。

ただし、これらを新しい source of truth として扱うテスト・ドキュメント・main merge は禁止する。

## 非目標

この設計では以下を同時に行わない。

- production 変更
- Discord gateway の挙動変更を #1006 QC の必須live対象にすること
- Nostr relay/watch 挙動の推測修正
- 既存 trusted user データの削除
- UI/API 全面再設計
- main への merge

main への取り込みは、人間 QC が `くらぶ` の live 動作を確認し、明示的に OK してから行う。

## 対象 seam

### 現在の主な結合点

- `crates/nostr/src/gate_provision.rs`
  - `desired_nostr_config_b64`
  - `desired_nostr_config`
  - `provision_nostr_gate`
  - `revise_nostr_gate`
  - `build_allow_sources`
- `crates/nostr/src/adapter.rs`
  - `AllowSources`
  - `pre_record_drop`
  - `accept_nostr_inbound` への allow 入力
  - これはNostr具体例だが、外部サービス固有admissionを core-side adapter に戻す構造そのものを撤去対象にする。
- `crates/gateway-migrate/src/destination_legacy_nostr.rs`
  - core `gate_instances.config_b64` と gateway destination config の同期
  - legacy allow 情報から `config.access` を再構成する処理
- `crates/gateway-migrate/src/destination_core_nostr.rs`
  - core-only historical source から `config.access` を再構成する処理
  - 分離後は core config ではなく gateway-side allow store への一方向移行にする。
- `crates/gateway-migrate/src/destination_plans.rs`
  - trusted users が gateway config に表現されていることを証明する処理
  - 分離後は gateway-side store に表現されていることを証明する。
- `crates/nostr-gateway/src/config.rs`
  - runtime `InstanceConfig.access`
  - empty access fail-loud validation
- `crates/discord-gateway/src/config.rs`
  - runtime `InstanceConfig.access`
  - system reactions / Discord IDs などの Discord-specific runtime config
  - Nostrと同型に、core-owned `config_b64` へ gateway authority を置いている既存例として扱う。

### 変更後の責務境界

- gateway別core-side crate は lifecycle/binding helper に縮退する。
- gateway runtime config 生成は gateway-side 管理面へ移す。
- legacy migration は「core config を権威として gateway config を作る」処理をやめ、gateway-side store/config への一方向移行として扱う。
- gateway runtime は必要な authority が空/不整合なら fail-loud にする。ただしその authority は core 生成物ではなく gateway 側 store/config から来る。
- core-side adapter に外部サービス固有admissionを二重実装しない。coreが受ける `said` は、generic subject/binding/session の認可に限って扱う。

## 実装前完了チェックリスト

実装着手前に、設計条項ごとに以下を満たす。

| ID | 設計条項 | production seam | assertion-level RED | minimal GREEN | 保持する成功証拠 | 禁止事項 | 後続段階への影響 |
|---|---|---|---|---|---|---|---|
| D1006-1 | core は gateway-specific authority を保持しない | generic `gate_instances.config_b64` と gateway-specific provisioning | core provisioning test で `gate_instances.config_b64` に external authority が入っていたら失敗 | provisioning 後 config が authority を含まない | focused test log | core projection helper の強化 | Phase 3 の gateway-side store が必須になる |
| D1006-2 | gateway が admission/delivery authority を所有する | gateway runtime config/store loader | gateway 起動時 authority が空/不整合なら fail-loud | gateway-side store/config から必要情報を読める | gateway unit/integration test | 空authorityで起動して silent drop | QC live gate の前提になる |
| D1006-3 | migration は core config を正にしない | gateway migration/reconciliation | migration test で core config を編集しないと destination が直らないなら失敗 | legacy data から gateway-side store へ一方向移行 | migration focused test | core config の再構成 | Phase 4 で旧projection撤去可能 |
| D1006-4 | main merge gate | branch/worktree運用 | 人間QC OKなしでmain候補にすると失敗 | QC evidence と明示OK後のみmerge候補 | QC証跡とユーザーOK | production/main先行投入 | リリース判断の安全境界 |

## 段階設計

### Phase 1: fail-loud と設計固定

目的: core が Nostr `access` を silently 生成・保持する前提を追加で固めない。

実施内容:

- `nostr-gateway` の empty `access` fail-loud validation は維持する。
- core-side config projection を恒久解として扱わないことを設計書に固定する。
- core が `access` を source of truth として扱うテストを追加しない。

完了条件:

- 設計書が `docs/design/issue-1006-nostr-gateway-core-separation.md` に存在する。
- 人間が方針を確認できる。

### Phase 2: core-side generation の縮小

目的: gateway別crateが外部サービス固有 authority を受け取り runtime config を作り、それを core DB に置く経路を廃止または非権威化する。Nostr では `crates/nostr` が `AllowSources` を受け取る経路が最初の対象である。

実施内容:

- `desired_nostr_config*` から `access: &AllowSources` 依存を外す。
- core DB の `gate_instances.config_b64` には Nostr admission authority を含めない。
- 既存互換が必要な場合も、core 側に置くのは opaque reference/version に留める。
- テストで、core provisioning 後の `gate_instances.config_b64` に `access.owner/trusted_users/followees/co_agents` が含まれないことを確認する。
- `build_allow_sources` は core-side config generation の入力として使わない。残す場合は gateway-side migration/input preparation に移す。
- `AllowSources` / `pre_record_drop` のような外部サービス固有admissionは core-side conversation admission の前段に置かない。必要なら gateway runtime crate 側へ移す。

完了条件:

- focused test が、core config に Nostr allow list が埋め込まれないことを検証する。
- `cargo test` の該当 package focused tests が通る。
- core code path で `AllowSources` が `gate_instances.config_b64` 生成へ流れない。
- core code path で external author の owner/trusted/followee/co-agent 判定が generic inbound 前段として残らない。

### Phase 3: gateway-side allow authority

目的: gateway が受信前に必要な allow 情報を gateway 側から読めるようにする。

実施内容:

- Nostr gateway 管理面または gateway DB に allow store を置く。
- owner/trusted/followee/co-agent を gateway instance 単位で解決する。
- gateway 起動時に allow store が空なら fail-loud にする。
- core DB の legacy trusted/allow 情報から gateway store へ移行する一回性 migration を用意する。

完了条件:

- core DB の `gate_instances.config_b64` を編集しなくても、gateway が owner/trusted admission を判断できる。
- くらぶ QC で通常 npub から「くらぶ」投稿 → Owner/Trusted admission → real LLM → public reply が通る。

### Phase 4: legacy projection の撤去

目的: core config を権威として gateway config/access を再構成する旧経路を消す。

実施内容:

- `gateway-migrate` の legacy Nostr projection が core `config.access` に依存しないようにする。
- core に Nostr 固有 allow projection を戻す helper/test を削除する。
- ドキュメントとテストで、新しい責務境界を固定する。

完了条件:

- core code に Nostr allow list の生成・正規化・比較が残らない。
- gateway-side migration/reconciliation tests が通る。

## データ配置

### core DB に残してよいもの

- `gate_instances.instance_id`
- `gate_instances.kind_id`
- `gate_instances.subject_id`
- `gate_instances.revision`
- `gate_instances.enabled/deleted_at`
- `gate_bindings.binding_id`
- `gate_bindings.address`
- `config_digest` 相当の handshake 用 digest。ただし digest 対象の内容を core が Nostr として構築しない。
- extgate transition 互換の generic `delivery_mode`。これは Nostr/Discord 等の個別gateway authorityではなく、core の最終配送契約確認に必要な共通bitに限る。

### core DB から出すもの

- 外部サービス固有の allow/routing list
- 外部サービス接続先 list
- gateway secret / token / nsec
- watch/subscription/filter の意味
- bot display name など、外部runtime configとしてのみ必要な値

### gateway 側に置くもの

- instance ごとの gateway runtime config
- instance ごとの admission/delivery/routing authority
- legacy allow/routing data からの移行結果
- gateway-specific canonicalization

## 互換・移行

1. 既存 QC runtime は、移行完了まで暫定 `access` が残る可能性がある。
2. 新しい code path は core へ Nostr allow authority を再投影しない。
3. legacy data から gateway allow store への移行は idempotent にする。
4. `destination_core_nostr.rs` のような historical importer は、core config JSONを書き換えず gateway-side store rows を生成/検証する。
5. `destination_plans.rs` の proof は「configに含まれる」ではなく「gateway-side authority storeに含まれる」を見る。
6. 移行後、gateway は gateway-side allow store を読めない場合に起動失敗する。
7. rollback は「旧candidateへ戻す」または「gateway-side allow storeを前世代へ戻す」で行い、core config.access 再生成を rollback 手段にしない。

## 人間 QC gate

main へ入れる前に、少なくとも以下を人間 QC で確認する。

1. 通常の Nostr クライアントから `くらぶ` を含む新規投稿を行う。
2. gateway が新規 event を受信する。
3. gateway が Owner または TrustedUser として admission する。
4. core が real LLM request を発行する。
5. public Nostr reply が配送される。
6. core DB の `gate_instances.config_b64` に Nostr allow authority を戻していないことを確認する。

人間 QC が OK するまで main に merge しない。

## リスクと扱い

- 既存 QC runtime は一時的に core config に `access` が残っている可能性がある。これは復旧用の暫定状態であり、恒久設計の正ではない。
- 旧データの移行順序を誤ると gateway が fail-loud で起動しない。これは silent drop より安全なので、移行時に明示的な rollback/再投影手順を用意する。
- core から gateway config を完全に消すには既存 admin/revision API と extgate `final_delivery` 互換チェックの調整が必要になる。現段階では generic `delivery_mode` だけを残し、core に Nostr allow/runtime authority を戻さない。

## 現時点の判断

今回の `access` 欠落障害の直接原因は、gateway が必要とする admission authority を core-side config projection に依存していたこと。したがって恒久修正は「core projection を強化する」ではなく、「gateway-side authority に戻し、core から個別 gateway 依存を外す」で進める。
