# 設計書: システムプロンプトへの非同期動作説明の組み込み

**作成日:** 2026-03-23  
**バージョン:** v3（エージェント視点に書き直し、簡潔化）  
**ステータス:** 設計完了（実装待ち）  
**対象:** `crates/server/src/process.rs` の `build_agent_context()`

---

## 1. 問題の本質

### エージェントが困惑する場面

エージェントのLLMは「会話の続きが来た」と感じるだけで、内部で何が起きたかは分からない。
ツールを呼び出した後、会話履歴に `[subtask_completed: ...]` が突然現れる。

**エージェントの視点:**
> 「自分はツールを呼んで…なんか結果が来た。でも何をすべき？また同じことを言うの？黙るの？」

これが重複発言や不適切な応答の原因。

### 解決方針

「ツールを呼んだ後に再び呼ばれたとき、何をすべきか」をシステムプロンプトに書く。
`build_agent_context()` に組み込むことで全エージェントに自動適用。

---

## 2. プロンプトに入れる実際の文章

以下を `## Async Behavior` セクションとしてシステムプロンプトに追加する。

```
## Async Behavior

You work asynchronously. When you call a tool, the result arrives later — and you
are called again with the result in the conversation history.

When you see `[subtask_completed: ...]` in the conversation:
- It means a tool you called has finished, and it's your turn again
- Check what the result contains
- If there's more to do: continue with the next step
- If the task is done: summarize and reply to the user
- If no reply is needed: respond with NO_REPLY

Do NOT repeat what you already said in the previous turn.
Do NOT re-explain what you're about to do if you already said it.
Just act on the result.
```

### 日本語版（参考）

上記の英語版が推奨。エージェントが英語モデルベースのため、英語の方が確実に解釈される。
日本語エージェント向けに日本語が必要な場合は以下を使用:

```
## 非同期動作について

あなたは非同期で動作します。ツールを呼び出すと、その結果は後で届きます。
結果が来た時、あなたは会話の続きとして再び呼ばれます。

会話履歴に `[subtask_completed: ...]` が見えた時:
- これはあなたが呼んだツールが完了し、あなたの番が来たサインです
- 結果を確認してください
- まだやることがあれば: 次のステップへ進む
- タスクが完了していれば: ユーザーへ結果を報告する
- 返答不要なら: NO_REPLY と返す

前のターンで既に言ったことを繰り返さないでください。
既に「〇〇します」と伝えた内容を再度説明しないでください。
結果に対してアクションを取ってください。
```

---

## 3. 実装箇所

`crates/server/src/process.rs` の `build_agent_context()` 内、
`## Silent Reply` セクションの直後、`{skills_text}` の前に追加する。

### 変更前後のdiff

```diff
     let prompt = format!(
         "You are {agent_name} ({persona}).\n\
          \n\
          You are an autonomous agent participating in a discussion. \
          Respond thoughtfully to the conversation. \
          You can use tools to search your history, learn from experience, \
          create new skills, and manage your workspace.\n\
          \n\
          The conversation history uses the format \"[speaker]: message\" for context, \
          but you must NOT include your own name prefix in your response. \
          Just reply with the message content directly.\n\
          \n\
          あなたは複数のアクションを順番に計画・実行できます。\
          例えば「Xを調べてYを設定する」という指示に対して、\
          1. execute_shell で情報収集、\
          2. 結果を解析、\
          3. add_allowed_command でコマンド追加、\
          4. create_my_skill でスキル作成、\
          のように、複数のアクションを連続して呼び出してください。\n\
          \n\
          ## Silent Reply\n\
          返答不要な場合は NO_REPLY とだけテキストで返してください（他のテキストと混在させない）:\n\
          - グループチャットで自分に関係ない会話の場合\n\
          - 他のBotが話している場合（Bot同士のループを防ぐ）\n\
          - 既に話が完結している場合\n\
          \n\
+         ## Async Behavior\n\
+         \n\
+         You work asynchronously. When you call a tool, the result arrives later — and you\n\
+         are called again with the result in the conversation history.\n\
+         \n\
+         When you see `[subtask_completed: ...]` in the conversation:\n\
+         - It means a tool you called has finished, and it's your turn again\n\
+         - Check what the result contains\n\
+         - If there's more to do: continue with the next step\n\
+         - If the task is done: summarize and reply to the user\n\
+         - If no reply is needed: respond with NO_REPLY\n\
+         \n\
+         Do NOT repeat what you already said in the previous turn.\n\
+         Do NOT re-explain what you're about to do if you already said it.\n\
+         Just act on the result.\n\
+         \n\
          {skills_text}{character_section}{instructions_section}"
     );
```

### process.rs の実際のコード（変更対象）

現在のフォーマット文字列末尾（行73〜87付近）:

```rust
    let prompt = format!(
        "You are {agent_name} ({persona}).\n\
         ...
         ## Silent Reply\n\
         返答不要な場合は NO_REPLY とだけテキストで返してください（他のテキストと混在させない）:\n\
         - グループチャットで自分に関係ない会話の場合\n\
         - 他のBotが話している場合（Bot同士のループを防ぐ）\n\
         - 既に話が完結している場合\n\
         \n\
         {skills_text}{character_section}{instructions_section}"   // ← ここの前に追加
    );
```

---

## 4. 設計上の判断

### なぜ `build_agent_context()` に組み込むか

opencrabの非同期動作は特定エージェントの個性ではなく**プラットフォーム共通の動作仕様**。
個別エージェントのinstructionsに書くと:
- 新エージェント追加時に毎回設定が必要
- 仕様変更時に全エージェントのDBを更新する必要がある

`build_agent_context()` に書けば全エージェントに自動適用され、コード1箇所の変更で済む。

### なぜ英語で書くか

- opencrabが使うLLM（Claude, GPT-4等）は英語での指示を最も正確に解釈する
- 日本語エージェントでも英語プロンプトは理解できる
- `## Silent Reply` セクションが英語で書かれているので統一感もある

### `[subtask_completed: ...]` という表記について

現在のcodebaseで実際に使われているフォーマットを前提としている。
フォーマットが変わった場合はこのセクションも更新すること。

---

## 5. テスト方針

実装後、以下のシナリオで動作確認:

1. **重複発言の抑止**: ツール呼び出し後、前のターンと同じ内容を繰り返さないか
2. **タスク完了の認識**: `[subtask_completed: ...]` を見てユーザーへの報告ができるか
3. **NO_REPLY判定**: 結果が不要な場合にNO_REPLYを返せるか
4. **既存動作への影響**: 通常会話（非同期でない場合）に影響がないか

---

## 6. 関連ファイル

- `design-message-loop-v3.md` — Event-Drivenモデルの実装詳細
- `design-bot-loop-prevention.md` — かいろのループ事例と対策
- `design-agent-instructions.md` — instructionsフィールド追加の設計
