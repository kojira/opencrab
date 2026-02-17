---
name: llm-meta-analysis
description: "LLMメタ分析スキル - 自分のLLM利用をメタ視点で分析・最適化する"
version: 1
actions:
  - analyze_llm_usage
  - evaluate_response
  - optimize_model_selection
  - get_llm_metrics
  - compare_models
---

# LLMメタ分析マニュアル

あなたは自分のLLM利用を**メタ視点で分析**し、最適化する能力を持っています。

## 1. メトリクスの自動収集

すべてのLLM呼び出しで以下が自動的に記録されます：
- コスト（入力/出力トークン数、推定費用）
- レイテンシ（応答時間）
- 用途（thinking, conversation, tool_calling等）
- タスクタイプ

## 2. 推論結果の自己評価

重要なタスクの後は `evaluate_response` で自己評価してください：

```
evaluate_response(
    quality_score: 0.85,
    task_success: true,
    evaluation: "複雑な推論を正確に行えた。ただし応答が少し冗長だった。",
    would_use_again: true
)
```

### 評価の観点
- **品質**: 出力の正確さ、有用さ
- **効率**: トークン数に対する価値
- **適切さ**: このタスクにこのモデルは適切だったか

## 3. 利用状況の分析

定期的に `analyze_llm_usage` で自分の利用パターンを分析：

```
analyze_llm_usage(
    period: "last_week",
    group_by: "model",
    focus: "cost_efficiency"
)
```

### 分析で得られる情報
- モデル別のコスト・品質・速度
- タスク別の最適モデル
- コスト効率（品質/コスト比）
- 改善の推奨事項

## 4. モデル選択の最適化

`optimize_model_selection` で最適なモデル構成を提案：

```
optimize_model_selection(
    optimization_goal: "balance",
    budget_limit_usd: 10.0,
    min_quality_threshold: 0.8,
    apply_immediately: false
)
```

### 最適化の目標
- **minimize_cost**: コスト最小化（品質を維持しつつ）
- **maximize_quality**: 品質最大化（予算内で）
- **balance**: コストと品質のバランス
- **minimize_latency**: 応答速度優先

## 5. 学習と適応

分析結果に基づいて、以下を学習・記録してください：

### タスク別の最適モデル
- 「複雑な推論 → reasoning モデルが最適」
- 「単純な応答 → fast モデルで十分」
- 「創造的タスク → creative モデルが高品質」

### コスト意識
- 「このタスクは fast で十分だった。次回から切り替える」
- 「品質が重要な場面では smart を使う価値がある」

### 失敗からの学習
- 「このモデルはツールコーリングが不安定だった」
- 「長文生成では別のモデルの方が良い」

## 6. 定期的な振り返り

ハートビート時や議論の合間に、以下を確認：

1. 今日のコストは予算内か？
2. 品質スコアは目標を達成しているか？
3. 非効率なモデル選択はなかったか？
4. 最適化の余地はあるか？

## 7. 自動最適化の例

### 例1: コスト削減
```
分析結果: 「単純な応答に smart モデルを使用、コスト効率が低い」
アクション: select_llm(purpose: "conversation", model_alias: "fast")
結果: コスト50%削減、品質維持
```

### 例2: 品質向上
```
分析結果: 「複雑な推論で fast モデルを使用、成功率が低い」
アクション: select_llm(purpose: "thinking", model_alias: "reasoning")
結果: 成功率30%向上、コスト増加は許容範囲
```

### 例3: バランス調整
```
分析結果: 「全体的にコストが高い、品質は十分」
アクション: optimize_model_selection(goal: "minimize_cost", min_quality: 0.8)
結果: タスク別に最適モデルを再配置、コスト30%削減
```
