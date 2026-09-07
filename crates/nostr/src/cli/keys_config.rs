const DEFAULT_NOSTARO_PATH: &str = "nostaro";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// 実行時に本鍵/生成鍵を渡す環境変数（nostaro 側が config より最優先で読む・#620）。
/// 鍵を config へ平文で書かず、spawn ごとにこの env で注入することで「設定を確認する
/// 操作で平文の鍵が目に入らない」を構造で成立させる。
const SECRET_KEY_ENV: &str = "NOSTARO_SECRET_KEY";

/// agent_id → 復号済み本鍵（nsec）を返すプロバイダ（#620）。DB の暗号文とマスターキーを
/// capture して主鍵を復号する。`base_command` の env 注入だけがこれを使う。
/// **`command_with_config`（本鍵/生成鍵の共有点）には差さない**（鍵混同防止）。
pub type MainKeyProvider = Arc<dyn Fn(&str) -> Result<Zeroizing<String>> + Send + Sync>;

/// 生成鍵ファイル（`enc:v1:…`）の暗号/復号に使うマスターキー holder（#620）。
pub type MasterKey = Arc<Zeroizing<[u8; secret_box::MASTER_KEY_LEN]>>;

/// DB の（暗号化された）本鍵を復号して返す [`MainKeyProvider`] を作る（#620）。
///
/// `base_command` に注入され、spawn ごとに `agent_id` の本鍵を DB から引いて復号し env へ
/// 載せる。暗号文（`enc:v1:…`）なら復号、平文（移行前）ならそのまま返す。マスターキーは
/// closure に capture する（外へ出さない）。
pub fn db_main_key_provider(db: opencrab_db::Db, master_key: MasterKey) -> MainKeyProvider {
    Arc::new(move |agent_id: &str| {
        let sk = {
            let conn = db
                .lock()
                .map_err(|_| anyhow::anyhow!("DB ロック取得に失敗しました"))?;
            opencrab_db::queries::get_agent_nostr_config(&conn, agent_id)
                .ok()
                .flatten()
                .map(|r| r.secret_key)
                .ok_or_else(|| anyhow::anyhow!("Nostr 設定が見つかりません"))?
        };
        if sk.trim().is_empty() {
            anyhow::bail!("秘密鍵が未設定です");
        }
        if secret_box::is_encrypted(&sk) {
            let bytes = secret_box::decrypt(&sk, &master_key)?;
            Ok(Zeroizing::new(
                String::from_utf8(bytes.to_vec()).context("復号した本鍵が UTF-8 ではありません")?,
            ))
        } else {
            // 移行前の平文（次回移行で暗号化される）。
            Ok(Zeroizing::new(sk))
        }
    })
}
/// nostaro を起動する作業ディレクトリ（= エージェントの workspace ルート）のテンプレート。
/// `config/default.toml` の `agent.workspace_path` と同じ既定値で、実配線ではそこから
/// [`NostaroCli::with_workspace_base`] で渡される（#299）。
const DEFAULT_WORKSPACE_BASE: &str = "data/agents/{agent_id}/workspace";
/// vanity 生成は prefix が長いと探索に非常に時間がかかる。上限を撤廃したため、
/// timeout で強制的に打ち切らず「実質無制限」にし、停止はキャンセル
/// （`CancellationToken` → タスク abort → future drop → `kill_on_drop` で nostaro を
/// kill）に委ねる。24h は保険（プロセス取り残し防止）で、通常はここに到達しない。
const DEFAULT_VANITY_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// vanity prefix に使える文字集合（bech32 小文字。npub の `npub1` 以降に現れる）。
/// `1` `b` `i` `o` は bech32 に存在しないので除外される。
const BECH32_CHARSET: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// vanity prefix の「目安」長。探索コストは 32^len で指数的に増える（32^3≈3万試行＝
/// ほぼ即時、それ以上は急激に伸びる）。**もはや上限として強制はしない**（ツール実行が
/// 内部 spawn + キャンセル可能になったため、長い prefix でも途中停止できる）。
/// 呼び出し側が UI/説明で目安を示すための参考値として残す。
pub const MAX_VANITY_PREFIX_LEN: usize = 3;

/// 新規生成された鍵。nsec は DB に保存し config へ materialize する。
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    pub nsec: String,
    pub npub: String,
    /// hex pubkey（任意。nostaro が返さなければ空）。
    pub pubkey: String,
}

