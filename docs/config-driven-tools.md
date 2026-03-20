# Config駆動ツールプラグイン設計

> **ステータス:** 設計フェーズ  
> **作成日:** 2026-03-21  
> **対象バージョン:** opencrab（現状調査ベース）

---

## 1. 概要

### 設計方針

opencrab の `ActionDispatcher` は現在、コード内でアクションをハードコードして登録している（`dispatcher.rs` の `ActionDispatcher::new()`）。新しいアクションを追加するには Rust コードの変更とリビルドが必要。

**Config駆動ツール**は、`config/default.toml`（または `config/agents/{agent_id}.toml`）に設定を書くだけでエージェントが新しいツール（アクション）を使えるようにする仕組み。リビルド不要で、エージェントごとに異なるツールセットを持てる。

### 設計の核心

- `[tools]` セクションを TOML に追加
- 起動時に `ToolsConfig` を読み込み、`ActionDispatcher` に動的にアクションを登録
- 最初の実装は `execute_shell` アクション（ホワイトリスト方式）
- SLM-kairo の `ToolsPlugin` が `load(config: toml::Value)` でコンフィグを受け取るパターンを参考にする

### KairoとOpenCrabの設計の違い

| 観点 | SLM-kairo | opencrab |
|------|-----------|----------|
| プラグイン単位 | `KairoPlugin` トレイト + `on_message` / `pre_inference` フック | `Action` トレイト（同期的な1アクション1関数） |
| 登録方法 | `PluginRegistry` にプラグインをロード | `ActionDispatcher::register()` でアクションを登録 |
| Config受け取り | `load(config: toml::Value)` | 現状なし → 今回追加 |

opencrab は「1ツール1アクション」モデルのため、Config駆動では「Config値から `Arc<dyn Action>` を動的生成して `register()` する」方式が最も自然。

---

## 2. 新規ファイル一覧

### 追加するファイル

| ファイルパス | 役割 |
|------------|------|
| `crates/actions/src/tools/mod.rs` | ツールモジュールのエントリポイント。`ToolsConfig` 構造体と `register_from_config()` 関数を公開 |
| `crates/actions/src/tools/config.rs` | `ToolsConfig`・`ShellToolConfig`・`HttpToolConfig` 等の設定構造体定義（serde + Deserialize） |
| `crates/actions/src/tools/shell.rs` | `ShellToolAction` 実装。`Action` トレイトを実装し、TOML の `allowed_commands` に従ってコマンドを実行 |
| `crates/core/src/config.rs`（拡張） | `AppConfig` に `tools: ToolsConfig` フィールドを追加（または新規作成） |
| `docs/config-driven-tools.md` | 本ドキュメント |

### 変更するファイル

| ファイルパス | 変更内容 |
|------------|---------|
| `crates/actions/src/lib.rs` | `pub mod tools;` を追加、`ToolsConfig` と `register_tools_from_config()` を再エクスポート |
| `crates/actions/src/dispatcher.rs` | `ActionDispatcher::register_tools()` メソッドを追加（または `register_from_config()` をモジュール側に置く） |
| `crates/server/src/process.rs` | `run_agent_response()` 内で `ToolsConfig` を読み込んで `dispatcher` にツールを登録するステップを追加 |
| `crates/server/src/state.rs`（またはAppState定義箇所） | `AppState` に `tools_config: ToolsConfig` フィールドを追加 |
| `config/default.toml` | `[tools]` セクションを追加 |

---

## 3. Config設計

### `config/default.toml` への追加

```toml
# ===== ツール設定 =====

[tools]
# trueの場合、ツールセクション全体が有効
enabled = true

# シェルコマンド実行ツール
[tools.shell]
enabled = true
# 実行を許可するコマンド名のホワイトリスト（フルパス指定も可）
allowed_commands = ["curl", "echo", "date", "jq", "cat", "ls"]
# コマンドのタイムアウト（秒）
timeout_secs = 30
# 作業ディレクトリ（省略時はエージェントのワークスペース）
working_dir = ""
# 環境変数の引き継ぎ（falseで最小限の環境のみ）
inherit_env = false
# 許可する環境変数名のリスト（inherit_env=falseのときに個別指定）
allowed_env_vars = ["PATH", "HOME", "LANG"]
# stdoutの最大バイト数（超過時は切り詰め）
max_output_bytes = 65536

# HTTP リクエストツール（将来拡張用、現状はスケルトン）
[tools.http]
enabled = false
allowed_hosts = []
timeout_secs = 10
max_response_bytes = 65536
```

### エージェント固有設定 `config/agents/{agent_id}.toml`

エージェントごとに `[tools.shell]` をオーバーライド可能にする。マージ戦略は「エージェント設定がデフォルト設定を上書き」。

```toml
# エージェント "researcher" は curl と jq のみ許可
[tools.shell]
enabled = true
allowed_commands = ["curl", "jq"]
timeout_secs = 60
```

### 設定構造体（概念レベル）

`ToolsConfig` はトップレベル構造体で以下のフィールドを持つ：

- `enabled: bool` — ツール全体の有効/無効
- `shell: Option<ShellToolConfig>` — シェルツール設定

`ShellToolConfig` のフィールド：

- `enabled: bool`
- `allowed_commands: Vec<String>` — ホワイトリスト
- `timeout_secs: u64`
- `working_dir: Option<String>`
- `inherit_env: bool`
- `allowed_env_vars: Vec<String>`
- `max_output_bytes: usize`

---

## 4. `execute_shell` アクションの設計

### アクション名

`execute_shell`

### 概要

エージェントが指定したシェルコマンドをサブプロセスとして実行し、その標準出力・終了コードを返すアクション。`ShellToolConfig.allowed_commands` のホワイトリストに含まれるコマンド名のみ実行可能。

### 引数（`parameters()` で公開するJSONスキーマ）

| フィールド | 型 | 必須 | 説明 |
|-----------|-----|------|------|
| `command` | `string` | ✅ | 実行するコマンド名（例: `"curl"`）。ホワイトリストで検証される |
| `args` | `array[string]` | ❌ | コマンド引数のリスト（例: `["-s", "https://example.com"]`） |
| `stdin` | `string` | ❌ | 標準入力に渡す文字列 |

### 戻り値（`ActionResult.data`）

成功時の `data` フィールドは以下の構造のJSON：

