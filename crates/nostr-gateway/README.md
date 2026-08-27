# nostr-gateway

Nostr inbound の独立 binary。`nostaro watch` 子プロセスの JSONL を V3 `said` へ写すだけ。投稿しない。Bearer を持たない。core crate の wire DTO に依存しない。

## 起動 ABI

```text
nostr-gateway /path/to/placement.json
```

argv は placement JSON 1 個だけ。HTTP listen はしない（ready=listen ではない）。配置が正しければ UDS client を張り、watch 子を起動して生存する。listen 文言を ready protocol にしない。

`NOSTARO_SECRET_KEY` は起動時に process env から除去し、watch child env にだけ渡す。argv・log・status・config に出さない。

## placement

`http_bind` は置かない。`core_socket` は絶対 path。`nostaro_bin` は nonempty。instance ごとに UDS 1 本。`config_b64` の decode バイトが hello `config_digest` の源。canonical config は relays / filter / self_pubkey / watches と optional `delivery_mode`。秘密を含めない。

watches が空なら default lane 1 本。1 件以上なら default なしで各行が 1 lane。同じ `address` の binding 1 枚を共有する。

## 写像

- origin: `nostr:event:v1:{lane}:{event_id}`。`lane` は `default` または `watch:{id}`
- 本文先頭: 版付きアンカー `[NOSTRGATE/V1 {…}]`（key 順固定）
- その下: 採取 §3 の履歴本文 renderer
- kind 4/1059 が現れたら `route=immediate`（Discard は core）
- 受信した `say` は投稿せず `external_rejected`

## 検収

`cargo test -p opencrab-nostr-gateway`

## 関連

- 本設計: `DESIGN-NOSTRGATE.md`
- 線の契約: `DESIGN-EXTGATE-V3.md`
- V3 client: `opencrab-gate-client`