/// vanity prefix を検証する（bech32 charset・長さ）。呼び出し前に弾いて、
/// 無効 prefix で nostaro を無駄に spawn したり探索が終わらないのを防ぐ。
pub fn validate_vanity_prefix(prefix: &str) -> Result<()> {
    // 長さ上限は撤廃した（探索は内部 spawn + キャンセルで途中停止できる）。
    // charset 検証だけは残す（bech32 に無い文字は永遠に一致せず探索が終わらないため）。
    for c in prefix.chars() {
        if !BECH32_CHARSET.contains(c) {
            anyhow::bail!(
                "vanity prefix に使えない文字 '{c}' があります（bech32 charset のみ: {BECH32_CHARSET}）"
            );
        }
    }
    Ok(())
}

/// nostaro CLI ラッパー。
#[derive(Clone)]
pub struct NostaroCli {
    binary_path: String,
    /// エージェント workspace のベーステンプレート（`{agent_id}` を含む）。nostaro を
    /// **その workspace ルートを cwd にして**起動するために使う（#299）。
    workspace_base: String,
    timeout: Duration,
    vanity_timeout: Duration,
    /// vanity 生成の同時実行を絞るゲート。`Arc` 共有なので clone 間で同じ制限が効く
    /// （HTTP ルートも LLM ツール経由も同じ 1 本のゲートを通る = 長時間 nostaro
    /// プロセスを並列に溢れさせない）。
    vanity_gate: Arc<Semaphore>,
    /// #620: 本鍵の実行時注入。`base_command` が `agent_id` からこれで復号済み本鍵を得て
    /// env へ載せる。`None` なら注入しない（テスト / 鍵不要コマンド）。本番では常に
    /// `Some`（マスターキーが在るときだけ Nostr サブシステムを起動するため）。
    main_key_provider: Option<MainKeyProvider>,
    /// #620: 生成鍵ファイル（`enc:v1:…`）の暗号/復号に使うマスターキー。`None` は
    /// テスト専用の平文フォールバック（本番では常に `Some`）。
    master_key: Option<MasterKey>,
}

impl std::fmt::Debug for NostaroCli {
    /// 鍵材料（provider / master_key）は**出さない**（Debug から秘密が漏れないように）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NostaroCli")
            .field("binary_path", &self.binary_path)
            .field("workspace_base", &self.workspace_base)
            .field("timeout", &self.timeout)
            .field("vanity_timeout", &self.vanity_timeout)
            .field("has_main_key_provider", &self.main_key_provider.is_some())
            .field("has_master_key", &self.master_key.is_some())
            .finish()
    }
}

impl Default for NostaroCli {
    fn default() -> Self {
        Self::new()
    }
}

impl NostaroCli {
    pub fn new() -> Self {
        Self {
            binary_path: DEFAULT_NOSTARO_PATH.to_string(),
            workspace_base: DEFAULT_WORKSPACE_BASE.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            vanity_timeout: Duration::from_secs(DEFAULT_VANITY_TIMEOUT_SECS),
            vanity_gate: Arc::new(Semaphore::new(1)),
            main_key_provider: None,
            master_key: None,
        }
    }

    /// 本鍵プロバイダ（agent_id → 復号済み本鍵）を注入する（#620）。`base_command` の
    /// env 注入だけがこれを使う。
    pub fn with_main_key_provider(mut self, provider: MainKeyProvider) -> Self {
        self.main_key_provider = Some(provider);
        self
    }

    /// 生成鍵ファイルの暗号/復号に使うマスターキーを注入する（#620）。
    pub fn with_master_key(mut self, key: MasterKey) -> Self {
        self.master_key = Some(key);
        self
    }

    /// 平文を封筒化する（生成鍵ファイル保存 / DB 本鍵の暗号化で使う）。マスターキー未注入
    /// （テスト）なら**平文のまま**返す（＝暗号化を有効化していない構成の従来挙動）。
    pub fn encrypt_secret(&self, plaintext: &str) -> Result<String> {
        match &self.master_key {
            Some(mk) => secret_box::encrypt(plaintext.as_bytes(), mk),
            None => Ok(plaintext.to_string()),
        }
    }