| フィールド | 型 | 説明 |
|-----------|-----|------|
| `stdout` | `string` | 標準出力（`max_output_bytes` で切り詰め） |
| `stderr` | `string` | 標準エラー出力（同上） |
| `exit_code` | `number` | プロセスの終了コード |
| `truncated` | `bool` | 出力が切り詰められた場合 `true` |

失敗時（コマンドがホワイトリスト外、タイムアウト、実行エラー）は `ActionResult::error(...)` を返す。

### セキュリティ考慮点

#### 必須の制約

1. **コマンド名ホワイトリスト（最重要）**  
   `args[0]`（コマンド名）を `allowed_commands` と照合する。完全一致のみ許可。シェル展開（`&&`、`|`、`;`、`$()`、バックティック等）を含む引数は拒否する。

2. **シェル経由の実行禁止**  
   `sh -c "..."` 形式ではなく、コマンド名と引数配列を直接 `Command::new(cmd).args(args)` で渡す。シェルのメタ文字が引数に混入しても展開されない。

3. **タイムアウト強制**  
   `timeout_secs` で指定した時間を超えたプロセスは強制終了。デフォルト30秒。

4. **環境変数の制限**  
   `inherit_env = false`（デフォルト）の場合、`allowed_env_vars` で明示したもののみ引き継ぐ。秘密情報（APIキー等）が子プロセスに漏れない。

5. **作業ディレクトリの制限**  
   `working_dir` を省略した場合、エージェントのワークスペースパス（`ActionContext.workspace` のルート）を使用。ワークスペース外へのパストラバーサルをガード。

6. **出力サイズ制限**  
   `max_output_bytes` を超える出力は切り詰めて `truncated: true` を返す。巨大出力によるメモリ枯渇を防ぐ。

#### 運用上の注意

- `curl` を許可する場合、任意のURLへのリクエストが可能になる。ネットワーク分離が別途必要。
- `allowed_commands` を `[]` にすると `execute_shell` はすべての実行を拒否する（安全なデフォルト）。
- エージェントが `allowed_commands` リスト自体を変更できないよう、`ActionContext` から Config はリードオンリーで参照する。

---

## 5. ActionDispatcherへの組み込み方

### 現状の登録フロー

```
ActionDispatcher::new()
  └─ register(Arc::new(SendSpeechAction))
  └─ register(Arc::new(WsReadAction))
  └─ ... (全てハードコード)
```

`dispatcher.rs` の `new()` に全アクションが静的に登録されており、Configを参照するポイントがない。

### 新しい登録フロー

```
AppState初期化
  └─ ToolsConfig を config から読み込み AppState に保持

run_agent_response() (process.rs)
  └─ ActionDispatcher::new()         ← 既存の静的アクションを登録
  └─ register_tools_from_config(&tools_config, &dispatcher)  ← 新規
       └─ tools_config.shell.enabled == true の場合
            └─ dispatcher.register(Arc::new(ShellToolAction::new(shell_config.clone())))
```

### `register_tools_from_config()` の役割（`tools/mod.rs`）

- 引数: `&ToolsConfig`, `&mut ActionDispatcher`
- `tools.enabled == false` なら即リターン
- `tools.shell.enabled == true` なら `ShellToolAction::new(config)` を生成して `register()`
- 将来追加するツール（`HttpToolAction` 等）も同様にここで登録

### `ShellToolAction` の設計

- 構造体: `ShellToolAction { config: ShellToolConfig }`
- `Action::name()` → `"execute_shell"`
- `Action::description()` → 許可コマンドリストを含む説明文（例: `"Execute shell commands. Allowed: curl, echo, date"`）
- `Action::parameters()` → Section 4 の引数スキーマをJSONで返す
- `Action::execute()` → セキュリティチェック → サブプロセス起動 → 結果返却

### `AppState` への `ToolsConfig` 追加

`crates/server/src/state.rs`（もしくは AppState 定義ファイル）の `AppState` 構造体に `tools_config: ToolsConfig` フィールドを追加し、サーバー起動時にTOMLから読み込んでセット。

`run_agent_response()` のシグネチャを変更するか、`AppState` から参照する形にする。

### エージェント固有Configのマージタイミング

エージェント固有のツール設定（`config/agents/{agent_id}.toml`）がある場合、`run_agent_response()` の先頭でデフォルト設定とマージし、マージ済み `ToolsConfig` を `register_tools_from_config()` に渡す。

---

## 6. 将来拡張

### 6.1 curl専用アクション（`fetch_url`）

`execute_shell` を汎用シェルラッパーとするのとは別に、`curl` の機能に特化した型安全なアクションとして設計できる。

**利点:**
- URLバリデーション、ホワイトリスト制御が構造的に行える
- HTTPヘッダー、ボディ、メソッドを個別フィールドで受け取れる
- レスポンスのパースとエラーハンドリングをアクション内で完結できる

**Config例:**
```toml
[tools.fetch_url]
enabled = true
allowed_hosts = ["api.github.com", "httpbin.org"]
allowed_schemes = ["https"]
timeout_secs = 15
max_response_bytes = 1048576
follow_redirects = true
max_redirects = 3
```

### 6.2 HTTPリクエストアクション（`http_request`）

`fetch_url` のより汎用版。メソッド（GET/POST/PUT/DELETE）、ヘッダー、ボディを指定できる。

**Config例:**
```toml
[tools.http]
enabled = true
allowed_hosts = ["api.internal.example.com"]
allowed_methods = ["GET", "POST"]
timeout_secs = 10
max_response_bytes = 65536
default_headers = { "Accept" = "application/json" }
```

### 6.3 Pythonスクリプト実行（`run_python`）

`execute_shell` のPython特化版。`python3 -c` 相当の使い捨てスクリプトを安全に実行。

**Config例:**
```toml
[tools.python]
enabled = false
python_path = "/usr/bin/python3"
timeout_secs = 30
allowed_imports = ["json", "math", "datetime", "re"]  # importable modules
max_output_bytes = 65536
```

### 6.4 Wasmサンドボックス（将来）

長期的には、Wasmランタイム（Wasmtime等）上でツールを動かすことで、ファイルシステム・ネットワークアクセスをランタイムレベルでサンドボックス化できる。Configで `wasm_module_path` を指定するだけでカスタムツールを追加できる形にする。

### 6.5 プラグイン動的ロード（kairo方式との統合）

