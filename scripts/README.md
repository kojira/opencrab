# scripts

| スクリプト | 役割 |
|---|---|
| `local-ci.sh` | `.github/workflows/ci.yml` と同じ手順をローカルで走らせる。`CARGO_TARGET_DIR` 未設定時は短い既定（`/tmp/opencrab-ci-tgt`）を使う（深い worktree で e2e ソケットが溢れるのを避ける） |
| `verify-v47-transplant-copy.sh` | 隔離済みv43 DBをSQLite `.backup`（読取）でコピーし、v47到達、既存列の全行digest不変、schema差分閉集合、二重適用no-op、source不変を確認する。入力は`OPENCRAB_REHEARSAL_DB`。旧`verify-v43-transplant-copy.sh`はこのscriptへ委譲する互換口。作業copyは`TMPDIR`下。 |
| `check-no-private-identifiers.sh` | 公開リポジトリへ実在識別子が混入していないか |
| `check-deps.sh` | クレート依存境界 |
| `check-samples-node.sh` | samples/node の runtime dependency 0 / Rust import 0 / Bearer 0 / 旧 route 実装 0（DESIGN-SAMPLES-NODE §5） |
| `capture-baseline-l1.sh` / `capture-baseline-l2.sh` | baseline 採取 |
| `webgate-provision` | operator 敷設。Bearer は admin 6 operation だけ。agent GET / sessions GET には付けない。config bytes は byte-exact `{"author_id":<encoded-owner-id>}`。gateway / browser へは渡さない |
