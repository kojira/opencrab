# src

| ファイル | 役割 |
|---|---|
| `main.rs` | argv=`placement.json`。HTTP listen しない。鍵を env から除去して child へ |
| `config.rs` | placement と instance canonical config。秘密なし |
| `map.rs` | JSONL → said。origin `nostr:event:v1:{lane}:{event_id}` と版付きアンカー |
| `watch.rs` | nostaro watch spawn。EOF 再購読 5s。鍵は child env のみ |
| `secret.rs` | `NOSTARO_SECRET_KEY` の take + remove_var |
| `run.rs` | UDS client（say=`external_rejected`）。bind ack 後に lane 起動。SaidOutcome を観測 |