kairo の `KairoPlugin` トレイトパターンを参考に、opencrab でも `ToolPlugin` トレイトを導入し、`.so`/`.dylib` ファイルを `dlopen` で動的ロードするDLプラグイン方式も検討できる。ただし安全性・デバッグ難易度のトレードオフがある。現状はConfig駆動の静的登録方式を優先する。

### 拡張の優先順位

| 優先度 | アクション | 理由 |
|--------|-----------|------|
| 🔴 高 | `execute_shell` | 汎用性が高く、多くのユースケースをカバー |
| 🟡 中 | `fetch_url` | APIコールが一般的なエージェントユースケース |
| 🟡 中 | `http_request` | Webhook送信等の書き込み系ユースケース |
| 🟢 低 | `run_python` | データ処理・変換など |
| 🟢 低 | Wasmサンドボックス | セキュリティ強化が必要な本番環境向け |

---

## 付録: 実装チェックリスト

実装時の参照用（詳細コードは別途実装ドキュメントで管理）。

- [ ] `ToolsConfig` / `ShellToolConfig` 構造体を定義し `serde::Deserialize` を実装
- [ ] `AppConfig` に `tools: ToolsConfig` フィールドを追加
- [ ] `default.toml` に `[tools.shell]` セクションを追加
- [ ] `ShellToolAction` を `Action` トレイトで実装
- [ ] `register_tools_from_config()` 関数を `tools/mod.rs` に実装
- [ ] `dispatcher.rs` に `register_from_config()` メソッドを追加（または `process.rs` で呼び出し）
- [ ] `process.rs` の `run_agent_response()` で `ToolsConfig` を読んでツール登録
- [ ] `AppState` に `tools_config: ToolsConfig` を追加
- [ ] エージェント固有Config（`config/agents/{agent_id}.toml`）のマージロジック
- [ ] `execute_shell` のセキュリティテスト（ホワイトリスト外コマンド拒否、シェルメタ文字拒否、タイムアウト）

---

## 7. ホットリロード

### 概要

サーバーを再起動せずに `config/default.toml`（またはエージェント固有Config）の変更を検知し、`ActionDispatcher` のツールセットをライブで更新する仕組み。

### Configファイル変更の検知方法

OSのファイルシステムイベントAPIを抽象化する Rust クレート **`notify`**（v6系）を使用する。

| OS | 内部実装 | `notify` での対応 |
|----|----------|-------------------|
| macOS | `kqueue` / FSEvents | `notify::recommended_watcher()` が自動選択 |
| Linux | `inotify` | `notify::recommended_watcher()` が自動選択 |
| Windows | ReadDirectoryChangesW | `notify::recommended_watcher()` が自動選択 |

`notify::recommended_watcher()` はプラットフォームを問わず最適な実装を選択するため、クレートを使う限りOS差異を意識する必要はない。監視対象は `config/` ディレクトリ全体（サブディレクトリ含む）を再帰的にウォッチする。

### ホットリロードのフロー

```
[FSイベント受信]
       ↓
1. Configファイル変更を検知（notify::Event::Modify）
       ↓
2. 新しいConfigをパース（toml::from_str() → 構文チェック）
       ↓
3. バリデーション実行（Section 8 参照）
       ↓
   ┌── 成功 ──────────────────────────────────────────────────┐
   │   4a. Arc<RwLock<ActionDispatcher>> の write ロックを取得  │
   │   4b. 新しい ToolsConfig から ActionDispatcher を再構築    │
   │   4c. write ロックを解放 → 原子的に入れ替え完了            │
   └──────────────────────────────────────────────────────────┘
       ↓（失敗の場合）
   5. 前の Config のまま維持（ロールバック）
   6. エラー内容を tracing::error! でログ出力
```

### 実装上の考慮点

#### ActionDispatcher の共有方法

現在 `process.rs` では `run_agent_response()` のたびに `ActionDispatcher::new()` を生成している。ホットリロードを実現するには、`ActionDispatcher` を `Arc<RwLock<ActionDispatcher>>` でラップして `AppState` に持たせ、各リクエストで共有する方式に変更する。

```
AppState {
    dispatcher: Arc<RwLock<ActionDispatcher>>,  // 新規追加
    tools_config: Arc<RwLock<ToolsConfig>>,     // 新規追加
    ...
}
```

#### ウォッチャーの起動タイミング

サーバー起動時（`main.rs` の `AppState` 初期化後）に `notify` ウォッチャーを別スレッドまたは `tokio::spawn` で起動し、`Arc<RwLock<ActionDispatcher>>` と `Arc<RwLock<ToolsConfig>>` のクローンを渡す。

#### デバウンス処理

エディタがファイルを保存すると複数のイベントが短期間に発生することがある。`notify::Debouncer`（`notify-debouncer-mini` クレート）を使って 200〜500ms のデバウンスを挟み、不要な再ロードを防ぐ。

#### リロード対象の限定

- `config/default.toml` の変更 → グローバルな `ToolsConfig` を更新
- `config/agents/{agent_id}.toml` の変更 → 該当エージェントのキャッシュ済み設定を無効化（次回リクエスト時に再マージ）

#### リロード失敗時の挙動

- 前の `ActionDispatcher` は `RwLock` 内に保持されているため、write ロックを取得せずに返すだけでロールバックが実現できる。
- エラーログには「どのファイルが」「どの行で」「どんな構文エラーが」発生したかを含める（`toml::de::Error` のメッセージを活用）。

---

## 8. Configバリデーション

### 概要

Configのパース成功後、`ActionDispatcher` への反映前に実施するバリデーションレイヤー。**構文チェック → セマンティクスチェック** の順で実行し、いずれかで失敗したら即座にロールバックする。

### 8.1 構文チェック

#### TOML構文チェック

- `toml::from_str::<ToolsConfig>(raw_toml)` が `Err` を返した場合 → 即失敗
- エラーメッセージに含まれる行番号・列番号をログに出力する

#### 必須フィールドの存在と型チェック

`serde(deny_unknown_fields)` と `#[serde(default)]` を組み合わせることで、未知フィールドをエラー、省略フィールドをデフォルト値として扱う。型の不一致は `toml::from_str` がエラーを返すため自動的に検出される。

#### 数値レンジチェック

