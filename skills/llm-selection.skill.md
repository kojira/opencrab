---
name: llm-selection
description: "LLM選択スキル - この会話で使うモデルを自分で選ぶ（select_llm）"
version: 2
permission: agent
actions:
  - select_llm
  - get_system_info
---

# LLM選択マニュアル

あなたは、いま話している会話で使うモデルを自分で変えられます。

## 1. 仕組み（ここを外すと「変えたのに変わらない」と誤解する）

- 1 つのターンは 1 つのモデルで走る。ターンの途中ではモデルは変わらない。
- `select_llm` は **この会話のモデル** を変える。オーナーの `/model` コマンドと同じ設定を書き換える。
- 変更は **次のターンから** 使われ、また変えるまで続く。いま実行中のターンは今のモデルのまま終わる。
- 変えられるのは会話の中だけ。サブタスクの中では変えられない（エラーで返る）。

## 2. 今のモデルと選べるモデルを確かめる

`get_system_info` が次を返す:

| 項目 | 意味 |
|---|---|
| `current_turn_model` | このターンで動いているモデル |
| `conversation_model` | この会話のモデル（次のターンから使われる） |
| `selectable_models` | 選べるモデルの一覧（`/model` と同じ一覧、`provider:model` 形式） |

「モデルは変わった？」と聞かれたら、`get_system_info` の `conversation_model` を見て答える。

## 3. 変え方

```
select_llm(
    model_alias: "chatgpt:gpt-6.2-sol",
    reason: "複雑な分析が続くため、より推論の強いモデルにする",
    purpose: "複雑な推論"
)
```

- `model_alias` は `selectable_models` にある `provider:model`、または一覧の中で 1 つに決まるモデル名（例: `gpt-6.2-sol`）。
- 一覧にない名前・複数のプロバイダにある名前は、何も変えずにエラーと選べる一覧が返る。一覧から選び直す。
- 成功すると `conversation_model`（次のターンから使うモデル）と `current_turn_model`（このターンのモデル）が返る。

## 4. いつ変えるか

- 複雑な推論・重要な判断・長い分析が続くとき → より強いモデル
- 単純な応答が続き、速さやコストを優先したいとき → 軽いモデル
- オーナーに指定されたとき → 指定どおりに
- 迷ったら変えない

過去の使用感は `recall_model_experiences` で、気づいたことは `save_model_insight` で残せる。
