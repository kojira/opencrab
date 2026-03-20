# CLAUDE.md - opencrab開発ガイドライン

## 調査・修正の手順（必須）

問題が発生したとき、以下の順番で対処する：

1. **仮説を立てる** — なぜその問題が起きているか
2. **エビデンスを得る** — コードを読んで仮説を検証する
3. **エビデンスが仮説を裏付けなければ** → 別の仮説を立てて2に戻る（二分探索で範囲を絞り込む）
4. **エビデンスと仮説がマッチしたら** → 修正する

「たぶんこれだろう」で直接コードを変えない。必ずエビデンスを取ってから。

## プロジェクト概要

opencrab: Rustで書かれた自律AIエージェントフレームワーク

- **かいろ** (`slm-kairo`): Spring生まれのヤドカリAIエージェント（2026-03-20）
- **hermit-shell**: Anthropic OAuth プロキシ（macOSキーチェーンから自動取得）

## 起動手順

```bash
# バックエンド + フロントエンド
./dev.sh start

# 再起動
./dev.sh restart

# ログ確認
tail -f .server.log
```

## 注意事項

- `config/default.toml` の `token = "${DISCORD_TOKEN}"` は環境変数から読む（Per-agentはDBから）
- エージェントはダッシュボード（`http://localhost:3000`）から登録する
- `agent_ids = ["crab"]` の "crab" はDBに存在するIDと一致させる


