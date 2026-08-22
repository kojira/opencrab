# 設計書: tool_call履歴管理

## 概要

execute_shellを含む全ツール呼び出しは非同期で処理され、process_subtask_completedで会話を再構築する際にtool_use/tool_resultのペアが欠落しているためAnthropicのAPI制約に違反し、LLMがtextを重複生成する。

---

## 根本原因

- Anthropic Messages APIの制約: assistantメッセージに`tool_use`ブロックがある場合、後続のuserメッセージに対応する`tool_result`ブロックが必須
- opencrabでは全ツール呼び出しが非同期。tool実行完了後、process_subtask_completedで会話を再構築するが、この時点でtool_use（assistantメッセージ）とtool_result（toolメッセージ）がDBに保存されておらず欠落している
- AnthropicはAPI制約違反の状態でレスポンスを生成するため、LLMが前の発言（「調べてみる。」）を「中断された思考」として再生成してしまう

---

## 修正方針（P1）

**DBにtool_use/tool_resultを保存し、process_subtask_completed時に復元する**

変更箇所:
1. `skill_engine.rs` - tool call発行時にassistantメッセージ（content + tool_calls）をDBに保存
2. `skill_engine.rs` - tool実行完了時にtool_resultをDBに保存（tool_call_idで紐付け）
3. `skill_engine.rs` - process_subtask_completed時の会話再構築でDBからtool_use+tool_resultペアを復元してmessages配列に追加

---

## P1適用後のLLMコール履歴（天気ユースケース）

**【LLMコール #1】** ユーザー発言を受けて
```
[system]    "You are エージェントB..."
[user]      "ドイツの天気教えて"
```
→ LLMレスポンス:
```
content = "調べてみる。"
tool_calls = [{id:"t01", name:"execute_shell", input:{command:"curl", args:["wttr.in/Frankfurt?format=j1"]}}]
```

↓ on_first_response: Discord「調べてみる。」送信
↓ DBに保存: assistant{content="調べてみる。", tool_calls=[t01]}
↓ execute_shell実行（非同期）→ 完了
↓ DBに保存: tool{tool_call_id="t01", content="{\"temp_C\":\"5\",\"desc\":\"Sunny\"}"}

**【LLMコール #2】** process_subtask_completedで再構築
```
[system]    "You are エージェントB..."
[user]      "ドイツの天気教えて"
[assistant] content="調べてみる。"
            tool_calls=[{id:"t01", name:"execute_shell", input:{command:"curl", args:["wttr.in/Frankfurt?format=j1"]}}]
[tool]      tool_call_id="t01"
            content="{\"temp_C\":\"5\",\"desc\":\"Sunny\"}"
```
→ LLMレスポンス:
```
content = "取れた！フランクフルトの天気は5°C、晴れだよ！"
```
↓ Discord: 「取れた！フランクフルトの天気は5°C、晴れだよ！」（重複なし）

**ツールが2回呼ばれた場合のLLMコール #3:**
```
[system]    "You are エージェントB..."
[user]      "ドイツの天気教えて"
[assistant] content="調べてみる。"
            tool_calls=[{id:"t01", name:"execute_shell", ...}]
[tool]      tool_call_id="t01", content="..."
[assistant] content="もうちょっと詳しく調べるね。"
            tool_calls=[{id:"t02", name:"execute_shell", ...}]
[tool]      tool_call_id="t02", content="..."
```
→ LLMレスポンス: 最終回答

---

## TODO

- NO_REPLY時のsession_log記録（エージェントが自分の黙った箇所を履歴から辿れるようにする）
