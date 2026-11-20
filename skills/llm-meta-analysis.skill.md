---
name: llm-meta-analysis
description: "LLMメタ分析スキル - 自分のLLM利用をメタ視点で分析・最適化する"
version: 2
permission: agent
actions:
  - analyze_llm_usage
  - evaluate_response
  - recall_model_experiences
  - save_model_insight
  - select_llm
---

# LLMメタ分析マニュアル

あなたは自分のLLM利用を**メタ視点で分析**し、モデル選びに活かせます。

## 1. メトリクスの自動収集

すべてのLLM呼び出しで以下が自動的に記録されます：
- 入力/出力トークン数、推定費用（`model_pricing` の料金で計算）
- レイテンシ（応答時間）
- 用途（`select_llm` の `purpose` など）

## 2. 推論結果の自己評価

重要なタスクの後は `evaluate_response` で自己評価してください（`evaluation` が必須）：

```
evaluate_response(
    quality_score: 0.85,
    task_success: true,
    evaluation: "複雑な推論を正確に行えた。ただし応答が少し冗長だった。",
    would_use_again: true,
    better_model_suggestion: "",
    tags: ["推論"]
)
```

### 評価の観点
- **品質**: 出力の正確さ、有用さ
- **効率**: トークン数に対する価値
- **適切さ**: このタスクにこのモデルは適切だったか

## 3. 利用状況の分析

`analyze_llm_usage` で自分の利用状況の生データを取得します：

```
analyze_llm_usage(
    period: "last_week",      # last_hour / last_day / last_week / last_month / all
    model_filter: "gpt-6.2-sol"  # 省略で全モデル
)
```

モデル別・用途別の統計が返ります。結論は自分で出します。

## 4. 過去の経験を思い出す・残す

- `recall_model_experiences(model_filter, include_notes, evaluation_limit)` — 数値メトリクス、自分の評価コメント、経験ノートをまとめて返す。
- `save_model_insight(situation, observation, recommendation, provider, model, tags)` — 気づいたこと（`situation` と `observation` が必須）を経験ノートとして残す。

## 5. モデルを変える

分析の結果モデルを変えるときは `select_llm` を使います。変更はこの会話の次のターンから効きます（詳しくは `llm-selection` スキル）。選べるモデルは `get_system_info` の `selectable_models` にあります。

## 6. 定期的な振り返り

定時実行や会話の合間に、以下を確認：

1. コストは想定内か？
2. 品質スコアは目標を達成しているか？
3. 非効率なモデル選択はなかったか？
