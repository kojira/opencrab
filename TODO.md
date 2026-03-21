# opencrab TODO

## 完了済み（2026-03-21）
全6タスク完了。エビデンス: `data/opencrab-evidence-2026-03-21.md`

## 未解決課題（残り）

1. ~~**`personality_json` / `custom_traits_json` 整理**~~ ✅ 完了（2026-03-21）

2. ~~**ハートビートプロンプトのバグ修正**~~ ✅ 完了（2026-03-21）

3. ~~**ハートビートSPEAK自発投稿の動作確認**~~ ✅ 完了（2026-03-21、Discord E2E確認済み）

4. ~~**チャンネルDELETE API未実装**~~ ✅ 完了（2026-03-21）

5. ~~**テスト設計の改善**~~ ✅ 完了（2026-03-21、E2Eテスト全タスクに組み込み）

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
   - cargo testは通過済み、DBにインデックス78件あり（自動構築？）
   - Claude設定後にDiscord経由で `rebuild_memory_index` をかいろが実行したエビデンスなし
   - kairo-testで「メモリインデックスを再構築して」と指示して確認が必要

8. ~~**一般ユーザーからのメッセージへの反応確認**~~ ✅ 完了（2026-03-21、kojiraさんが「自己紹介して」でかいろ返答確認済み）

9. ~~**シナリオ3後の実動作確認**~~ ✅ 完了（2026-03-21）

10. ~~**ツール許可リストのダッシュボード管理機能**~~ ✅ 完了（2026-03-21）
    - gateway actions: add/list/remove_allowed_command 実装済み
    - ダッシュボードUI: AgentAllowedCommands.tsx 追加済み
    - E2E確認: kojiraが「curlを追加して」→実行→「削除して」フル動作確認済み

11. ~~**`dashboard/` (Leptosクレート) の整理**~~ ✅ 完了（削除済み）
    - `dashboard/` ディレクトリは既に削除済み（`67820ca`）

## 新規課題（2026-03-21追加）

12. **タスクチャンネルDB同期 + 自動削除**
    - タスクチャンネルの内容をDBに同期（discord_logまたは専用テーブル）
    - 同期完了後にDiscordチャンネルを削除
    - ハートビートまたはタスク完了時に自動実行
    - 背景: Discordサーバーチャンネル500上限に達した（2026-03-21）

13. ~~**スキルシステムの改善**~~ ✅ 完了（2026-03-22、スキルシステムv2）
    - executable タイプ廃止、`guidance` + `execute_shell` 動的実行に統一
    - `skill_type`/`code` カラム削除（`c89ac29`）

14. ~~**スキル呼び出しの自然言語対応（プロンプト改善）**~~ ✅ 完了（2026-03-22）
    - `build_context()` 更新により LLM が guidance を読んで動的に execute_shell で実行
    - かいろが「博多の天気は？」等で柔軟に動作確認済み

15. ~~**自律的複合タスク実行の設計（実装なし）**~~ ✅ Phase 1 完了（2026-03-22）
    - Bootstrap allowed_commands 実装済み (`64a783f`)
    - multi-step planning prompt 追加済み
    - Phase 2 (fetch_web action) は今後の課題

16. ~~**スキル同名upsert対応**~~ ✅ 完了（2026-03-22、`c92b25c`）
    - `create_skill`・`create_my_skill` 同名upsert対応済み

17. **Discord画像添付送信サポート**
    - 現状: `send_speech`はテキストのみ
    - 必要: ファイル（画像）をDiscordに送信するgateway action (`discord_send_file` など)
    - 用途: nano-banan-proで生成した画像をかいろがDiscordに投稿できるようにする
    - 関連: `serenity`のcreate_message + file_uploadまたはattachment機能

18. **OpenClaw→OpenCrabインポート機能**
    - ディレクトリ指定でOpenClawのワークスペースデータをインポート
    - SOUL.md → soul、IDENTITY.md → identity、memory/*.md → curated memories変換
    - 設計中: `docs/design-openclaw-import.md`（サブエージェント作成中）

19. **もぐたろう防止アーキテクチャ（一次回答+バックグラウンド処理分離）**
    - 現状: かいろのメインループで長い処理をするとコンテキストが膨張
    - 求める設計: 受信→即時一次応答→長い処理はサブタスク自動エスカレート→完了通知
    - HTTPサーバーのリクエスト/レスポンスモデルに相当
    - 3人（のすたろう・らぼみ・kojira）でレビューしてから実装

20. **エージェントシークレット管理**
    - 現状: GEMINI_API_KEY等はホストの環境変数に依存
    - 目標: opencrab内でエージェントごとに暗号化シークレットを管理
    - 設計: ダッシュボードから登録・更新、スキルのguidanceで`$SECRET_NAME`参照
    - 優先度: 中（現状は環境変数で動くため急ぎではない）

21. **execute_shellのcwdをワークスペースに設定**
    - 現状: cwdがサーバー起動ディレクトリ（opencrabルート）になっている
    - 修正: `cmd.current_dir(&ctx.workspace.root)` でワークスペースをcwdに設定
    - 効果: スクリプトやファイルパスが相対パスで書けるようになり可搬性が上がる

22. **Discord添付画像のvision対応**
    - 現状: LLMレイヤーに`ImageUrl`/`supports_vision`実装あるが、Discord添付→LLMパイプラインが未接続
    - 修正: `message_loop.rs`でDiscordの`attachments`を取得し、画像URLをLLMの`ContentPart::ImageUrl`として渡す
    - 効果: かいろがDiscordで送られた画像を理解できるようになる

23. **ホワイトリスト外チャンネルでのセッションDB保存バグ修正**
    - 現状: whitelist外チャンネルのメッセージもDBにセッションログが蓄積される
    - 修正: `message_loop.rs`でホワイトリストチェックをDBへの保存処理より前に実施
    - 影響: 不要なセッション履歴が積み上がるのを防ぐ
