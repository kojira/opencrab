# DC-959 v0.2: v43→v47 synthetic migration verification

Issue: https://github.com/kojira/opencrab/issues/959

## 目的

現行のv43→v47 migration chainが期待するschema構造へ到達することを、小さなsynthetic DBだけで高速かつ決定的に検証する。旧v43専用scriptのpostconditionを現行v47へ更新する。

## v0.1からの変更

実DB copy、全table COUNT/digest、DB/WAL/SHM hash、A/B大容量copyを受入条件から削除する。v43→v47には既存tableの行数を増減するmigrationがないため、schema確認に実データ全走査を要求しない。

## 検証責務

- synthetic v43 fixtureをv47までmigrationする
- v44の`agents.subject_id` backfillが既存agentへ正整数・一意で設定されることを確認する
- v44の4 gate tables、index、guard triggersを確認する
- v45 `nostr_bundle_state`、v46 `conversation_snapshots`、v47 `gateway_operation_calls`と`gate_instances.operation_declaration_digest`を確認する
- migration再実行がno-opであることを確認する
- malformed schemaやconstraint違反を既存unit testsで拒否する
- 通常testおよびscriptは外部DBを開かない
- Pythonを必要としない

## Interface

`verify-v47-transplant-copy.sh`は互換名として残すが、外部DBを入力せず、`opencrab-db`のsynthetic schema migration testsだけを実行する。旧`verify-v43-transplant-copy.sh`は同scriptへ委譲する。

```bash
./scripts/verify-v47-transplant-copy.sh
```

## 非目的

- production/QC DBの読み書き
- 全rowのdata preservation監査
- migration SQLやschema versionの変更
- runtime、gateway、webの変更

## 受入条件

- synthetic v43→v47 schema testsが合格する
- `agents.subject_id` backfill・unique・positive guardsが合格する
- v44〜v47の新規table/column/index/trigger/constraint testsが合格する
- 再適用no-op testsが合格する
- scriptが外部DB、Python、production環境を必要としない
- CI、独立review、800行監査が合格する
