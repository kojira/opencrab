# opencrab TODO

## 完了済み（2026-03-21）
全6タスク完了。エビデンス: `data/opencrab-evidence-2026-03-21.md`

## 未解決課題

1. ~~**`personality_json` / `custom_traits_json` 整理**~~
   - `custom_traits_json` → `personality` にリネーム
   - `personality_json` を削除・参照箇所を `personality` に変更
   - JSONではなくプレーンテキストとして扱う

2. **ハートビートプロンプトのバグ修正**
   - `build_heartbeat_prompt()` が `personality_json`（空`{}`）を参照している
   - 修正後は `personality`（実際のペルソナテキスト）を渡すこと

3. **ハートビートSPEAK自発投稿の動作確認（未完了）**
   - Discord E2Eで実際にかいろが自発投稿するか未確認
   - 課題2の修正後に再テスト

4. **チャンネルDELETE API未実装**
   - `channel-configs` の DELETE エンドポイントがない
   - 漫才チャンネルの `whitelisted=false` レコードが残存

5. **テスト設計の改善**
   - 「操作後に実際の動作確認」ステップを必ずシナリオに含める
   - Discord E2Eテストを基本とする（APIだけでなく実際の動作確認まで）

6. **ハートビートをエージェントのセッションとして処理する設計への刷新** ✅ v2完了（チャンネルごとセッション設計）
   - ~~現状：サーバー側で独立したLLM呼び出し、かいろは「今がハートビートの実行」と知らない。SPEAKしても会話履歴に残らない~~
   - ~~必要：ハートビートtickごとにエージェントのセッションを作成し、「ハートビートの時間です。今この瞬間何をしますか？」というシステムイベントとして投入する~~
   - ~~これによりかいろが自律的に判断・行動し、SPEAKした内容も会話履歴として蓄積される~~
   - **v1完了（2026-03-21）**: エージェントセッション統合、SkillEngine経由でchat_completion実行
   - **v2完了（2026-03-21）**: チャンネルごとのセッション設計に刷新
     - `discord_channel_config`からwhitelisted=trueのチャンネルを全取得
     - 各チャンネルにセッション `heartbeat-{agent_id}-{channel_id}` を作成
     - SPEAKはそのチャンネルIDに投稿
     - heartbeat_logにchannel_idを記録（result_json=channel_id=...）
     - E2Eテスト: kairo-test(1470698801395273861)への自発投稿確認済み

7. **ツリー再マージ・インデックス再構築のDiscord E2E確認**
   - cargo testは通過したがDiscord経由の動作確認がない
   - かいろに「メモリインデックスを再構築して」と指示して実際に動くか確認

8. **一般ユーザーからのメッセージへの反応確認**
   - 今日のテストはすべてのすたろうbot経由
   - kojiraさん等の実ユーザーがkairo-testで話しかけた時に反応するか未確認

9. **シナリオ3後の実動作確認**
   - かいろがwhitelisted=trueで登録した後、そのチャンネルで実際に反応するかのステップが抜けていた

10. **ツール許可リストのダッシュボード管理機能**
    - 現状: config.toml (`tools.shell.allowed_commands`) の手動変更のみ
    - オーナー（trusted_user）からの指示でかいろ自身が許可コマンドを追加できる機能が必要
    - ダッシュボード（web/）からもGUIで管理できることが望ましい
    - 「curlを許可して」→かいろが自分のconfig/DBを更新して実行できるようになる

11. **`dashboard/` (Leptosクレート) の整理**
    - Task 6 v4でLeptosの`dashboard/src/`を変更したが、実際に使われているのは`web/`（React）
    - Leptosクレートが不要なら削除、必要なら用途を整理する
