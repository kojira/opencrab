# nostr-gateway

Nostr inbound の独立 binary。`nostaro watch` 子プロセスの JSONL を V3 `said` へ写すだけ。投稿しない。Bearer を持たない。core crate の wire DTO に依存しない。

## 起動 ABI

```text
nostr-gateway /path/to/placement.json
```

argv は placement JSON 1 個だけ。HTTP listen はしない（ready=listen ではない）。配置が正しければ UDS client を張り、watch 子を起動して生存する。listen 文言を ready protocol にしない。

`NOSTARO_SECRET_KEY` は起動時に process env から除去し、watch child env にだけ渡す。argv・log・status・config に出さない。

## placement

`http_bind` は置かない。`core_socket` は絶対 path。`nostaro_bin` は nonempty。instance ごとに UDS 1 本。`config_b64` の decode バイトが hello `config_digest` の源。canonical config は relays / filter / self_pubkey / watches と optional `delivery_mode` / `name`。秘密を含めない。

default(メンション)車線は常設。watch は追加車線。同じ `address` の binding 1 枚を共有する。
メンション車線の nostaro argv は `--match=any` に加え、instance config の `self_pubkey`（と `name` があれば名前）を `--keyword` として付ける。条件ゼロの空網にはしない。この車線の said は `route=immediate`（束ね待ちしない）。`beyond_self` は false。
watch child は bind ack の後にだけ起動する。切断で child を止め、読取済み未送信は破棄して再送しない。
`SaidOutcome` は Accepted / NotAdmitted / Disconnected / WireErr を記録し、`store_error` / `bad_request` はカウンタに残す。turn 実行中の said は core の session queue 32 に積む。`PostRefuse::Busy`（`said refused; binding busy`）はキュー満杯の応答にだけ使う。
watch の有効フィルタは `filter_json`。アンカー `beyond_self` は watch 設定値（`!p_self` の代用はしない）。

## 写像

- origin: `nostr:event:v1:{lane}:{event_id}`。`lane` は `default` または `watch:{id}`
- 本文先頭: 版付きアンカー `[NOSTRGATE/V1 {…}]`（key 順固定）
- Bundle の次行: `[NOSTRBUNDLE/V1 [origin…]]`（index 順。coordinator が最初の非重複 member で全 origin を照合する）。flush の各 member は `post_said_receipt`（Accepted 後も次 origin を送る）
- その下: 採取 §3 の履歴本文 renderer
- kind 4/1059 が現れたら `route=immediate`（Discard は core）
- 受信した `say` は投稿せず `external_rejected`

## 検収

`cargo test -p opencrab-nostr-gateway`

## 関連

- 本設計: `DESIGN-NOSTRGATE.md`
- 線の契約: `DESIGN-EXTGATE-V3.md`
- V3 client: `opencrab-gate-client`