| フィールド | 条件 | エラーメッセージ例 |
|-----------|------|-------------------|
| `timeout_secs` | `> 0` | `"tools.shell.timeout_secs must be > 0, got 0"` |
| `max_output_bytes` | `> 0` | `"tools.shell.max_output_bytes must be > 0, got 0"` |

これらは `serde` のデシリアライズ後に `validate()` メソッド（`ShellToolConfig` に実装）で確認する。

### 8.2 セマンティクスチェック

#### `allowed_commands` の存在確認

`allowed_commands` の各エントリについて、`which` コマンド相当の確認を行う。Rust での実装は以下のいずれか：

- `std::process::Command::new("which").arg(cmd)` を実行して終了コードを確認
- `which` クレート（`which::which(cmd)` → `Err` なら存在しない）を使用

フルパス（`/usr/bin/curl`）が指定された場合は `Path::new(cmd).exists()` で直接確認する。

存在しないコマンドが含まれている場合の挙動：
- **エラー扱い（厳格モード）**: バリデーション失敗としてロールバック（デフォルト）
- **警告扱い（寛容モード）**: ログに警告を出して続行（`validate_mode = "lenient"` オプションで制御可能にする）

#### `working_dir` のパストラバーサルチェック

`working_dir` が指定された場合、以下の順序で検証する：

1. `..` コンポーネントを含んでいないか確認（`Path::components()` でイテレート）
2. パスを正規化（`canonicalize()`）して、エージェントのワークスペースルート以下に収まるか確認
3. 絶対パスの場合はワークスペースルートのプレフィックスチェック
4. 相対パスの場合はワークスペースルートとジョインしてから正規化

```
working_dir: "../../../etc"  → NG（.. を含む）
working_dir: "/etc"          → NG（ワークスペース外の絶対パス）
working_dir: "subdir"        → OK（ワークスペース内の相対パス）
working_dir: "a/b/c"         → OK（ワークスペース内のネスト）
```

#### `allowed_env_vars` の危険変数チェック

以下の環境変数名は、共有ライブラリの差し替えや動的リンカーの制御に使われるため、許可リストへの追加を **禁止** する：

| 変数名 | 危険な理由 |
|--------|-----------|
| `LD_PRELOAD` | Linux: 任意の共有ライブラリをロードさせてコードインジェクション可能 |
| `LD_LIBRARY_PATH` | Linux: ライブラリ検索パスを差し替えて偽ライブラリをロード可能 |
| `DYLD_INSERT_LIBRARIES` | macOS: `LD_PRELOAD` 相当 |
| `DYLD_LIBRARY_PATH` | macOS: `LD_LIBRARY_PATH` 相当 |
| `DYLD_FORCE_FLAT_NAMESPACE` | macOS: シンボル解決を変更してコード実行フローを乗っ取り可能 |
| `LD_AUDIT` | Linux: カスタム監査ライブラリのロード |

チェック実装例（Rust）：

```rust
const DANGEROUS_ENV_VARS: &[&str] = &[
    "LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES", "DYLD_LIBRARY_PATH", "DYLD_FORCE_FLAT_NAMESPACE",
];

for var in &config.allowed_env_vars {
    if DANGEROUS_ENV_VARS.contains(&var.as_str()) {
        bail!("Dangerous env var '{}' is not allowed in allowed_env_vars", var);
    }
}
```

### 8.3 バリデーション失敗時の挙動

| 状況 | 挙動 |
|------|------|
| 構文エラー | `tracing::error!` でファイルパス・行番号・エラーメッセージをログ出力し、即失敗 |
| 必須フィールド欠損 | フィールド名とファイルパスをログ出力し、即失敗 |
| `timeout_secs == 0` 等 | フィールド名と実際の値をログ出力し、即失敗 |
| コマンド不在 | 不在のコマンド名をログ出力し、失敗（または警告） |
| パストラバーサル | 問題のあるパスをログ出力し、即失敗 |
| 危険な環境変数 | 変数名をログ出力し、即失敗 |

**共通ルール:**
- 失敗理由は必ず具体的なメッセージ（値・ファイルパス・フィールド名を含む）でログに残す
- `tracing::error!` を使用し、ログレベルを ERROR とする
- バリデーション失敗時は前の Config を維持（`ActionDispatcher` は更新しない）
- エージェントは旧ツールセットで継続動作する（サービス断なし）

---

## 9. ワークスペースパスバグ修正

### 9.1 現状の問題

`config/default.toml` には以下の設定が存在する：

```toml
workspace_path = "data/agents/{agent_id}/workspace"
```

しかし実際にファイルが作成されるパスは `data/{agent_id}/hello.txt` のようになっており、`agents/` ディレクトリと `workspace/` ディレクトリが抜けた形になっている。

### 9.2 調査結果

#### `crates/core/src/workspace.rs`

`Workspace` 構造体は以下の2つのコンストラクタを持つ：

- **`Workspace::new(agent_id, base_path)`**: `base_path/workspaces/{agent_id}` を root として作成（`workspaces/` ディレクトリが挿入される）
- **`Workspace::from_root(root)`**: 指定されたパスをそのまま root として使用

現在 `process.rs` では `from_root()` を使用しているが、渡すパスが正しくない。

#### `crates/server/src/process.rs`（`run_agent_response()` 内、L103付近）

```rust
let ws_path = format!("{}/{}", state.workspace_base, agent_id);
std::fs::create_dir_all(&ws_path).ok();
let workspace = opencrab_core::workspace::Workspace::from_root(
    std::path::Path::new(&ws_path)
)?;
```

`state.workspace_base` の値は `"data"`（後述）のため、`ws_path = "data/{agent_id}"` となる。

#### `crates/server/src/main.rs`（L36付近）

```rust
workspace_base: "data".to_string(),
```

`AppState.workspace_base` が `"data"` にハードコードされており、`config/default.toml` の `workspace_path` 設定が全く読み込まれていない。

#### `crates/server/src/lib.rs`（`AppState` 定義）

```rust
pub struct AppState {
    pub workspace_base: String,  // "data" にハードコード
    ...
}
```

### 9.3 バグの根本原因

以下の3つの問題が重なっている：

| # | 問題 | 場所 |
|---|------|------|
| 1 | `workspace_path` の設定値が `AppState` に反映されていない | `main.rs` L36 |
| 2 | `{agent_id}` プレースホルダーが展開されていない（そもそも設定を使っていない） | `main.rs` L36 |
| 3 | `process.rs` のパス構築が `agents/` と `workspace/` を含まない | `process.rs` L103 |

