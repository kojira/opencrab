# samples/node

Node.js 列の独立実装。Rust/core の code・型・parser・生成 DTO を import しない。

- 実行系の exact version は `.node-version`（repository-owned pin）
- production runtime dependency は 0。`node:net` / `node:http` / `node:crypto` と言語組込みだけ
- transpile しない。`node_modules` を置かない
- merge CI は `scripts/check-samples-node.sh` で dependency / Rust import / Bearer / 旧 route を static audit する

被検体は `web-gateway/`。
