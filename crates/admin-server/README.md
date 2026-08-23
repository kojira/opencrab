# opencrab-admin-server

ダッシュボード（管理面）の**読み取り専用 API + React SPA 配信**（#767 Phase 1）。

会話ゲート（`crates/web-gate`）とは**別プロセス・別クレート**。会話ゲートに管理 API を混ぜない。
DB は常に**読み取り専用**で開く（書き系は Phase 1 の範囲外）。

## データの向き先（AGREED §2.11）

- oc2 が概念を置き換えたもの（agent→subject、session→place、会話ログ→events/turn_records、
  memory→memories）は **oc2 store の新テーブル**を読み、旧ダッシュボードの JSON 形へ写す（フロント無改変）。
- それ以外（schedules / trusted-users / allowed-commands / model-pricing 等）は**本体 DB スキーマ
  （正本）の旧テーブル**を `opencrab-db` の queries で読む。旧 `crates/server` のハンドラを移植したもの。
- 正本スキーマへ**未移行**のテーブル・列は、偽の空配列を返さず **501** で明示する（migration 側の責務）。
  データ実体の無い機能（skills / soul presets / analytics 等）も 501。
- 書き系メソッドはルートに載せない（axum が **405** を返す＝偽の成功を作らない）。

## ビルドと起動

SPA を先にビルドしてから起動する（`web/dist` を配信する）。

```sh
# 1) SPA をビルド（web/dist を生成。dist はコミットしない）
cd web
npm ci && npm run build     # あるいは pnpm install && pnpm build
cd ..

# 2) admin-server を起動
#    引数:   admin-server <db_path> [http_port] [web_dist_dir] [compaction_ratio]
#    既定:   db=data/opencrab.db  port=8787  web_dist=web/dist  compaction_ratio=0.5
#    環境変数でも指定可: OPENCRAB_ADMIN_DB / _PORT / _WEB_DIST / _COMPACTION_RATIO
cargo run -p opencrab-admin-server -- data/opencrab.db 8787 web/dist
```

**稼働中 core と同じ DB を開くときも安全**: `Store::open` は使わず、SQLite を read-only
（`SQLITE_OPEN_READ_ONLY`）で直接開いて観測する（稼働中 core の epoch を閉じる副作用を避ける）。