### 9.4 修正方針

#### 修正対象ファイルと箇所

**`crates/server/src/main.rs`（優先度: 最高）**

`workspace_path` を `config` から読み込み `AppState` に渡す。ただし `{agent_id}` はエージェントごとに異なるため、`workspace_base` はテンプレート文字列のまま保持するか、`agents/` + `workspace/` の構造を暗黙知として `process.rs` 側で組み立てる。

推奨アプローチ（シンプル）：
- `workspace_base` をテンプレートとして `config` から読み込む（デフォルト値: `"data/agents/{agent_id}/workspace"`）
- `process.rs` 側で `{agent_id}` を実際のエージェントIDで置換する

```rust
// main.rs の変更イメージ
workspace_base: config.workspace_path.clone(),  // "data/agents/{agent_id}/workspace"
```

**`crates/server/src/process.rs`（優先度: 最高）**

`ws_path` の構築を修正し、`{agent_id}` プレースホルダーを展開する：

```rust
// 修正前
let ws_path = format!("{}/{}", state.workspace_base, agent_id);

// 修正後
let ws_path = state.workspace_base.replace("{agent_id}", agent_id);
```

`Workspace::from_root()` に渡すパスは `ws_path` のままでよい（テンプレート展開後のパスが正しく `data/agents/{agent_id}/workspace` になるため）。

**`crates/server/src/lib.rs`（優先度: 中）**

`AppState.workspace_base` の型・意味は変わらないため変更不要。ただし、ドキュメントコメントに「`{agent_id}` プレースホルダーを含むテンプレートパス」である旨を追記することを推奨。

#### `Workspace::new()` vs `Workspace::from_root()` の選択

- `Workspace::new(agent_id, base_path)` は内部で `base_path/workspaces/{agent_id}` を生成するため、`workspace_path` の構造と合致しない（`workspaces/` が余分に挿入される）
- **`Workspace::from_root(ws_path)` を引き続き使用し、`ws_path` を正しく構築するのが最もシンプルな修正**

#### 修正の確認方法

修正後、エージェントがファイルを作成した際に `data/agents/{agent_id}/workspace/` 以下にファイルが作成されることを確認する。既存のテストは `Workspace::new()` の `workspaces/` 付与挙動を検証しているため、`process.rs` のパス修正とは独立して動作する。

### 9.5 影響範囲

| ファイル | 変更 | 影響 |
|---------|------|------|
| `crates/server/src/main.rs` | `workspace_base` の初期化をconfigから読む | サーバー起動時のディレクトリ構造が変わる |
| `crates/server/src/process.rs` | `{agent_id}` プレースホルダー展開を追加 | 既存の `data/{agent_id}/` ディレクトリとの非互換（マイグレーション要検討） |

> ⚠️ **注意**: 既存のエージェントワークスペースが `data/{agent_id}/` に存在する場合、修正後は `data/agents/{agent_id}/workspace/` に変わるため、既存データのマイグレーションが必要になる可能性がある。本番適用前に確認すること。

---

## 10. コマンド権限レベル

### 概要

`[tools.shell]` のコマンド設定に権限レベル (`permission`) フィールドを追加し、「誰の指示であればそのコマンドを実行できるか」をコマンドごとに制御する。

### 10.1 権限レベルの定義

| レベル | 説明 | 典型的なコマンド例 |
|--------|------|--------------------|
| `owner` | エージェントのオーナーが直接指示した場合のみ実行可能。エージェント自身も自律的には使えない。 | `rm`, `shutdown`, `kill`, システム操作全般 |
| `agent` | エージェントが自律的に使えるコマンド。通常の作業用。 | `curl`, `jq`, `git`, `ls`, `cat` |
| `co-agent` | 共同作業が認められた他のエージェント（co-agent）からの指示でも使えるコマンド。最も緩い権限。 | `echo`, `date`, `pwd` |

**デフォルト拒否の原則**: ホワイトリスト（`[[tools.shell.commands]]`）に存在しないコマンドは、権限レベルに関わらずすべて拒否する。未登録コマンドは「許可されていない」とみなす。

### 10.2 Config設計

```toml
[tools.shell]
enabled = true

# ownerのみが指示できるコマンド（エージェント自身は自律的に使えない）
[[tools.shell.commands]]
name = "rm"
permission = "owner"
description = "ファイル削除（危険操作のためownerのみ）"

[[tools.shell.commands]]
name = "kill"
permission = "owner"
description = "プロセス終了（危険操作のためownerのみ）"

# エージェントが自律的に使えるコマンド
[[tools.shell.commands]]
name = "curl"
permission = "agent"
timeout_secs = 30
description = "HTTPリクエスト"

[[tools.shell.commands]]
name = "jq"
permission = "agent"
description = "JSONパース"

# co-agentからの指示でも使えるコマンド
[[tools.shell.commands]]
name = "echo"
permission = "co-agent"
description = "標準出力（最も安全なコマンド）"

[[tools.shell.commands]]
name = "date"
permission = "co-agent"
description = "現在時刻取得"
```

#### 既存設定からの移行方針

既存の `allowed_commands = ["curl", "echo"]` フラットリストは後方互換として引き続きサポートするが、非推奨とする。

- フラットリストのコマンドは自動的に `permission = "agent"` として扱う
- 移行ガイドを提供し、次のメジャーバージョンでフラットリスト形式を削除予定
- 設定ロード時に `allowed_commands` が存在する場合は警告ログを出力する

```toml
# 非推奨（後方互換のみ）
[tools.shell]
allowed_commands = ["curl", "echo"]  # WARNING: 非推奨。[[tools.shell.commands]] への移行を推奨

# 推奨（新形式）
[[tools.shell.commands]]
name = "curl"
permission = "agent"
```

### 10.3 指示者の判定方法

#### ActionContext への権限情報の追加

アクションが呼び出される際、`ActionContext` に呼び出し元の権限レベルを付与する。

```rust
pub struct ActionContext {
    pub agent_id: String,
    pub workspace_path: PathBuf,
    pub caller: CallerIdentity,  // 追加: 誰がこのアクションを呼んだか
}

pub enum CallerIdentity {
    Owner,                        // オーナー（人間）からの指示
    AgentSelf,                    // エージェント自身の自律的判断
    CoAgent { agent_id: String }, // 認証済みco-agentからの指示
}
```