    /// 封筒（`enc:v1:…`）を復号して文字列で返す。**未暗号（平文）はそのまま返す**
    /// （移行前ファイル / テストの平文フォールバックに耐える）。封筒だがマスターキー未注入
    /// のときだけエラー。
    fn decrypt_secret(&self, material: &str) -> Result<Zeroizing<String>> {
        let material = material.trim();
        if !secret_box::is_encrypted(material) {
            return Ok(Zeroizing::new(material.to_string()));
        }
        let mk = self
            .master_key
            .as_ref()
            .context("マスターキー未設定のため暗号化された鍵を復号できません")?;
        let bytes = secret_box::decrypt(material, mk)?;
        let s = String::from_utf8(bytes.to_vec()).context("復号した鍵が UTF-8 ではありません")?;
        Ok(Zeroizing::new(s))
    }

    pub fn with_binary_path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        if !p.trim().is_empty() {
            self.binary_path = p;
        }
        self
    }

    /// エージェント workspace のベーステンプレート（`agent.workspace_path`）を設定する。
    /// 空は無視（既定を保つ）。`execute_shell` / `ws_*` と**同じテンプレート**を渡すことで、
    /// nostaro の cwd がそれらと同じディレクトリになる（#299）。
    pub fn with_workspace_base(mut self, base: impl Into<String>) -> Self {
        let b = base.into();
        if !b.trim().is_empty() {
            self.workspace_base = b;
        }
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.timeout = Duration::from_secs(secs);
        }
        self
    }

    /// vanity 生成の timeout（秒）を設定する。0 は無視（既定を保つ）。
    /// 探索を強制打ち切りしたくない場合は大きな値を渡し、停止はキャンセルに委ねる。
    pub fn with_vanity_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.vanity_timeout = Duration::from_secs(secs);
        }
        self
    }

    /// エージェント毎の Nostr 専用ディレクトリ（鍵・config の隔離先）。
    /// `validate_agent_id` を通す唯一の入口（パストラバーサル防止）。
    pub fn agent_nostr_dir(agent_id: &str) -> Result<PathBuf> {
        resolve_agent_workspace("data/agents/{agent_id}/nostr", agent_id)
    }

    /// エージェント毎の nostaro config パス（`--config` に渡す）。
    pub fn agent_config_path(agent_id: &str) -> Result<PathBuf> {
        Ok(Self::agent_nostr_dir(agent_id)?.join("config.toml"))
    }

    /// エージェントの workspace ルート（`execute_shell` / `ws_*` と同じディレクトリ）。
    /// nostaro の cwd に使う（#299）。
    pub fn agent_workspace_dir(&self, agent_id: &str) -> Result<PathBuf> {
        resolve_agent_workspace(&self.workspace_base, agent_id)
    }

    /// cwd（= エージェント workspace ルート）と `--config` の絶対パスを**セットで**決める
    /// （#299 / #301 レビュー反映）。
    ///
    /// cwd を workspace に移す以上、プロセス cwd 基準で組まれた `--config` は絶対化しないと
    /// `<workspace>/data/agents/...` を探して必ず見失う。逆に「config だけ絶対・cwd はそのまま」
    /// も基準ズレは直らない。よって**両方成功したときだけ両方適用**し、途中で失敗したら
    /// `None` を返して**両方見送る**（＝ #299 修正前の挙動そのままに degrade する。cwd は
    /// サーバプロセスのものを継承し、config は従来どおり相対のまま渡る）。片方だけ適用された
    /// 中間状態は作らない。
    ///
    /// 失敗しうるのは次の 3 つで、いずれも degrade（`warn` を 1 行出す）：
    /// - workspace テンプレートの解決失敗
    /// - `current_dir()` の取得失敗（相対パスの解決基準が無い）
    /// - workspace ディレクトリの作成失敗（同名ファイルで塞がれている等）。ここで無理に
    ///   `current_dir` を設定すると spawn が `ENOENT`/`ENOTDIR` で落ち、「nostaro が PATH に
    ///   無い」場合と区別の付かないエラー文面になるため、設定しない方が安全。
    fn plan_cwd_and_config(
        &self,
        agent_id: &str,
        config_path: &Path,
    ) -> Option<(PathBuf, PathBuf)> {
        let root = match self.agent_workspace_dir(agent_id) {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "nostr: workspace パスを解決できないため cwd 固定と --config 絶対化を見送る（従来どおりサーバ cwd で起動）"
                );
                return None;
            }
        };
        // 相対パスの解決基準は **1 回だけ**取る（cwd と config で別々に取らない）。
        let process_cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "nostr: current_dir を取得できないため cwd 固定と --config 絶対化を見送る（従来どおりサーバ cwd で起動）"
                );
                return None;
            }
        };
        let root = absolutize_with(&root, &process_cwd);
        // ディレクトリを用意する（gateway の restore_from_db は「一度も走っていない＝
        // Workspace 未作成」のエージェントでも watch を張りうるので、ここは実際に仕事をする）。
        if let Err(e) = std::fs::create_dir_all(&root) {
            tracing::warn!(
                agent_id = %agent_id,
                error = %e,
                "nostr: workspace ディレクトリを用意できないため cwd 固定と --config 絶対化を見送る（従来どおりサーバ cwd で起動）"
            );
            return None;
        }
        Some((root, absolutize_with(config_path, &process_cwd)))
    }

    /// `--config <path>` 付きの Command を組み、可能なら cwd をエージェントの workspace
    /// ルートに固定する（#299）。
    ///
    /// これが無いと nostaro は opencrab-server プロセスの cwd（リポジトリルート）を継承し、
    /// `execute_shell` / `ws_*`（`cmd.current_dir(ctx.workspace.root())`）と基準がズレる。
    /// その結果、エージェントが `ws_write` で書いたファイルを `--file <相対パス>` に渡すと
    /// 見つからず、`--out <相対パス>` の出力は `ws_read` から見えなかった。
    ///
    /// cwd 固定と `--config` 絶対化は [`Self::plan_cwd_and_config`] で「両方成功 or
    /// 両方見送り」になる。`base_command` と `generated_key_command` は**この 1 箇所**を
    /// 共有する（片方だけ直す退行を作らない）。
    fn command_with_config(&self, agent_id: &str, config_path: &Path) -> Command {
        let mut cmd = Command::new(&self.binary_path);
        cmd.kill_on_drop(true);
        match self.plan_cwd_and_config(agent_id, config_path) {
            Some((cwd, config)) => {
                cmd.arg("--config").arg(config);
                cmd.current_dir(cwd);
            }
            // degrade：cwd は設定せず、config も従来どおり（絶対化しない）渡す。
            None => {
                cmd.arg("--config").arg(config_path);
            }
        }
        cmd
    }

    /// 共通の base command（`nostaro --config <per-agent> <subcommand>...`）。
    ///
    /// #620: **本鍵を env で注入する**（config へ平文で置かない）。config.toml はもう鍵行を
    /// 持たず、実行時に provider が DB の暗号文を復号して `NOSTARO_SECRET_KEY` へ載せる。
    /// provider 未注入（テスト / 鍵不要）なら env を付けず、nostaro は config へフォール
    /// バックする（テストの平文 config はそのまま動く）。
    fn base_command(&self, agent_id: &str) -> Result<Command> {
        let config_path = Self::agent_config_path(agent_id)?;
        // 親ディレクトリを用意（config の置き場所）。
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut cmd = self.command_with_config(agent_id, &config_path);
        if let Some(provider) = &self.main_key_provider {
            let nsec = provider(agent_id)?;
            cmd.env(SECRET_KEY_ENV, nsec.as_str());
        }
        Ok(cmd)
    }

    /// 送信系の command を組む。`from` が None なら本鍵（config.toml）、Some(npub) なら
    /// **そのエージェントが生成した鍵**（`generated-keys/<npub>.nsec`）で投稿する。
    fn command_for(&self, agent_id: &str, from: Option<&str>) -> Result<Command> {
        match from.map(str::trim).filter(|s| !s.is_empty()) {
            None => self.base_command(agent_id),
            Some(npub) => self.generated_key_command(agent_id, npub),
        }
    }

    /// generated key（`from`）用の一時 config を用意して `--config` 付き Command を返す。
    ///
    /// `from` に使えるのは**このエージェントが生成した鍵のみ**（`generated-keys/<npub>.nsec`
    /// が存在するもの）。本設定 config.toml の relays/blossom を継承し、secret_key 行だけを
    /// 生成鍵に差し替えた from-config を 0600 で作る（本設定と同じリレーへ publish する）。
    fn generated_key_command(&self, agent_id: &str, from_npub: &str) -> Result<Command> {
        let stem = sanitize_key_stem(from_npub);
        if stem.is_empty() {
            anyhow::bail!("from の npub が不正です");
        }
        let nsec_path = Self::agent_nostr_dir(agent_id)?
            .join("generated-keys")
            .join(format!("{stem}.nsec"));
        let material = std::fs::read_to_string(&nsec_path).map_err(|_| {
            anyhow::anyhow!(
                "指定 npub の鍵が見つかりません（from に指定できるのは、このエージェントが \
                 nostr_generate_key で生成した鍵だけです）"
            )
        })?;
        // #620: 生成鍵ファイルは暗号文（`enc:v1:…`）。復号して env で注入する。
        let nsec = self.decrypt_secret(&material)?;
        // `--config` は**鍵行なしの本設定**をそのまま使う（relays/blossom を継承）。平文
        // from-config の生成はやめた。共有点 [`command_with_config`] には鍵を差さず、ここで
        // 生成鍵だけを env に載せる（本鍵で送るべき投稿が生成鍵で／その逆で送られる鍵混同を防ぐ）。
        // base_command は通さない（あれは本鍵を注入する）。cwd/config 絶対化の degrade 条件は
        // base_command と同一（command_with_config を共有）。
        let main_path = Self::agent_config_path(agent_id)?;
        if !main_path.exists() {
            anyhow::bail!("本設定 (config.toml) がありません。先に Nostr を設定してください");
        }
        let mut cmd = self.command_with_config(agent_id, &main_path);
        cmd.env(SECRET_KEY_ENV, nsec.as_str());
        Ok(cmd)
    }

    /// generated key の nsec をサーバ内から読む（**サーバ側専用**。LLM には渡さない）。
    /// identity 乗り換え（本鍵採用）で使う。存在チェック＝「自分が生成した鍵のみ」を担保。
    pub fn read_generated_key(&self, agent_id: &str, npub: &str) -> Result<String> {
        let stem = sanitize_key_stem(npub);
        if stem.is_empty() {
            anyhow::bail!("npub が不正です");
        }
        let path = Self::agent_nostr_dir(agent_id)?
            .join("generated-keys")
            .join(format!("{stem}.nsec"));
        let material = std::fs::read_to_string(&path).map_err(|_| {
            anyhow::anyhow!(
                "指定 npub の生成鍵が見つかりません（このエージェントが生成した鍵のみ採用できます）"
            )
        })?;
        // #620: 生成鍵ファイルは暗号文。復号して返す（サーバ内でのみ使い、LLM には渡さない）。
        Ok(self.decrypt_secret(&material)?.trim().to_string())
    }

    /// このエージェントが生成した鍵（`generated-keys/<npub>.nsec`）の **npub 一覧**を返す。
    ///
    /// **秘密鍵(nsec)は読まない・返さない**（ファイル本文は一切開かず、ファイル名＝npub
    /// のみを列挙する）。ファイル名の stem は `save_generated_key` が
    /// `sanitize_key_stem`（英数字のみ）で焼いたもの。bech32 の npub は英数字だけなので
    /// stem がそのまま npub になる。`generated-keys/` ディレクトリが無ければ空の Vec。
    /// `.config.toml`（`from` 送信用の一時 config）等、`.nsec` 以外は無視する。
    ///
    /// **堅牢化（#265 レビュー）**: 通常ファイルのみ対象にし（ディレクトリ / symlink は
    /// 除外）、stem が `npub1` で始まるものだけを npub として返す（`sanitize_key_stem` の
    /// hex fallback で焼かれた非 npub な `.nsec` を採用候補に混ぜない）。
    pub fn list_generated_keys(agent_id: &str) -> Result<Vec<String>> {
        let dir = Self::agent_nostr_dir(agent_id)?.join("generated-keys");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // ディレクトリ未作成（まだ 1 度も生成していない）＝空一覧。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read key dir: {}", dir.display()))
            }
        };
        let mut npubs = Vec::new();
        for entry in entries.flatten() {
            // 通常ファイルのみ（ディレクトリ / symlink を列挙しない）。file_type が取れ
            // なければスキップ（本文は開かない）。
            match entry.file_type() {
                Ok(ft) if ft.is_file() => {}
                _ => continue,
            }
            let path = entry.path();
            // `<npub>.nsec` だけを対象にする（拡張子で判定。nsec 本文は開かない）。
            if path.extension().and_then(|e| e.to_str()) != Some("nsec") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                // npub の体裁（`npub1...`）のものだけ採用候補として返す。
                if stem.starts_with("npub1") {
                    npubs.push(stem.to_string());
                }
            }
        }
        // 決定的な順序で返す（列挙順は OS 依存）。
        npubs.sort();
        Ok(npubs)
    }

}
