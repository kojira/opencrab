# OpenCrab をはじめる

この文書は、初めて OpenCrab にさわる人のための道案内です。

- **動かしてエージェントと話したい人** → 1〜4 を順に進めてください。
- **OpenCrab を開発する人** → 1〜4 のあと、5〜6 を読んでください。

ここに書いたコマンドは、すべて macOS（arm64）で実際に実行して確かめたものです。
実行できなかったものは、そう書いてあります。

## 1. OpenCrab とは

- **core**（このリポジトリ）が、エージェントの本体です。会話、記憶、道具（ツール）、DB（SQLite）、ダッシュボード用の REST API を持ちます。
- **gateway** は別のプロセスです。Discord、Nostr、Web、ターミナル、Bluesky などとの出入りを受け持ちます。
- gateway は core の **gate socket**（Unix ソケット 1 本）につなぎます。資格情報はありません。
- gateway は、自分の道具（返信、リアクションなど）を接続時に core へ宣言します。
- gateway のコードは別のリポジトリにあります。[kojira/opencrab-gateways](https://github.com/kojira/opencrab-gateways)（discord / nostr / web / cli）と [kojira/opencrab-bluesky-gateway](https://github.com/kojira/opencrab-bluesky-gateway)（Go）です。

```
 Discord   Nostr   Web   Terminal   Bluesky
    |        |      |       |          |
 discord-  nostr-  web-  opencrab-   bluesky
 gateway  gateway gateway cli-gateway gateway     <- 別プロセス（gateway の設定・鍵はここ）
    \        \      |       /          /
     +--------+-----+------+----------+
                    |  gate socket（Unix ソケット, protocol 3）
              +-----+------------------+
              |  core (opencrab-server) |  <- 会話・記憶・道具・LLM・SQLite
              +-----+------------------+
                    |  REST API :8080
              ダッシュボード（web/, :3000）
```

## 2. 必要なもの

| もの | 版 | どこで決まっているか |
|---|---|---|
| Rust | 1.98.0 | `.github/workflows/ci.yml`（`dtolnay/rust-toolchain@1.98.0`）。`rust-toolchain` ファイルは無い |
| Node.js | 22 | `.github/workflows/ci.yml`（`node-version: 22`）。`.nvmrc` や `engines` は無い |
| pnpm | 11.14.0 | `web/package.json` の `packageManager`。corepack が入れる |
| SQLite | 入れなくてよい | `rusqlite` の `bundled` で同梱（`Cargo.toml`） |
| LLM | どれか 1 つ | ChatGPT のログイン（`~/.codex/auth.json`）か API キー |

Rust 1.98.0 は次で入ります。

```sh
rustup toolchain install 1.98.0
```

Linux では、CI が `cmake pkg-config libopus-dev libssl-dev lld` を入れています（`ci.yml`）。
macOS では追加の導入なしでビルドできました。

## 3. core を動かす

### 3.1 取ってきてビルドする

```sh
git clone https://github.com/kojira/opencrab.git
cd opencrab
cargo +1.98.0 build -p opencrab-server
```

### 3.2 設定ファイルを作る

```sh
cp config/default.toml.example config/default.toml
```

**注意:** 今のテンプレートのままでは起動しません（7 章の「起動直後に context budget で止まる」）。
`config/default.toml` の `[llm]` から `# ===== REST API =====` の直前までを、使う LLM 1 つ分に置き換えます。

ChatGPT にログイン済み（`~/.codex/auth.json` がある）なら、次のとおりです。

```toml
[llm]
default_provider = "chatgpt"
default_model = "gpt-6-sol"

[llm.providers.chatgpt]
auth_file = "~/.codex/auth.json"
reasoning_effort = "low"
```

`default_model` には、新しい DB に予算（`context_window` と `max_output_tokens`）が最初から入っているモデルを選びます。
今入っているのは `chatgpt:gpt-6-sol`・`chatgpt:gpt-6-luna`・`codex:gpt-5.6` です（`crates/db/src/schema/`）。

API キーで使う場合（例: Anthropic）は、`.env` にキーを書きます。

```sh
cp .env.example .env
# .env の ANTHROPIC_API_KEY= にキーを書く
```

```toml
[llm.providers.anthropic]
api_key = "${ANTHROPIC_API_KEY}"
```

このときは、上の ChatGPT の設定で一度起動し、使うモデルの予算を登録してから `default_provider` と `default_model` を切り替えます（予算は DB に残ります）。
この手順は、偽のキーで「切り替えたあと起動できる」ところまで確かめました。本物のキーでの返事は試していません。

```sh
curl -s -X PUT localhost:8080/api/llm/model-pricing -H 'content-type: application/json' \
  -d '{"provider":"anthropic","model":"<model>","input_price_per_1m":0.0,"output_price_per_1m":0.0,"context_window":200000,"max_output_tokens":32000}'
```

ポート・DB・gate socket の場所も `config/default.toml` で決めます。この 3 つは再起動しないと変わりません。

```toml
[gateway.rest]
port = 8080

[database]
path = "data/opencrab.db"

[gate]
listen_socket = "/tmp/opencrab-extgate.sock"
```

DB や socket を別の場所に置くと、試した跡を消すのが楽です（例: `path = "/tmp/opencrab-try/opencrab.db"`）。
DB のフォルダは core が作ります。

### 3.3 起動する

`config/default.toml` は、作業フォルダからの相対パス `config/` で読まれます。リポジトリの直下で起動してください。

```sh
./target/debug/opencrab-server
```

`Server listening on 0.0.0.0:8080` が出れば動いています。別のターミナルで確かめます。

```sh
curl -s localhost:8080/health        # ok
curl -s localhost:8080/api/health    # {"status":"ok"}
curl -s localhost:8080/api/llm/model-choices
```

最後のコマンドは、選べるモデルの一覧です（`/model` と `select_llm` と同じ一覧）。

### 3.4 ダッシュボードを開く

```sh
cd web
corepack pnpm install --frozen-lockfile
./dev.sh
```

ブラウザで `http://localhost:3000/setup` を開きます。
ダッシュボードの `/api` は `localhost:8080` へ転送されます（`web/vite.config.ts` に書いてある固定値）。core を別のポートにしたときは、ダッシュボードは使えません。

ダッシュボードには認証がありません。信頼できるネットワークの中だけで開いてください。

### 3.5 エージェントを作る

`/setup` の 2 つ目の手順「Agent」で作れます。同じことを REST API でもできます。

```sh
curl -s -X POST localhost:8080/api/agents -H 'content-type: application/json' \
  -d '{"id":"crab","name":"Crab","persona_name":"Crab"}'
curl -s -X POST localhost:8080/api/agents/crab/skills/seed-standard -H 'content-type: application/json' -d '{}'
curl -s localhost:8080/api/setup/status   # "complete":true
```

2 つ目は、標準スキル（`skills/*.skill.md`）を入れるものです。

## 4. エージェントと話す（cli-gateway）

いちばん簡単なのは、ターミナル用の `opencrab-cli-gateway` です。鍵もトークンも要りません。

### 4.1 core と同じ版の gateway をビルドする

gateway は core のクレート（`opencrab-gate-client` など）を、git の rev 1 つに固定して使います。
固定している rev は `Cargo.toml` に書いてあり、`Cargo.lock` が正本です。
**core と gateway の rev はそろえてください。** そろっていないと、取り決めが合わずにつながらないことがあります。

```sh
git clone https://github.com/kojira/opencrab-gateways.git
cd opencrab-gateways
python3 scripts/core-rev.py      # gateway が固定している core の rev
git -C ../opencrab rev-parse HEAD  # 動かしている core の rev
```

2 つが違うときは、core 側を `git -C ../opencrab checkout <rev>` で合わせてからビルドし直します。

```sh
cargo +1.98.0 build -p opencrab-cli-gateway --locked
```

### 4.2 placement を書く

`cli-placement.json` を作ります。`core_socket` は core の `[gate] listen_socket` と同じ絶対パスにします。
`agent_id` は 3.5 で作ったエージェントの ID です。`instance_id` は小文字の UUID なら何でもかまいません。

```json
{
  "core_socket": "/tmp/opencrab-extgate.sock",
  "instances": [{
    "instance_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    "agent_id": "crab",
    "author_id": "local-operator"
  }]
}
```

core に前もって登録するものはありません。gateway が接続時に名乗ります。

### 4.3 話す

新しい会話を始めます（ターミナルで対話）。

```sh
./target/debug/opencrab-cli-gateway --placement cli-placement.json --agent crab --new "Terminal chat" --mode repl
```

`Connected: crab / extgate-…` と出たら、1 行書いて Enter で送ります。`:quit` で終わります。
表示された `extgate-…` を `--session` に渡すと、同じ会話の続きから話せます。

```sh
./target/debug/opencrab-cli-gateway --placement cli-placement.json --agent crab \
  --session extgate-<uuid> --mode repl
```

スクリプトから使うときは JSONL です。

```sh
printf '%s\n' '{"type":"message","id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","text":"こんにちは。一言で自己紹介して。"}' |
  ./target/debug/opencrab-cli-gateway --placement cli-placement.json --agent crab --new "Terminal chat" --mode jsonl
```

実際に返ってきた行（一部）:

```
{"type":"ready","agent_id":"crab","session_id":"extgate-…","state":"connected"}
{"type":"accepted","id":"bbbbbbbb-…","origin":"cli:bbbbbbbb-…","seq":1}
{"type":"message","delivery_id":"…","text":"こんにちは、Crabです。調べて考え、手際よくお手伝いします。","reply_origin":null}
{"type":"completed","target":"…"}
{"type":"closed","reason":"requested"}
```

cli-gateway の発言は、オーナー（`owner`）の発言として core に届きます。
ほかの使い方は `crates/cli-gateway/README.md`（opencrab-gateways）にあります。

### 4.4 ほかの gateway

Discord・Nostr・Web・Bluesky は、それぞれの bot トークンや鍵が要るので、ここでは試していません。
各リポジトリの README を見てください。

- Discord / Nostr / Web: [opencrab-gateways の README](https://github.com/kojira/opencrab-gateways)
- Bluesky: [opencrab-bluesky-gateway の README](https://github.com/kojira/opencrab-bluesky-gateway)

ダッシュボードの会話画面（`/api/web-conversations`）は web-gateway（`127.0.0.1:8787`）が受け持ちます。

## 5. どこに何があるか

### リポジトリ

| 場所 | 中身 |
|---|---|
| `crates/core` | エージェントの本体（人格、記憶、スキル、ワークスペース、ターン） |
| `crates/llm`, `crates/llm-types` | LLM の provider とルーティング |
| `crates/actions` | 組み込みの道具と実行、権限の表（`bridge/policy.rs`） |
| `crates/db` | SQLite（スキーマ、マイグレーション、クエリ） |
| `crates/server` | REST API、起動、設定の読み込みと再読み込み |
| `crates/extgate` | gate socket の core 側 |
| `crates/gate-client` | gate socket の gateway 側（Rust の gateway が使う） |
| `crates/cli` | DB 管理用の REPL（`cargo run -p opencrab-cli --bin opencrab-cli`） |
| `web/` | ダッシュボード（React + Vite） |
| `skills/` | 標準スキル |
| kojira/opencrab-gateways | discord / nostr / web / cli の gateway、E2E、適合試験 |
| kojira/opencrab-bluesky-gateway | Bluesky の gateway（Go） |

### 読む文書

- [docs/design/external-gate.md](design/external-gate.md) — gate socket の取り決め（正本）
- [docs/DESIGN.md](DESIGN.md) — 全体の設計
- [docs/api.md](api.md) — REST API の一覧
- [docs/config-precedence.md](config-precedence.md), [docs/config-driven-tools.md](config-driven-tools.md) — 設定
- [docs/design-plugin-architecture.md](design-plugin-architecture.md) — gateway を足す前に読む
- `config/default.toml.example` — 設定の各項目の説明

### gateway が道具を宣言するしくみ

gateway は接続の最初の `hello` で、道具の一覧（`operations`）を送ります。
core はその道具を、その gateway の会話のターンでだけエージェントに見せます。
エージェントが呼ぶと、core が gate socket で gateway に `invoke` を送ります。

道具の JSON Schema には、core 向けの印が 2 つあります。

- `"x-opencrab-short-ref": true` — エージェントが書いた短い参照（`e7` など）を、core が本当の ID に置き換えます。
- `"x-opencrab-workspace-file": true` — エージェントのワークスペースのファイルを、core が outbox にコピーして渡します。

くわしくは external-gate.md の 6 章です。

### モデルの選び方

- 会話（セッション）ごとにモデルを持ちます。ターンは始まったときのモデルで最後まで動きます。
- オーナーの `/model`（gateway の `command` の `set_session_model`）か、エージェントの `select_llm` で変えます。変更は**次のターンから**です。
- 選べる名前は `GET /api/llm/model-choices` の一覧だけです。一覧に無い名前は断られ、一覧が返ります。
- エージェントの `get_system_info` は、今のターンのモデル・次のターンのモデル・選べる一覧を返します。

## 6. 開発の流れ

1. `main` からブランチを切ります。
2. 変えます。コードを変えたら、関係する文書（README、docs/）も同じ PR で直します。文書はコードに合わせます。
3. push する前に、手元で確かめます（下）。
4. PR を出します。CI が通ってからマージします。

### CI が見ること（`.github/workflows/ci.yml`）

- Rust: Rust ファイル 800 行上限（`scripts/check-file-size.sh`）、`cargo fmt`、`cargo clippy -D warnings`、`cargo test --workspace --all-features`、非公開の識別子の検査（`scripts/check-no-private-identifiers.sh`）、依存の境界（`scripts/check-deps.sh`）
- Web: `pnpm install --frozen-lockfile`、`pnpm run build`（型検査込み）、`pnpm run test`

### 手元で確かめる

```sh
cargo +1.98.0 fmt --all -- --check
bash scripts/check-file-size.sh
git add -A && bash scripts/check-no-private-identifiers.sh   # 追跡しているファイルだけを見るので先に git add
bash scripts/check-deps.sh
cargo +1.98.0 test -p opencrab-db                               # 1 つのクレートだけ試す
```

全部のテストは `cargo +1.98.0 test --workspace --all-features` です（時間がかかります）。
公開リポジトリなので、実在の ID・鍵・ホスト名・個人のパスを書かないでください。テストの値は `example.com` のような予約の名前か、明らかに作り物とわかる値にします。

### gateway の core の固定を上げる

core を変えて gateway 側でも使うときは、opencrab-gateways で次をします（README の「Core version」）。

1. `Cargo.toml` の `rev = "…"` を、マージ済みの core の commit に書き換える。
2. `cargo update -p opencrab-gate-client` を実行する。
3. `python3 scripts/core-rev.py` が新しい rev を出すことを確かめる。
4. PR を出して CI を通す（CI はその rev の core を起動して試します）。

## 7. 困ったとき

### 起動直後に `context budget fail-loud at startup` で止まる

既定モデルの予算（`context_window`）が DB に無いと、core は起動しません。
今の `config/default.toml.example` は既定が `openai:claude-sonnet-4-6` で、新しい DB には予算が入っていないので、そのままでは必ずここで止まります。
3.2 のとおり、予算のあるモデルを既定にしてください。API キーの provider は、3.2 の `PUT /api/llm/model-pricing` で先に登録します。

### `fallback.chain references undefined provider` / `alias '…' targets undefined provider`

`[llm.fallback] chain` と `[llm.aliases]` が、無い provider を指しています。
ダッシュボードで provider を無効にしたときにも起きます（無効にした provider が chain に残る）。
このとき、起動もできなくなります。使わない chain と alias は消してください。

### `GET /api/llm/model-choices` が `model_catalog_unavailable`

登録された provider のどれか 1 つでもモデル一覧を出せないと、一覧全体が出ません。
テンプレートのままだと、動いていない openai（ローカルの代理）・ollama・codex で起きました。使う provider だけを残してください。

### `Address already in use (os error 48)`

ポートが使われています。`lsof -iTCP:8080 -sTCP:LISTEN -nP` で何が使っているか見て、`[gateway.rest] port` を変えるか、そのプロセスを止めます。
**注意:** 同じ設定の core を 2 つ目に起動すると、ポートで失敗する前に gate socket を作り直してしまいます。動いている 1 つ目の core に gateway がつながらなくなるので、1 つ目も再起動してください。

### gateway が `uds connect/hello failed` を繰り返し、`instance_unavailable` で終わる

- core が動いているか、`core_socket` が `[gate] listen_socket` と同じか確かめます。
- すぐ上の「2 つ目の core」も疑ってください。
- core と gateway の rev はそろえてください（4.1）。違う rev の組み合わせで何が起きるかは、ここでは試していません。`hello` で断られたときのコード（`protocol_unsupported` など）は external-gate.md の 7 章にあります。

### 返事が来ず `{"type":"turn_failed",…}` になる

LLM の呼び出しが失敗しています。core のログに理由が出ます。
例: `Failed to read auth file: …`（ChatGPT にログインしていない、`auth_file` が違う）。API キーなら `.env` を確かめます。
`POST /api/llm/providers/<name>/test` で provider が使えるか見られます。

### モデルを変えようとしたら断られた

一覧（`GET /api/llm/model-choices`）に無い名前です。`provider:model` の形で、一覧にある名前を使ってください。
エージェントのモデルを REST で変えるとき（`PATCH /api/agents/{id}`）は、そのモデルの予算が登録されていないと断られます。

### `./dev.sh start` が `none of the selected packages contains this feature: discord` で失敗する

ルートの `dev.sh` は、もう無い `discord` feature でビルドします。3.3 と 3.4 の手順で起動してください（`web/dev.sh` は使えます）。

### `cargo run -p opencrab-cli` が `could not determine which binary to run`

`--bin opencrab-cli` を付けます。