#### 判定フロー

1. **オーナー判定**: セッションコンテキストにオーナー認証トークンが含まれている場合 → `CallerIdentity::Owner`
2. **エージェント自身**: エージェントの内部ロジックから直接呼ばれた場合 → `CallerIdentity::AgentSelf`
3. **co-agent判定**: メッセージの送信元エージェントIDが `config/agents/{agent_id}.toml` の `trusted_co_agents` リストに含まれている場合 → `CallerIdentity::CoAgent { agent_id }`

#### co-agentの認証方法

```toml
# config/agents/{agent_id}.toml
[agent]
id = "my-agent"

# 信頼するco-agentのIDリスト（ownerが管理）
trusted_co_agents = [
    "helper-agent-1",
    "tool-agent-2",
]
```

- `trusted_co_agents` リストはownerのみが変更可能
- エージェント自身は `trusted_co_agents` を変更できない（Section 11 参照）
- co-agentの認証はセッションコンテキストの `sender_agent_id` フィールドで行う

#### セッションコンテキストからの取得

```rust
impl ActionContext {
    pub fn from_session(session: &SessionContext, config: &AgentConfig) -> Self {
        let caller = match session.origin {
            Origin::HumanOwner { token } if verify_owner_token(token) => CallerIdentity::Owner,
            Origin::AgentSelf => CallerIdentity::AgentSelf,
            Origin::RemoteAgent { agent_id } => {
                if config.trusted_co_agents.contains(&agent_id) {
                    CallerIdentity::CoAgent { agent_id }
                } else {
                    CallerIdentity::Unknown  // 未認証エージェントは権限なし
                }
            }
        };
        ActionContext { agent_id: session.agent_id.clone(), caller, .. }
    }
}
```

### 10.4 権限チェックロジック

コマンド実行前に以下の順序でチェックを行う。

```
1. コマンドがホワイトリストに存在するか？
   → 存在しない場合: BLOCK（デフォルト拒否）

2. コマンドの permission レベルは何か？
   → "co-agent": co-agent / AgentSelf / Owner すべて許可
   → "agent":    AgentSelf / Owner のみ許可（co-agentは拒否）
   → "owner":    Owner のみ許可（AgentSelf / co-agent は拒否）

3. caller の CallerIdentity は何か？
   → 上記マトリクスに基づいて許可/拒否を決定
```

#### 権限マトリクス

| コマンド permission | Owner | AgentSelf | CoAgent |
|--------------------|-------|-----------|---------|
| `owner`            | ✅    | ❌        | ❌      |
| `agent`            | ✅    | ✅        | ❌      |
| `co-agent`         | ✅    | ✅        | ✅      |
| （未登録）          | ❌    | ❌        | ❌      |

### 10.5 ブロック処理

権限不足または未登録コマンドの実行を試みた場合の挙動：

#### エラーレスポンス

```rust
// 権限不足
ActionResult::error(format!(
    "Command {} requires {} permission, but caller has {} level. \
     This command can only be used when instructed by {}.",
    cmd_name,
    required_permission,
    caller_level,
    required_description  // "the owner" / "the agent itself or owner" 等
))

// 未登録コマンド（ホワイトリスト外）
ActionResult::error(format!(
    "Command {} is not in the allowed command list. \
     All commands must be explicitly registered in [[tools.shell.commands]] config. \
     Contact the owner to add this command.",
    cmd_name
))
```

#### 監査ログへの記録

ブロックされたコマンドは以下の情報とともに監査ログに記録する：

```json
{
  "event": "command_blocked",
  "timestamp": "2026-03-21T02:00:00Z",
  "agent_id": "my-agent",
  "caller": { "type": "CoAgent", "agent_id": "helper-agent-1" },
  "command": "rm",
  "required_permission": "owner",
  "caller_permission": "co-agent",
  "reason": "permission_insufficient"
}
```

---

## 11. エージェントによる設定変更

### 概要

エージェント自身が設定を変更できる仕組みを提供する。ただし、変更可能な範囲はオーナーが事前に定義した範囲に限定される。これにより、エージェントが自律的に自分のツール設定を調整できる柔軟性と、オーナーが最終的な制御権を持つ安全性を両立する。

### 11.1 変更できる設定の範囲

#### エージェントが変更できるもの

- **自分のエージェント設定ファイル**: `config/agents/{agent_id}.toml`
  - `[[tools.shell.commands]]` のうち `permission = "agent"` または `permission = "co-agent"` のコマンドの追加・削除
  - ただし、**ownerが `agent_modifiable = true` を設定したコマンドのみ**追加・削除可能
  - `timeout_secs` などのパラメータ調整（ownerが許可した範囲内で）

```toml
# ownerがエージェントによる変更を許可した設定の例
[[tools.shell.commands]]
name = "curl"
permission = "agent"
timeout_secs = 30
agent_modifiable = true   # エージェントがtimeout_secsを変更可能
modifiable_fields = ["timeout_secs"]  # 変更可能なフィールドを制限
```

#### エージェントが変更できないもの

- `permission = "owner"` のコマンドリスト（追加・削除・変更すべて不可）
- `trusted_co_agents` リスト（co-agentの信頼リストはownerのみ管理）
- 他のエージェントの設定ファイル（`config/agents/{other_agent_id}.toml`）
- デフォルト設定（`config/default.toml`）
- `agent_modifiable = false`（または未設定）のフィールド

### 11.2 update_tool_config アクションの設計

#### アクション定義

```
アクション名: update_tool_config
呼び出し権限: AgentSelf のみ（ownerも直接設定ファイルを編集すればよいため）
```

#### 引数

| フィールド | 型 | 必須 | 説明 |
|-----------|-----|------|------|
| `command_name` | String | ✅ | 変更対象のコマンド名 |
| `operation` | Enum | ✅ | `add` / `remove` / `update` |
| `fields` | Map<String, Value> | ✅（add/update時） | 設定する値 |

#### 実行フロー

