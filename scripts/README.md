# scripts

| スクリプト | 役割 |
|---|---|
| `local-ci.sh` | `.github/workflows/ci.yml` と同じ手順をローカルで走らせる。`CARGO_TARGET_DIR` 未設定時は短い既定（`/tmp/opencrab-ci-tgt`）を使う（深い worktree で e2e ソケットが溢れるのを避ける） |
| `verify-v47-transplant-copy.sh` | 外部DBを開かず、synthetic fixtureでv43→v47のschema、対象backfill、constraint、再適用no-opを確認する。旧`verify-v43-transplant-copy.sh`はこのscriptへ委譲する互換口。 |
| `check-no-private-identifiers.sh` | 公開リポジトリへ実在識別子が混入していないか |
| `check-deps.sh` | クレート依存境界 |
| `check-samples-node.sh` | samples/node の runtime dependency 0 / Rust import 0 / Bearer 0 / 旧 route 実装 0（DESIGN-SAMPLES-NODE §5） |
| `capture-baseline-l1.sh` / `capture-baseline-l2.sh` | baseline 採取 |
| `webgate-provision` | operator 敷設。Bearer は admin 6 operation だけ。agent GET / sessions GET には付けない。config bytes は byte-exact `{"author_id":<encoded-owner-id>}`。gateway / browser へは渡さない |
