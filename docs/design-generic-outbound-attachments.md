# OpenCrab 汎用送信添付設計 D-1079a

_エージェントがワークスペースのファイルを gateway operation に添付する仕組み（core 側）。受信側は [design-generic-inbound-attachments.md](design-generic-inbound-attachments.md)（DC-953）。_

## 要件

- エージェントは既存の operation（例: `reply`）の引数でワークスペース相対パスを渡し、ファイルを投稿できる
- 新しい tool、新しい wire message、protocol version の変更は追加しない
- core は platform 語彙と operation 名分岐を持たない
- 失敗は tool error でモデルへ返し、半端な送信や黙った欠落を作らない
- OpenCrab 独自のサイズ上限は設けない（platform の上限で拒否される）

## 宣言（gateway）

gateway は operation の `input_schema` の top-level property に汎用 marker `"format": "workspace-file"` を付ける。型は string か、`items` で string の array。

```json
{
  "name": "reply",
  "input_schema": {
    "type": "object",
    "required": ["event", "text"],
    "properties": {
      "event": {"type": "string", "format": "short-ref"},
      "text": {"type": "string"},
      "files": {
        "type": "array",
        "description": "Workspace-relative paths of files to attach",
        "items": {"type": "string", "format": "workspace-file"}
      }
    }
  }
}
```

marker を宣言しない gateway では引数自体がモデルに見えない。宣言されていない field、marker の無い field は書き換えない。

## core の処理（`extgate/src/outbound_files.rs`）

`ExtgateOpsGatewayActions::execute` が short-ref 解決の直後、delivery/speech 行や invoke frame を作る前に行う。

1. 呼び出し元 agent のワークスペース `resolve_agent_workspace(workspace_base, agent_id)` で各パスを解決する。
2. 相対パスのみ許可する。絶対パス、`..`、ディレクトリや leaf の symlink、通常ファイル以外、存在しないファイルは拒否する。leaf は全 component を `O_NOFOLLOW` で開く。
3. `<db_dir>/attachments/outbox/<instance_id>/<uuid>.bin` に `create_new`・0600・fsync でコピーし、size と sha256 を計算する。
4. field の値を受信 `LocalFile` と同形の descriptor に置き換える。`local_path` は outbox root からの相対パス。

```json
{"id": "<uuid>", "name": "x.png", "media_type": "image/png", "size": 12345,
 "sha256": "<64 lower hex>", "local_path": "<instance_id>/<uuid>.bin"}
```

`media_type` は拡張子から推定し、不明なら `null`。

失敗時は tool error `workspace_file_rejected: <field>[<index>]: <reason>` を返す。deliveries 行、speech 行、invoke frame は 0 件で、それまでに作ったコピーは削除する。outbox 未設定で marker 付き field を使うと拒否し、フォールバックしない。

## 記録とライフサイクル

- 発話 op では speech の metadata に `"attachments":[{"name":"x.png"}]` を記録する。deliveries の payload は `{text}` のまま（DB CHECK）。
- コピーは pending entry（`Pending::Utterance` / `Pending::Invoke`）が所有し、終端（ok / err / close・indeterminate）で削除する。
- server 起動時に outbox を 0700 で作成し、前プロセスの残骸を掃除する（restart を越える call は無い）。

## gateway 側（`opencrab-gate-client::outbound`）

gateway は outbox root を placement で受け取り、`OutboundFile::parse` / `parse_list` と `open_verified(root, &file)` で読む。`local_path` は Normal component だけの相対パスであること、symlink を辿らないこと、size と sha256 の再検証を行う。raw string（旧 core）や非 object は `Malformed` として拒否する。新 gateway と旧 core の組み合わせは fail closed になる。