```
1. バリデーション（Section 8 のバリデーションを適用）
   - command_name が有効な形式か（英数字・ハイフンのみ）
   - fields の値が許可された型・範囲内か

2. 権限チェック
   - 対象コマンドが agent_modifiable = true か
   - operation = "add" の場合: 追加するコマンドの permission が "owner" でないか
   - 変更しようとしているフィールドが modifiable_fields に含まれるか

3. 変更の適用
   - config/agents/{agent_id}.toml を更新
   - Section 7 のホットリロードフローで即時反映

4. 監査ログへの記録（11.3 参照）

5. ActionResult::success で変更後の設定を返す
```

#### エラーケース

```rust
// 変更不可のコマンドを変更しようとした場合
ActionResult::error(
    "Command rm has permission=owner and cannot be modified by the agent. \
     Only the owner can manage owner-level commands."
)

// agent_modifiable = false のフィールドを変更しようとした場合
ActionResult::error(
    "Field permission in command curl is not modifiable by the agent. \
     Modifiable fields are: [timeout_secs]. Contact the owner to change other fields."
)

// 他のエージェントの設定を変更しようとした場合
ActionResult::error(
    "Cannot modify config for agent other-agent. \
     Agents can only modify their own configuration."
)
```

### 11.3 監査ログ

設定変更はすべて監査ログに記録する。ログファイルの場所: `data/agents/{agent_id}/audit.log`

#### ログエントリのフォーマット

```json
{
  "event": "config_changed",
  "timestamp": "2026-03-21T02:15:00Z",
  "agent_id": "my-agent",
  "caller": { "type": "AgentSelf" },
  "action": "update_tool_config",
  "target": {
    "command_name": "curl",
    "operation": "update",
    "field": "timeout_secs"
  },
  "before": { "timeout_secs": 30 },
  "after": { "timeout_secs": 60 },
  "result": "success"
}
```

#### 記録される情報

| フィールド | 内容 |
|-----------|------|
| `event` | `config_changed` 固定 |
| `timestamp` | ISO 8601形式のUTC時刻 |
| `agent_id` | 変更を行ったエージェントのID |
| `caller` | 呼び出し元の `CallerIdentity`（エージェント自身の変更は `AgentSelf`） |
| `action` | 実行したアクション名 |
| `target` | 変更対象（コマンド名・操作種別・フィールド名） |
| `before` | 変更前の値 |
| `after` | 変更後の値 |
| `result` | `success` / `error`（エラー時はerrorメッセージも記録） |

#### ログの保持と参照

- 監査ログはローテーションせず追記のみ（削除・変更禁止）
- ownerはいつでも監査ログを参照できる
- エージェント自身は監査ログを読むことはできるが、書き込み・削除は不可
- 監査ログ自体の設定変更は `update_tool_config` のスコープ外（変更不可）

### 11.4 設定変更の安全性まとめ

| 操作 | Owner | AgentSelf | CoAgent |
|------|-------|-----------|---------|
| `owner` 権限コマンドの追加・削除 | ✅（直接ファイル編集） | ❌ | ❌ |
| `agent` 権限コマンドの追加（`agent_modifiable=true`） | ✅ | ✅ | ❌ |
| `agent_modifiable` フラグ自体の変更 | ✅ | ❌ | ❌ |
| `trusted_co_agents` の変更 | ✅ | ❌ | ❌ |
| 他エージェントの設定変更 | ✅（直接ファイル編集） | ❌ | ❌ |
| デフォルト設定の変更 | ✅（直接ファイル編集） | ❌ | ❌ |

この設計により、エージェントはオーナーが許可した範囲内でのみ自律的に設定を調整でき、オーナーは常に最終的な制御権を保持する。

---

## 10.6 ダッシュボードからのco-agent管理

> **補足**: Section 10.3「co-agentの認証方法」の続き。TOMLファイルを直接編集する代わりに、ダッシュボードUIで管理する方法。

opencrabのダッシュボード（`http://localhost:3000`）のエージェント管理画面（`dashboard/src/routes/agents.rs`）から、co-agentの信頼設定をGUI操作で管理できる。

### UIの配置方針

既存の `routes/agents.rs` に「Co-Agents」タブを追加する。エージェント詳細ページ（`AgentDetail` コンポーネント）内に以下のタブ構成で追加するのが自然：

```
[Overview] [Skills] [Sessions] [Co-Agents] [Settings]
```

`Co-Agents` タブの内容：
- 信頼済みco-agentの一覧表示（エージェントID・許可アクション・登録日時）
- 「Add Co-Agent」ボタン → エージェントID入力 + 許可アクション選択モーダル
- 各エントリに「Remove」ボタン（確認ダイアログ付き）
- **co-agent設定の変更はownerのみ可能**（dashboard認証と連動）

### DBスキーマへの追加設計（概念レベル）

```sql
-- co-agentの信頼関係テーブル
CREATE TABLE trusted_co_agents (
  id           TEXT PRIMARY KEY,
  agent_id     TEXT NOT NULL,        -- 信頼する側のエージェント
  co_agent_id  TEXT NOT NULL,        -- 信頼されるco-agent
  allowed_actions TEXT,              -- JSON配列 (null = 全アクション許可)
  created_by   TEXT NOT NULL,        -- owner（変更者のowner ID）
  created_at   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE (agent_id, co_agent_id)
);
```

`allowed_actions` の値例：
- `null` → 全アクション許可（現行の `trusted_co_agents` リストと同等）
- `["execute_shell", "ws_read"]` → 指定アクションのみ許可

### APIエンドポイント設計（概念レベル）

`dashboard/src/api.rs` に以下を追加する：

| メソッド | パス | 説明 | 必要権限 |
|---------|------|------|---------|
| `GET` | `/api/agents/{id}/co-agents` | co-agentリスト取得 | owner |
| `POST` | `/api/agents/{id}/co-agents` | co-agent追加 | owner |
| `PATCH` | `/api/agents/{id}/co-agents/{co_agent_id}` | 許可アクション更新 | owner |
| `DELETE` | `/api/agents/{id}/co-agents/{co_agent_id}` | co-agent削除 | owner |

リクエスト/レスポンス例（`POST /api/agents/{id}/co-agents`）：

```json
// リクエスト
{
  "co_agent_id": "helper-agent-1",
  "allowed_actions": ["execute_shell", "ws_read"]
}

// レスポンス
{
  "id": "ca_abc123",
  "agent_id": "my-agent",
  "co_agent_id": "helper-agent-1",
  "allowed_actions": ["execute_shell", "ws_read"],
  "created_by": "owner",
  "created_at": "2026-03-21T02:00:00Z"
}
```

