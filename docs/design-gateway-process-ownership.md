# Gateway process ownership

## Decision

個別 gateway の設定・秘密・外部 I/O・受信判定・表示整形・起動・再起動は、gateway daemon だけが所有する。`core` / `actions` / `db` / `extgate` / `gate-client` / `server` は gateway 名を解釈しない。

## Process boundary

```text
operator
        |
        | gateway-owned admin interface
        v
gateway daemon ---------------- gateway-owned SQLite / secrets
        |
        | generic gate admin: instance + binding config bytes
        v
core extgate UDS -------------- core-owned conversation DB
```

### Core/server owns

- subject と session
- opaque `kind_id`、instance、binding、revision、config bytes
- Said dedup、会話記録、ターン直列化、LLM、Say delivery

### Gateway daemon owns

- platform設定DBとschema
- secretの取得・暗号化・復号・環境変数scrub
- platform設定からopaque instance/bindingを敷設する処理
- 外部CLI/API childの起動、停止、再起動、backoff
- admission、watch、bundle、attachment URL、reply target、表示文脈
- gateway固有admin operationとvalidation

## Compatibility

- 境界違反のserver内gateway管理HTTP APIは削除し、gateway daemon自身のadmin interfaceへ置換する。
- Rust内部のplatform固有trait・enum・定数はgateway crateへ移す。
- core-owned `gate_instances.kind_id` は保存・照合にだけ使い、値を列挙・比較しない。

## Dependency direction

- shared crates (`core` / `db` / `gateway` / `actions`) must not depend on any concrete gateway crate.
- a concrete gateway daemon may depend inward on shared crates and its platform implementation crate.
- the platform implementation crate must not depend back on its gateway daemon crate.
- `server` must not depend on a concrete gateway crate in production dependencies.
- CI verifies the reverse dependency tree of each concrete gateway has no shared/server production crate.

## Startup and lifecycle

- serverはgateway childをspawn/superviseしない。
- gateway daemonはserverと別のservice unitから常時起動する。
- gateway daemonは自分の設定DBを読み、enabled instanceを起動する。
- 設定更新はdaemon内で stop → revision/provision → start の単一経路を通る。
- 旧server内managerとの並走、fallback、feature flagは作らない。

## Migration

1. gateway daemonとgateway-owned admin UDSを先に実装する。
2. gateway daemonの一回限りimportで既存platform設定をgateway DBへ移す。秘密はgateway process内でのみ復号する。
3. serverのplatform管理API、manager、secret field、provision、child supervisionを削除する。
4. gateway daemon自身に必要なadmin interfaceを実装する。
5. shared production sourceのplatform語彙auditをallowlistなしで有効化する。
6. isolated QC後に旧設定表へのruntime参照を削除する。履歴migrationは新規runtime判断に使わない。

## Failure rules

- gateway daemon不在時にserver側fallbackはしない。
- secret、raw admin socket path、内部SQLはHTTP・logへ出さない。
- gateway設定更新とcore instance revisionの片方だけが成功した場合、gatewayは起動せずfail-loudにする。
