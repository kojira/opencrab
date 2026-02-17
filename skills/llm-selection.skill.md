---
name: llm-selection
description: "LLM選択スキル - タスクに応じて最適なLLMを自分で選ぶ"
version: 1
actions:
  - select_llm
  - get_available_models
  - get_current_llm_config
---

# LLM選択マニュアル

あなたは状況に応じて使用するLLMを自分で選択できます。

## 1. いつLLMを切り替えるか

### 高性能モデルに切り替える場合
- 複雑な推論が必要なとき
- 重要な判断を下すとき
- 創造的なアイデアが必要なとき
- 長文の分析・要約が必要なとき

### 高速モデルに切り替える場合
- 単純な応答で十分なとき
- 素早い反応が求められるとき
- コストを抑えたいとき

### ローカルモデルに切り替える場合
- プライバシーが重要なとき
- オフラインで動作させたいとき
- APIコストを完全に避けたいとき

## 2. 利用可能なモデルエイリアス

| エイリアス | 用途 | 特徴 |
|-----------|------|------|
| `fast` | 高速応答 | 低コスト、シンプルなタスク向け |
| `smart` | 高性能 | 複雑な推論、高品質な出力 |
| `reasoning` | 深い推論 | 数学、論理、複雑な問題解決 |
| `creative` | 創造的 | アイデア生成、ライティング |
| `local` | ローカル | プライバシー重視、オフライン |
| `cheap` | 低コスト | 大量処理、コスト最小化 |

## 3. 選択の例

### 例1: 複雑な分析タスク
```
select_llm(
    purpose: "thinking",
    model_alias: "reasoning",
    reason: "複雑な問題を分析するため、深い推論能力が必要",
    duration: "this_turn"
)
```

### 例2: 大量の要約タスク
```
select_llm(
    purpose: "analysis",
    model_alias: "fast",
    reason: "多数のドキュメントを素早く要約するため",
    duration: "this_session"
)
```

### 例3: プライバシー重視
```
select_llm(
    purpose: "conversation",
    model_alias: "local",
    reason: "機密情報を含む会話のため、ローカルで処理",
    duration: "this_session"
)
```

## 4. 注意事項

- 切り替えには若干のオーバーヘッドがある
- 頻繁な切り替えは避ける
- 不明な場合はデフォルトのまま使用
- ツールコーリングはFunction Calling対応モデルが必須
