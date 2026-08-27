# e2e

DESIGN-WEBGATE §7.4a の必須層。jsdom/vitest の代替ではない。

実 `opencrab-server` + 実 UDS + 実 `web-gateway` + `vite` ビルド済み UI を **平文 HTTP** で立て、ヘッドレス Chromium で「新しい会話ボタン→作成→入力→送信→pending 表示→SSE 返話表示」を 1 本通す。LLM は mock。平文で走ること自体が仕様（secure context 前提 API の混入検出）。

| ファイル | 役割 |
|---|---|
| `harness.mjs` | mock LLM・core・gateway・同一 origin の静的+逆プロキシを平文で起動 |
| `run.mjs` | ハーネス起動 → `playwright test` → 停止 |
| `web-conversation.spec.ts` | 上記 1 本のブラウザ操作 |

実行: `npm run test:e2e`（リポジトリルートで `opencrab-server` / `web-gateway` を debug ビルドできること）。
