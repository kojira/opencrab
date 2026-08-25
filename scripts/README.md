# scripts

| スクリプト | 役割 |
|---|---|
| `local-ci.sh` | `.github/workflows/ci.yml` と同じ手順をローカルで走らせる。`CARGO_TARGET_DIR` 未設定時は短い既定（`/tmp/opencrab-ci-tgt`）を使う（深い worktree で e2e ソケットが溢れるのを避ける） |
| `verify-v43-transplant-copy.sh` | 載せ替え工程 3。本番 DB を sqlite3 `.backup`（読取）でコピーし、v43 適用前後の既存全表全行 diff ゼロを機械確認する。ソースパスは `OPENCRAB_REHEARSAL_DB`（リポジトリに書かない）。作業コピーは `TMPDIR` 下（7GB 級を 3 本置くので空き容量に注意） |
| `check-no-private-identifiers.sh` | 公開リポジトリへ実在識別子が混入していないか |
| `check-deps.sh` | クレート依存境界 |
| `capture-baseline-l1.sh` / `capture-baseline-l2.sh` | baseline 採取 |