### TOMLとDBの同期

既存の `trusted_co_agents = [...]` TOMLリストとDBテーブルは**段階的に移行**する：

1. **現状（Phase 1）**: TOMLのみ。ダッシュボードはTOMLを読み書き
2. **将来（Phase 2）**: DBテーブルを正とし、TOMLは起動時にDBから生成（バックワードコンパット維持）

Phase 1では `dashboard/src/api.rs` のco-agent APIがTOMLファイルを直接読み書きすることで、既存の `AgentConfig` 構造体との整合性を保つ。

---

## 12. スキルシステムとの統合

### 概要

opencrabの既存スキルシステム（`skills/*.skill.md`）とconfig-driven toolsを統合する。スキルのfrontmatterで使用可能なアクションを宣言し、スキルレベルの権限設定を可能にする。

### 12.1 既存スキルシステムの調査結果

現行の `.skill.md` ファイルのフロントマター構造（`crates/core/src/skill.rs` 調査より）：

```yaml
---
name: workspace-management
description: "ワークスペース管理スキル"
version: 1
actions:
  - ws_read
  - ws_write
  - ws_edit
  - ws_list
  - ws_delete
  - ws_mkdir
---
```

`Skill` 構造体のフィールド（`skill.rs` 実装）：
- `name: String`
- `description: String`
- `version: String`
- `actions: Vec<String>` — DBの `situation_pattern` カラムにJSON配列で保存
- `guidance: String` — LLMへのプロンプトガイダンス
- `source: SkillSource` — `Standard { file_path }` or `Acquired { source_type, source_context }`

### 12.2 execute_shellをスキルから使う方法

スキルの `.skill.md` フロントマターで `execute_shell` アクションを宣言することで、そのスキルにシェル実行権限を付与する：

```yaml
---
name: image-generation
description: "画像生成スキル - diffusionモデルを呼び出して画像を生成する"
version: 1
actions:
  - execute_shell  # tools.shell.commandsのホワイトリストに従う
  - generate_image # 専用Actionがある場合はそちらも宣言
---
```

**重要**: `execute_shell` を宣言しても、実際に実行できるコマンドは `tools.shell.commands` のホワイトリストに登録されているものだけ。スキルのfrontmatter宣言は「このスキルがシェル実行を使う意図がある」ことを明示するセキュリティ境界として機能する。

#### エンジン側の権限チェックフロー

```
スキルがアクションを呼び出す
  ↓
1. スキルのfrontmatter `actions` に該当アクションが含まれるか？
   → NO → 即拒否（スキル宣言外のアクションは使用不可）
  ↓
2. `execute_shell` の場合: `tools.shell.commands` ホワイトリストをチェック
   → コマンドが未登録 → 拒否
   → コマンドの `permission` をチェック
  ↓
3. スキルの `permission` レベルをチェック（12.3参照）
  ↓
4. 実行許可
```

`execute_shell` を宣言していないスキルがシェル実行を試みた場合のエラー：

```
ActionResult::error(
  "Skill 'image-generation' is not authorized to use execute_shell. \
   Add 'execute_shell' to the skill's actions frontmatter to enable this capability."
)
```

### 12.3 スキル単位の権限設定

スキルのfrontmatterに `permission` フィールドを追加することで、スキル全体の最低権限レベルを設定できる：

```yaml
---
name: system-maintenance
description: "システムメンテナンススキル - OSレベルの操作を行う"
version: 1
permission: owner   # このスキル全体がownerのみ使用可能
actions:
  - execute_shell
---
```

権限なしのスキルはデフォルトで `agent` 権限（エージェント自身が使用可能）：

```yaml
---
name: casual-chat
description: "カジュアル会話スキル"
version: 1
# permission未指定 → デフォルト: agent
actions:
  - send_speech
---
```

#### 権限レベルの優先順位

スキルレベルの権限とアクションレベルの権限が競合した場合、**より厳しい方（higher restriction wins）** が適用される：

| ケース | スキル permission | アクション permission | 実際に要求される権限 |
|--------|------------------|----------------------|---------------------|
| 通常 | `agent`（デフォルト） | `agent` | `agent` |
| スキルが厳しい | `owner` | `agent` | `owner` |
| アクションが厳しい | `agent` | `owner` | `owner` |
| 両方厳しい | `owner` | `owner` | `owner` |

例：`permission: owner` のスキルが `permission: agent` のコマンドを呼び出しても、スキルレベルの制約により owner 権限が必要になる。

### 12.4 スキルが使えるアクションの制限

スキルのfrontmatterに宣言されたアクションのみ実行可能。宣言外のアクション呼び出しはエンジンがブロックする。

これにより：
- **セキュリティ境界**: スキル作者が意図しないアクション使用を防ぐ
- **最小権限原則**: 必要なアクションのみを宣言し、不必要な権限を持たせない
- **監査性**: スキルが使えるアクションが frontmatter から一目でわかる

```yaml
# 悪い例: 何でもできてしまう（推奨しない）
---
name: generic-agent
version: 1
actions: ["*"]  # 全アクション許可（将来的にサポート予定だが非推奨）
---

# 良い例: 必要なアクションのみ宣言
---
name: file-processor
description: "ファイル処理スキル"
version: 1
actions:
  - ws_read
  - ws_write
  - execute_shell  # ファイル変換コマンド用
---
```

### 12.5 スキルローダーへの変更点（概念レベル）

現行の `skill.rs` の `row_to_skill` 関数では `actions` を `situation_pattern` フィールドから取得している。`permission` フィールドの追加には以下の変更が必要（概念レベル）：

```rust
// Skill構造体への追加
pub struct Skill {
    // ... 既存フィールド ...
    pub permission: SkillPermission,  // 追加
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPermission {
    Agent,   // デフォルト: エージェント自身が使用可能
    CoAgent, // co-agentからも使用可能
    Owner,   // ownerのみ使用可能
}

impl Default for SkillPermission {
    fn default() -> Self {
        SkillPermission::Agent
    }
}
```

DBスキーマへの追加（`skills` テーブル）：

```sql
ALTER TABLE skills ADD COLUMN permission TEXT NOT NULL DEFAULT '"agent"';
```

`.skill.md` パーサーは frontmatter の `permission` フィールドを読み取り、`Skill.permission` に格納する。未設定の場合は `SkillPermission::Agent` をデフォルトとする。
