# e2e

DESIGN-WEBGATE §7.4a / §7.4a-r1 / §7.2c-r1 の必須層。jsdom/vitest の代替ではない。

実 `opencrab-server` + 実 UDS + 実 `web-gateway` + `vite` ビルド済み UI を **平文 HTTP・非 loopback ホスト名**（`http://qc-e2e.test:port`。Chromium `--host-resolver-rules` で 127.0.0.1 へ解決）で立てる。テスト冒頭で `window.isSecureContext === false` をアサートする。これが成立して初めて secure context 前提 API の混入検出が機械化される。

ヘッドレス Chromium で次を通す。LLM は mock。

1. 新しい会話ボタン→作成→入力→送信→pending スピナー→SSE 返話
2. 物理 ID（extgate-…）で開く→溢れる件数を積む→送信→最下部到達（誤差 1px 台）→上へスクロール中は追従しない

| ファイル | 役割 |
|---|---|
| `harness.mjs` | mock LLM・core・gateway・同一 origin の静的+逆プロキシを `qc-e2e.test` 向けに起動。溢れログ投入は `POST /__e2e/seed-logs` |
| `run.mjs` | ハーネス起動 → `playwright test` → 停止 |
| `web-conversation.spec.ts` | 上記 2 本のブラウザ操作 |

実行: `npm run test:e2e`（リポジトリルートで `opencrab-server` / `web-gateway` を debug ビルドできること）。
