# DC-962 v0.1: Discord QC harness settlement wait

Issue: https://github.com/kojira/opencrab/issues/962

## Problem

`scenario_915_max_iterations_flag_only_on_last_delivered_say`は30回のLLM call到達後に固定800ms待機する。並列CI負荷では最後のcompletion reaction記録がassertionより遅れ、製品挙動が正しくてもfalse REDになる。

## Design

固定sleepを、対象`mlsay`の最後のmessage IDに対応するcompletion reactionがcaptureされるまでのbounded `wait_until`へ置換する。既存の5秒timeout、message ID相関、最後の投稿に1件・対象turn合計1件というassertionを維持する。

## Scope

- test-only
- runtime、API、DB、gateway実装は変更しない
- suite直列化やassertion緩和は行わない

## Acceptance

- 単独testと38件並列suiteが合格する
- timeout時はfail closedする
- 最後の対象say以外のcompletion reactionを成功条件にしない
