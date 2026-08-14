//! nostaro（自作 Nostr CLI）を subprocess 制御するラッパー。
//!
//! codex/cursor プロバイダと同じ「別コマンドを spawn して制御」パターン。鍵の共有
//! 事故を防ぐため、エージェント毎に **一意な config パス**（`data/agents/{id}/nostr/
//! config.toml`）を `--config` で明示指定する（`resolve_agent_workspace` と同じ検証
//! 経路で組む）。リレー/フィルタは watch のフラグで渡し、nostaro の config 側 default
//! に依存しない（指定リレー以外に繋がせない）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use opencrab_core::secret_box;
use opencrab_core::workspace::resolve_agent_workspace;
use tokio::process::Command;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

use crate::config::NostrConfig;

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

    /// 一発実行系（post/reply/dm/zap/upload）を既定 timeout 付きで走らせ stdout を返す。
    async fn run(&self, cmd: Command) -> Result<String> {
        self.run_with_timeout(cmd, self.timeout).await
    }

    /// 指定 timeout でコマンドを走らせ stdout を返す（vanity 等の長時間処理用）。
    async fn run_with_timeout(&self, mut cmd: Command, timeout: Duration) -> Result<String> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("nostaro timed out after {}s", timeout.as_secs()))?
            .context("failed to run nostaro")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // config パース失敗時、nostaro は config 先頭行（`secret_key = "nsec1..."`）を
            // stderr にエコーする。そのまま anyhow エラー/ログへ載せると平文 nsec が漏れる
            // ため、秘密材料をマスクしてから載せる（#262 セキュリティ所見）。
            anyhow::bail!(
                "nostaro failed ({}): {}",
                output.status,
                mask_secrets(stderr.trim())
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// `nostaro post -- "<text>"` — 新規ノート投稿。
    ///
    /// 全ての positional 引数の前に `--`（オプション終端）を置く。target/text/recipient
    /// 等はモデル/受信イベント由来で、`-` 始まりの値をフラグと誤解釈させない
    /// （引数インジェクション対策）。
    pub async fn post(&self, agent_id: &str, text: &str, from: Option<&str>) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("post").arg("--").arg(text);
        self.run(cmd).await
    }

    /// `nostaro reply -- <target> "<text>"` — 返信。target は note1.../hex id。
    pub async fn reply(
        &self,
        agent_id: &str,
        target: &str,
        text: &str,
        from: Option<&str>,
    ) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("reply").arg("--").arg(target).arg(text);
        self.run(cmd).await
    }

    // #514: DM 送信メソッド（`nostaro dm send`）は撤去した。DM は秘密鍵漏洩で過去に
    // 遡って全部読めるため送信禁止。`nostr_dm` ツールの撤去に加え、送信の実プリミティブ
    // 自体をここから消し、`nostr_run dm` passthrough も `PASSTHROUGH_DENIED_SUBCOMMANDS`
    // で塞いでいる（送信の全経路を封じる）。

    /// `nostaro zap -m <message> -- <recipient> <amount>`。
    /// `-m` は positional の前（`--` 後に置くと value 扱いされるため）。
    pub async fn zap(
        &self,
        agent_id: &str,
        recipient: &str,
        amount: u64,
        message: Option<&str>,
        from: Option<&str>,
    ) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("zap");
        if let Some(m) = message.filter(|s| !s.is_empty()) {
            // 値も = 形式で確実に束ねる（`-` 始まりのメッセージ対策）。
            cmd.arg(format!("-m={m}"));
        }
        cmd.arg("--").arg(recipient).arg(amount.to_string());
        self.run(cmd).await
    }

    /// `nostaro upload -- <path>` — Blossom アップロード。返り値は URL。
    pub async fn upload(&self, agent_id: &str, path: &str, from: Option<&str>) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("upload").arg("--").arg(path);
        self.run(cmd).await
    }

    /// `nostr_run`（server-own passthrough / #268）で**拒否**するサブコマンド。
    ///
    /// - `init`: 鍵の作成/上書き。鍵管理は opencrab の `nostr_generate_key` /
    ///   `nostr_switch_identity` に閉じる（passthrough から鍵をいじらせない）。
    /// - `watch`: 受信は per-agent ゲートウェイ管理が担う。passthrough から
    ///   `--no-mention-only` 等で無制限 watch を上げさせない（かつ長時間ブロックを避ける）。
    /// - `relay`: リレー設定の真実源は opencrab の DB（`agent_nostr_config`）で、config.toml は
    ///   materialize で毎回上書きされる。passthrough から `relay add/remove` すると config.toml
    ///   だけ書き換わって DB と desync し、次の gateway start / switch_identity で黙って揮発する。
    ///   よってリレー管理は opencrab の DB 経路（configure_nostr / ダッシュボード）に閉じる。
    ///   壊れている（揮発する）機能を塞ぐので**非劣化ではない**。
    /// - `dm`（#514）: DM 送信は禁止。DM は暗号化されていても秘密鍵が漏れた時点で過去に
    ///   遡って全部読めるため、その前提ごと無くす（オーナー決定）。`nostr_dm` ツールの撤去
    ///   だけでは `nostr_run dm send` から通ってしまう（passthrough は inner ツール名を
    ///   隠しても能力は塞げない / #306）ので、送信のもう一方の経路であるここも塞ぐ。
    ///   `nostr_run dm ...` は拒否される。private な話は Discord の DM か指定チャンネルへ。
    /// - `event`（#514）: `nostaro event` は**任意 kind を publish する汎用コマンド**で、
    ///   `-k 4` / `--kind=1059`、さらに `--file <JSON>`（`{"kind":4,...}`）でも kind を
    ///   指定できる。つまり `dm` を塞いでも `nostr_run event -k 4 -t "p,<pubkey>" -c ...` で
    ///   DM（kind:4 / 1059）を投げられる。**「--kind が DM のときだけ拒否」は不可**:
    ///   `-k`/`--kind`/`--kind=`/JSON file の表記ゆれを全部拾う必要があり、取りこぼすと
    ///   静かに穴が残る（security boundary を引数解析に依存させない）。よって `event` は
    ///   丸ごと拒否する。長文（NIP-23）やカスタム kind の正当用途は現時点で実績が無く、
    ///   必要になれば `nostr_post` / `nostr_reply` のような**専用ツール**を足す形が安全。
    ///   （NIP-28 の `channel` は kind:40/41/42 の**公開**イベントで暗号化オプションも無く、
    ///   private/DM 経路ではないので拒否しない。）
    ///
    /// これ以外のサブコマンドは**そのまま nostaro に委ねる**（Nostr 仕様の判断は
    /// opencrab で再実装せず nostaro に委譲する＝非劣化）。
    pub const PASSTHROUGH_DENIED_SUBCOMMANDS: &'static [&'static str] =
        &["init", "watch", "relay", "dm", "event"];

    /// nostaro サブコマンドを**薄く passthrough 実行**する（#268）。
    ///
    /// `nostaro --config <data/agents/{agent_id}/nostr/config.toml> <subcommand> [args]` を
    /// **構造化引数**で起動する（シェル文字列を組まないので注入不可）。守るのは 2 点だけ:
    ///
    /// 1. **鍵のエージェント間混同防止**: config は常に `agent_id` のもの
    ///    （[`base_command`](Self::base_command)）。`--config` を args で上書きさせない。
    /// 2. **nsec 隠蔽**: agent は nsec を引数に持たない前提に加え、`init` を拒否して鍵の
    ///    作成/上書きを塞ぎ、stdout / エラー出力の双方を [`mask_secrets`] に通す。
    ///
    /// `init`/`watch`/`relay`/`dm`/`event` は拒否し、それ以外は素通しする。config.toml 未
    /// materialize（鍵未採用）なら nostaro を spawn せず明示エラーを返す。
    pub async fn run_passthrough(
        &self,
        agent_id: &str,
        subcommand: &str,
        args: &[String],
    ) -> Result<String> {
        let sub = subcommand.trim();
        if sub.is_empty() {
            anyhow::bail!("subcommand が空です");
        }
        // deny: 鍵の作成/上書き（init）・無制限受信（watch）・リレー編集（relay）・
        // DM 送信（dm）・任意 kind publish（event: DM を迂回できる / #514）。
        // relay は config.toml だけ書き換えて DB(agent_nostr_config) と desync し次の
        // gateway start / switch_identity で揮発するため塞ぐ。
        if Self::PASSTHROUGH_DENIED_SUBCOMMANDS.contains(&sub) {
            anyhow::bail!(
                "nostr_run では '{sub}' は実行できません（init は nostr_generate_key / \
                 nostr_switch_identity に、watch はゲートウェイ管理に閉じています。リレー設定は \
                 opencrab 側（configure_nostr / ダッシュボード）で管理してください。dm と event は \
                 #514 で禁止です — DM は秘密鍵漏洩で過去に遡って読めるため扱わず、event は任意 \
                 kind を投げられ DM を迂回できてしまうため塞いでいます。private な話は Discord の \
                 DM か指定チャンネルを使ってください）"
            );
        }
        // `--config` の上書きを封じる（config は常にあなた自身の鍵設定＝鍵混同防止を回避
        // させない）。それ以外のフラグは nostaro にそのまま委ねる。
        if args
            .iter()
            .any(|a| a == "--config" || a.starts_with("--config="))
        {
            anyhow::bail!(
                "--config は指定できません（config は常にあなた自身の Nostr 鍵設定を使います）"
            );
        }
        // config.toml が無い＝まだ本鍵を採用していない。nostaro を spawn せず明示エラー。
        let config_path = Self::agent_config_path(agent_id)?;
        if !config_path.exists() {
            anyhow::bail!(
                "Nostr の鍵がまだ採用されていません（config.toml が未生成）。先に \
                 nostr_switch_identity で鍵を採用してください"
            );
        }
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg(sub);
        for a in args {
            cmd.arg(a);
        }
        // `run` は失敗時に stderr を mask する。成功時の stdout も念のため mask を通す
        // （config を表示しうる系のサブコマンドで万一 nsec が混じっても伏せる / 多層防御 #263）。
        self.run(cmd).await.map(|out| mask_secrets(&out))
    }

    /// `nostaro pubkey` — このエージェント（config）の公開鍵（hex）を返す。
    /// 自分の投稿への自己返信ループを防ぐために使う。
    pub async fn pubkey(&self, agent_id: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("pubkey");
        self.run(cmd).await
    }

    /// **生成鍵**（`from` npub）の公開鍵（hex）を返す（#620・identity 切替）。
    ///
    /// identity 切替では DB の本鍵を新鍵へ更新する**前**に新 pubkey が要る。本鍵プロバイダは
    /// DB を読むため `pubkey()` では旧鍵の pubkey が返る。ここは生成鍵を env で注入して
    /// nostaro に引かせるので、**新鍵が実際に使えること（検証）と新 pubkey の取得**を DB
    /// 更新前に同時に行える。
    pub async fn pubkey_from(&self, agent_id: &str, from_npub: &str) -> Result<String> {
        let mut cmd = self.generated_key_command(agent_id, from_npub)?;
        cmd.arg("pubkey");
        self.run(cmd).await
    }

    /// `nostaro vanity --json [--prefix=<p>]` — 新規鍵を生成して返す。
    ///
    /// prefix は npub の `npub1` 以降に前置される bech32 文字列。空なら通常のランダム鍵。
    /// **config を読まない**（新規鍵生成なので既存 nsec に依存しない）ため `--config` は
    /// 付けない。探索が終わらないよう prefix を検証し、長めの vanity_timeout を使う。
    pub async fn vanity(&self, prefix: &str) -> Result<GeneratedKey> {
        let prefix = prefix.trim().to_lowercase();
        validate_vanity_prefix(&prefix)?;
        // 同時実行を 1 に絞る（各生成は最大 vanity_timeout ぶん nostaro プロセスを
        // 抱える。並列に溢れさせない = DoS/資源枯渇の防止）。生成は ≤3 文字で通常
        // 即時なので、待ちは実質発生しない。
        let _permit = self
            .vanity_gate
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("vanity ゲートが閉じています"))?;
        // config 非依存なので base_command は使わず素で組む。
        let mut cmd = Command::new(&self.binary_path);
        cmd.kill_on_drop(true);
        cmd.arg("vanity").arg("--json");
        if !prefix.is_empty() {
            // `-` 始まりはあり得ない（bech32 charset 検証済み）が = 形式で束ねる。
            cmd.arg(format!("--prefix={prefix}"));
        }
        let out = self.run_with_timeout(cmd, self.vanity_timeout).await?;
        parse_generated_key(&out)
    }

    /// per-agent の nostaro config.toml を DB 由来のリレーから materialize する。
    ///
    /// #620: **secret_key 行はもう書かない**（鍵は実行時に env で注入する）。config には
    /// relays/default_relays/blossom だけを書く。エージェントがこの config を読んでも平文の
    /// 鍵は目に入らない。relays は送信（post/reply）が publish するリレー。受信は watch の
    /// フラグで別途明示する。partial read で誤ったリレーへ繋がせないよう、書き込みは従来どおり
    /// アトミックにする（[`write_secret_file`]）。
    pub fn materialize_config(
        agent_id: &str,
        relays: &[String],
        blossom_server: Option<&str>,
    ) -> Result<PathBuf> {
        let path = Self::agent_config_path(agent_id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create nostr dir: {}", parent.display()))?;
        }
        // TOML 文字列を壊す/追記させない文字（`"` `\` 改行）を除去する（値は URL 前提なので
        // 本来含まれない。防御的にサニタイズ）。
        let esc = |s: &str| {
            s.chars()
                .filter(|c| !matches!(c, '"' | '\\' | '\n' | '\r'))
                .collect::<String>()
        };
        let relay_list = relays
            .iter()
            .map(|r| format!("\"{}\"", esc(r)))
            .collect::<Vec<_>>()
            .join(", ");
        // nostaro は `relays` と `default_relays` の**両方**を必須フィールドとして要求する
        // （どちらか一方だけだと `missing field ...` で config パースが失敗し、post/watch/pubkey
        // など Nostr 全操作が止まる。#262）。opencrab は送信/受信リレーを常にフラグで明示する
        // ため両者は同値でよい。secret_key は Option なので省略しても parse できる（鍵は env）。
        let mut toml = format!("relays = [{relay_list}]\ndefault_relays = [{relay_list}]\n");
        if let Some(b) = blossom_server.filter(|s| !s.is_empty()) {
            toml.push_str(&format!("blossom_server = \"{}\"\n", esc(b)));
        }
        write_secret_file(&path, &toml)?;
        Ok(path)
    }

    /// LLM が生成した鍵の nsec を**サーバ内に 0600 で保存**する（LLM には返さない）。
    ///
    /// 保存先は per-agent の `data/agents/{id}/nostr/generated-keys/<npub>.nsec`。
    /// ファイル名は npub（無ければ pubkey）を bech32/hex 文字に限定して安全化する
    /// （パストラバーサル/インジェクション防止）。返り値は保存パス。
    pub fn save_generated_key(&self, agent_id: &str, key: &GeneratedKey) -> Result<PathBuf> {
        let dir = Self::agent_nostr_dir(agent_id)?.join("generated-keys");
        // 失敗経路でも鍵の**所在（保存先パス）をエラーに載せない**（#241）。成功経路は
        // 返り値の PathBuf をツール層（`nostr_generate_key`）が捨てて npub だけ返す＝所在を
        // LLM に渡さない。だが失敗経路の `with_context` がパスを載せると、その保護が失敗時
        // だけ破れ、エラーがツール結果としてそのままエージェントへ渡る。所在は**サーバログ
        // にだけ**残し、返すエラーは「失敗した」事実のみにする（運用者はログで所在を追える）。
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %format!("{e}"), "generated key: 保存先ディレクトリの作成に失敗");
            anyhow::bail!("鍵の保存先ディレクトリの作成に失敗しました");
        }
        // ファイル名は英数字のみ（bech32/hex は満たす）。空や異物は fallback。
        let stem = sanitize_key_stem(&key.npub);
        let stem = if stem.is_empty() {
            let hexed = sanitize_key_stem(&key.pubkey);
            if hexed.is_empty() {
                "key".to_string()
            } else {
                hexed
            }
        } else {
            stem
        };
        let path = dir.join(format!("{stem}.nsec"));
        // #620: 平文ではなく暗号文（`enc:v1:…`）で保存する（エージェントが `../nostr/
        // generated-keys/*.nsec` を読んでも平文の鍵が目に入らない）。マスターキー未注入
        // （テスト）は平文フォールバック。
        let material = self.encrypt_secret(&key.nsec)?;
        // `write_secret_file` のエラーには path が Context として載るため、ここで握り、
        // 所在はログにだけ残して、返すエラーからは落とす（上と同じ理由 = #241）。
        if let Err(e) = write_secret_file(&path, &material) {
            tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "generated key: 鍵ファイルの保存に失敗");
            anyhow::bail!("生成した鍵の保存に失敗しました");
        }
        Ok(path)
    }

    /// watch 用の Command を組む（spawn はループ側が行い、stdout の JSONL を読む）。
    ///
    /// リレー/フィルタは**必ずフラグで明示**して渡す（config の default に依存しない
    /// ＝指定リレー以外へ繋がせない）。`--json` で JSONL を stdout に出させる。
    ///
    /// ## 条件の結合は `--match=any` で固定する（#278）
    ///
    /// nostaro の `watch` は p タグ（mention-only）／keyword／author の 3 条件を
    /// `--match any|all` で結合する。opencrab は **`any`（OR）を明示的に渡す**。既定と
    /// 同じ値だが、**受信セマンティクスを argv に焼く**ことで nostaro 側の既定が将来
    /// 変わっても opencrab の受信が黙って変わらないようにする。
    ///
    /// `all`（AND）を選ばない理由は、nostaro では **mention-only も 1 つの条件**であり、
    /// `all` にすると「自分宛（p タグ）**かつ** keyword 一致」になるからである。運用者が
    /// keyword を設定しているエージェントでは、
    ///
    /// - 本文に keyword を含まない e/p タグだけの返信が落ちる（#271 で直したい事象そのもの）、
    /// - 本文が暗号文/絵文字である kind:4・1059（DM）や kind:7（リアクション）が
    ///   keyword 一致しえないので全部落ちる、
    /// - 「自分宛でない keyword 一致投稿を拾う」という運用者の意図（keyword 監視）も落ちる、
    ///
    /// という三重の劣化になる。`any` なら「自分宛は必ず届く（#271）＋ 運用者が明示した
    /// keyword/author の分が上乗せされる」となり、旧挙動を狭めない。
    ///
    /// なお `--mention-only` は nostaro 側の既定 true に委ね、**`--no-mention-only` は
    /// 絶対に渡さない**（渡すと p タグ条件が消えて全ノート購読になりうる）。この不変条件は
    /// `test_watch_command_never_disables_mention_only` で固定している。
    pub fn build_watch_command(&self, agent_id: &str, config: &NostrConfig) -> Result<Command> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("watch").arg("--json");
        // 条件の結合方法（OR）。既定と同値でも明示して契約を argv に残す（#278）。
        cmd.arg("--match=any");
        // フラグ値は `--flag=value` の = 形式で束ねる（`-` 始まりの author/keyword を
        // 別フラグと誤解釈させない = 引数インジェクション対策）。
        for relay in config.effective_relays() {
            cmd.arg(format!("--relay={relay}"));
        }
        for author in &config.filter.authors {
            cmd.arg(format!("--author={author}"));
        }
        for keyword in &config.filter.keywords {
            cmd.arg(format!("--keyword={keyword}"));
        }
        for kind in config.effective_kinds() {
            cmd.arg(format!("--kind={kind}"));
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd)
    }
}

/// 相対パスを `base`（＝プロセス cwd）基準で絶対化する（存在は要求しない）。
///
/// nostaro は**エージェント workspace を cwd にして**起動する（#299）ので、プロセス cwd
/// 基準で組まれた `data/agents/{id}/nostr/config.toml` のような相対パスをそのまま渡すと
/// workspace 配下を探して見失う。spawn 前にここで絶対化して基準ズレを断つ。
///
/// 基準は呼び出し側（[`NostaroCli::plan_cwd_and_config`]）が**1 回だけ**取得して渡す。
/// 取得できない場合は絶対化も cwd 固定も行わない（両方見送り）ので、ここでは fallback を
/// 持たない。
fn absolutize_with(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    base.join(path)
}

/// 鍵ファイル名の stem を英数字のみに安全化する（bech32 npub / hex pubkey は満たす）。
/// パストラバーサル/インジェクション防止。空文字は呼び出し側で fallback/拒否する。
fn sanitize_key_stem(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// nostaro の stderr/エラー文字列から秘密材料をマスクする。
///
/// config パース失敗時、nostaro は config 先頭行（`secret_key = "nsec1..."`）を stderr に
/// エコーする。これを anyhow エラーやログへ載せると平文の秘密鍵が漏れるため、載せる前に
/// **多層防御**で伏せる（#262）:
/// - 第1層: `secret_key` を含む行は行ごと伏せる。nostaro は行番号ガター付き
///   （`1 | secret_key = "..."`）でエコーするため `starts_with` では発火しない。
///   `contains` にして、ガター付き行も、nsec でない hex 秘密（`secret_key = "<64hex>"`）も
///   行ごと落とす。潰しすぎても秘密漏れ側には倒れない。
/// - 第2層: 文字列中の任意の `nsec1...`（bech32）トークンを伏せ字へ置換する。
///
/// `secret_key` を含まない診断行（`missing field ...` / `TOML parse error` 等）は残す。
/// regex 依存を持ち込まないよう手書きで処理する。
fn mask_secrets(input: &str) -> String {
    let mut lines: Vec<String> = input
        .lines()
        .map(|line| {
            if line.contains("secret_key") {
                // ガター（`1 | `）等の前置きは残し、`secret_key` 以降の値部分だけを伏せる。
                // これで行番号など診断に有用な前置きを保ちつつ、秘密値は確実に落とす。
                let idx = line.find("secret_key").unwrap();
                format!("{}secret_key = \"<redacted>\"", &line[..idx])
            } else {
                line.to_string()
            }
        })
        .collect();
    for line in &mut lines {
        *line = redact_nsec_tokens(line);
    }
    lines.join("\n")
}

/// 文字列中の `nsec1<bech32...>` トークンをすべて `nsec1<redacted>` に置換する。
fn redact_nsec_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("nsec1") {
            out.push_str("nsec1<redacted>");
            i += "nsec1".len();
            // bech32 データ部（小文字英数字）を読み飛ばして落とす。
            while i < bytes.len() && (bytes[i] as char).is_ascii_alphanumeric() {
                i += 1;
            }
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 一意な temp path 用のプロセス内カウンタ（同一 pid の並行書き込みでの temp 衝突防止）。
static SECRET_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 秘密（nsec 等）を含むファイルを**作成時から 0600**で、かつ**アトミックに**書く。
///
/// 一意な temp（同ディレクトリ・0600）へ書いてから `rename` で差し替える。これにより
/// 読み手（nostaro）が**部分書き込みの config を絶対に読まない**（partial read → 既定
/// リレーへ publish のような事故を防ぐ）。同一 config への並行書き込みも、最終パスは
/// 常に完全なファイルを指す（内容は決定的で同一）。unix 以外は通常書き込み。
fn write_secret_file(path: &std::path::Path, contents: &str) -> Result<()> {
    let n = SECRET_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), n));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("failed to open secret temp: {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write secret temp: {}", tmp.display()))?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, contents)
            .with_context(|| format!("failed to write secret temp: {}", tmp.display()))?;
    }
    // アトミックに差し替え。失敗したら temp を掃除する。
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to place secret file: {}", path.display()));
    }
    Ok(())
}

/// `nostaro vanity --json` の stdout から鍵を取り出す。進捗ログが混ざりうるので
/// **最後の JSON 行**（`{` 始まり）を採用する。`{"nsec","npub","pubkey"}` を想定。
///
/// **重要**: エラーメッセージに stdout / JSON 行を絶対に載せない。それらは nsec 平文を
/// 含みうる（例: `--json` 非対応版が生鍵を吐く / JSON 破損）。載せると 500 応答やログに
/// 秘密鍵が漏れる。失敗時は固定文言のみを返す。
fn parse_generated_key(stdout: &str) -> Result<GeneratedKey> {
    let line = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('{'))
        .ok_or_else(|| anyhow::anyhow!("nostaro vanity: JSON 出力を解釈できません"))?;
    let v: serde_json::Value = serde_json::from_str(line)
        .map_err(|_| anyhow::anyhow!("nostaro vanity: JSON を解釈できません"))?;
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };
    let nsec = get("nsec");
    if nsec.is_empty() {
        anyhow::bail!("nostaro vanity: nsec が空です");
    }
    Ok(GeneratedKey {
        nsec,
        npub: get("npub"),
        pubkey: get("pubkey"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_dir_isolated_per_agent() {
        let a = NostaroCli::agent_nostr_dir("agent-1").unwrap();
        let b = NostaroCli::agent_nostr_dir("agent-2").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("data/agents/agent-1/nostr"));
        assert!(NostaroCli::agent_config_path("agent-1")
            .unwrap()
            .ends_with("data/agents/agent-1/nostr/config.toml"));
    }

    /// [#299] nostaro は**エージェントの workspace ルート**を cwd にして起動する。
    ///
    /// `execute_shell` / `ws_*` は `data/agents/{id}/workspace` を cwd にしている
    /// （`shell.rs` の `cmd.current_dir(ctx.workspace.root())`）。ここが揃っていないと
    /// `nostr_run event --file <相対>` が ws_write したファイルを見つけられず、
    /// `--out <相対>` の出力も ws_read から見えない。
    #[test]
    fn test_base_command_runs_in_agent_workspace() {
        let cli = NostaroCli::new();
        let agent = "agent-cwd-test";
        let cmd = cli.base_command(agent).unwrap();
        let cwd = cmd
            .as_std()
            .get_current_dir()
            .expect("cwd がエージェント workspace に固定されていない（#299）");
        assert!(
            cwd.ends_with(format!("data/agents/{agent}/workspace")),
            "cwd は execute_shell / ws_* と同じ workspace ルート: {}",
            cwd.display()
        );
        // 相対パスの解決基準がプロセス cwd に左右されないよう絶対パスで渡す。
        assert!(cwd.is_absolute(), "cwd は絶対パス: {}", cwd.display());
        // ディレクトリは用意されている（spawn 時に「そんなディレクトリは無い」で落ちない）。
        assert!(cwd.is_dir(), "workspace ルートが無い: {}", cwd.display());
        let _ = std::fs::remove_dir_all(cli.agent_workspace_dir(agent).unwrap());
    }

    /// [#299] cwd を変えても `--config` が解決できる（＝絶対パスで渡す）。
    ///
    /// config は `data/agents/{id}/nostr/config.toml` とプロセス cwd 基準の相対パスで
    /// 組まれる。cwd を workspace へ移す以上、相対のまま渡すと
    /// `<workspace>/data/agents/...` を探して**必ず見失う**。
    #[test]
    fn test_base_command_passes_absolute_config_path() {
        let cli = NostaroCli::new();
        let agent = "agent-cfgabs-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://relay.test".to_string()], None).unwrap();

        let cmd = cli.base_command(agent).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "--config");
        let config_arg = std::path::PathBuf::from(&args[1]);
        assert!(
            config_arg.is_absolute(),
            "--config は絶対パスで渡す（cwd を変えるため）: {}",
            config_arg.display()
        );
        assert!(
            config_arg.ends_with(format!("data/agents/{agent}/nostr/config.toml")),
            "config は常にこのエージェントのもの: {}",
            config_arg.display()
        );
        // cwd（workspace）を基準にしても、そのパスは実在する config を指す。
        assert!(
            config_arg.is_file(),
            "cwd 変更後も解決できない config パス: {}",
            config_arg.display()
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let _ = std::fs::remove_dir_all(cli.agent_workspace_dir(agent).unwrap());
    }

    /// [#299] cwd の元になる workspace テンプレートは `agent.workspace_path` 由来
    /// （`with_workspace_base` で配線される）。既定値を焼き込まない。
    #[test]
    fn test_workspace_base_is_configurable() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agents/{agent_id}/ws");
        let cli = NostaroCli::new().with_workspace_base(base.to_string_lossy().to_string());
        let cmd = cli.base_command("agent-wsbase").unwrap();
        let cwd = cmd.as_std().get_current_dir().unwrap();
        assert!(
            cwd.ends_with("agents/agent-wsbase/ws"),
            "設定した workspace_base が使われていない: {}",
            cwd.display()
        );
        // 空文字は無視して既定を保つ（他の with_* と同じ扱い）。
        let cli = NostaroCli::new().with_workspace_base("  ");
        let cwd = cli
            .base_command("agent-wsbase2")
            .unwrap()
            .as_std()
            .get_current_dir()
            .unwrap()
            .to_path_buf();
        assert!(cwd.ends_with("data/agents/agent-wsbase2/workspace"));
        let _ = std::fs::remove_dir_all(
            NostaroCli::new()
                .agent_workspace_dir("agent-wsbase2")
                .unwrap(),
        );
    }

    /// workspace ルートを**同名ファイル**で塞ぎ、`create_dir_all` を失敗させる。
    /// degrade 経路（cwd 設定も `--config` 絶対化も見送り）のテスト用。
    fn blocked_workspace_base(dir: &std::path::Path, agent_id: &str) -> String {
        let blocked = dir.join(format!("agents/{agent_id}/ws"));
        std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        // ディレクトリを作らせない：同名の通常ファイルを置く。
        std::fs::write(&blocked, b"not a directory").unwrap();
        dir.join("agents/{agent_id}/ws")
            .to_string_lossy()
            .to_string()
    }

    /// [#301 レビュー] workspace ディレクトリを用意できないときは **cwd を設定しない**。
    ///
    /// ここで `current_dir` だけ設定すると spawn が `ENOENT`/`ENOTDIR` で落ち、
    /// 「nostaro が PATH に無い」場合と同じ文面（`failed to run nostaro: No such file or
    /// directory`）になって切り分け不能になる。cwd を諦めれば #299 修正前の挙動のまま
    /// post/reply/watch/pubkey は動き続ける。
    ///
    /// 併せて **`--config` も従来どおり**（絶対化しない）ことを見る。cwd だけ移して config が
    /// 相対、という中間状態こそ #299 で実測した壊れ方（`CONFIG_MISSING`）なので、
    /// 「両方成功 or 両方見送り」を固定する。
    #[test]
    fn test_base_command_degrades_when_workspace_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let agent = "agent-degrade-base";
        let cli = NostaroCli::new().with_workspace_base(blocked_workspace_base(dir.path(), agent));

        let cmd = cli.base_command(agent).unwrap();
        assert!(
            cmd.as_std().get_current_dir().is_none(),
            "workspace を用意できないのに cwd を設定している（spawn が ENOTDIR で落ちる）: {:?}",
            cmd.as_std().get_current_dir()
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "--config");
        let config_arg = std::path::PathBuf::from(&args[1]);
        assert!(
            !config_arg.is_absolute(),
            "cwd を見送ったのに config だけ絶対化している（片方だけ適用の中間状態）: {}",
            config_arg.display()
        );
        assert_eq!(
            config_arg,
            NostaroCli::agent_config_path(agent).unwrap(),
            "degrade 時の --config は従来どおりのパス"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#301 レビュー] `from`（生成鍵）経路も同じく「両方成功 or 両方見送り」。
    /// `base_command` だけ直して `generated_key_command` が取り残される退行を防ぐ。
    #[test]
    fn test_from_command_degrades_when_workspace_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let agent = "agent-degrade-from";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://relay.test".to_string()], None).unwrap();
        let key = GeneratedKey {
            nsec: "nsec1gen".into(),
            npub: "npub1degrade".into(),
            pubkey: "hex".into(),
        };
        NostaroCli::new().save_generated_key(agent, &key).unwrap();
        let cli = NostaroCli::new().with_workspace_base(blocked_workspace_base(dir.path(), agent));

        let cmd = cli.generated_key_command(agent, "npub1degrade").unwrap();
        assert!(
            cmd.as_std().get_current_dir().is_none(),
            "from 経路が degrade していない（cwd を設定している）"
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "--config");
        let config_arg = std::path::PathBuf::from(&args[1]);
        assert!(
            !config_arg.is_absolute(),
            "from 経路で config だけ絶対化している: {}",
            config_arg.display()
        );
        // #620: from 経路は**鍵行なしの本設定**を --config に使う（from-config は作らない）。
        assert_eq!(
            config_arg,
            NostaroCli::agent_config_path(agent).unwrap(),
            "degrade 時の --config は本設定 config.toml のパス"
        );
        // 生成鍵は env で注入される。
        let env_key = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(SECRET_KEY_ENV))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().to_string());
        assert_eq!(
            env_key.as_deref(),
            Some("nsec1gen"),
            "生成鍵が env に載っていない"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_agent_dir_rejects_traversal_id() {
        // validate_agent_id 経由なので `../` 入りは弾かれる。
        assert!(NostaroCli::agent_nostr_dir("../etc").is_err());
        assert!(NostaroCli::agent_nostr_dir("a/b").is_err());
        assert!(NostaroCli::agent_nostr_dir("").is_err());
    }

    #[test]
    fn test_validate_vanity_prefix() {
        // 空 = ランダム鍵（OK）。
        assert!(validate_vanity_prefix("").is_ok());
        assert!(validate_vanity_prefix("cat").is_ok());
        // bech32 に無い文字（`1` `b` `i` `o`）は拒否。
        assert!(validate_vanity_prefix("1ac").is_err());
        assert!(validate_vanity_prefix("bob").is_err());
        assert!(validate_vanity_prefix("cab").is_err()); // 'b' は bech32 に無い
                                                         // 長さ上限は撤廃した。bech32 charset のみで構成される長い prefix は OK
                                                         // （探索は内部 spawn + キャンセルで途中停止できる）。
        assert!(validate_vanity_prefix("cafe").is_ok()); // c,a,f,e は全て bech32 charset
        assert!(validate_vanity_prefix("crab").is_err()); // 'b' は bech32 に無い
        assert!(validate_vanity_prefix("qqqqqqqq").is_ok()); // 長くても charset OK なら通す
    }

    #[test]
    fn test_save_generated_key_writes_0600_and_sanitizes_name() {
        let key = GeneratedKey {
            nsec: "nsec1secret".to_string(),
            npub: "npub1cat/../x".to_string(), // 異物入り → 英数字のみに安全化
            pubkey: "deadbeef".to_string(),
        };
        let path = NostaroCli::new()
            .save_generated_key("agent-gen-test", &key)
            .unwrap();
        // ファイル名は英数字のみ（`/` や `.` は落ちる）。
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "npub1catx.nsec");
        assert!(path.ends_with("data/agents/agent-gen-test/nostr/generated-keys/npub1catx.nsec"));
        // 中身は nsec、パーミッションは 0600（unix）。
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "nsec1secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// #241: 保存に失敗しても、返るエラーに**鍵の所在（保存先パス）を載せない**。
    /// 成功経路は所在を捨てて npub だけ返すのに、失敗経路の `with_context` がパスを
    /// 載せると保護が失敗時だけ破れ、エラーが `nostr_generate_key` の結果として
    /// そのままエージェントへ渡ってしまう。所在はサーバログにだけ残す。
    #[test]
    fn test_save_generated_key_failure_does_not_leak_path() {
        let agent = "agent-241-save-fail";
        let base = NostaroCli::agent_nostr_dir(agent).unwrap();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // `generated-keys` の位置に**ファイル**を置く → create_dir_all がそこで失敗する。
        let blocker = base.join("generated-keys");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let key = GeneratedKey {
            nsec: "nsec1secret".to_string(),
            npub: "npub1abc".to_string(),
            pubkey: "deadbeef".to_string(),
        };
        let err = NostaroCli::new()
            .save_generated_key(agent, &key)
            .unwrap_err();
        let msg = format!("{err:#}");

        // 所在（保存先ディレクトリ / agent id / `generated-keys` / データパス）が一切載らない。
        assert!(!msg.contains("generated-keys"), "保存先が漏れている: {msg}");
        assert!(
            !msg.contains(agent),
            "agent id 経由で所在が漏れている: {msg}"
        );
        assert!(
            !msg.contains(base.to_string_lossy().as_ref()),
            "保存先パスが漏れている: {msg}"
        );
        // 失敗した事実は返す（エージェントは「保存に失敗した」ことは知ってよい）。
        assert!(msg.contains("失敗"), "失敗である旨は返すべき: {msg}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_list_generated_keys_returns_npubs_only() {
        let agent = "agent-list-keys-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        // 生成前（ディレクトリ未作成）は空一覧。
        assert!(NostaroCli::list_generated_keys(agent).unwrap().is_empty());

        // 複数鍵を保存する。
        for npub in ["npub1alpha", "npub1bravo", "npub1charlie"] {
            NostaroCli::new()
                .save_generated_key(
                    agent,
                    &GeneratedKey {
                        nsec: format!("nsec1secret-{npub}"),
                        npub: npub.to_string(),
                        pubkey: "deadbeef".to_string(),
                    },
                )
                .unwrap();
        }
        // `from` 送信用の一時 config（`.config.toml`）が混ざっていても無視される。
        let dir = NostaroCli::agent_nostr_dir(agent)
            .unwrap()
            .join("generated-keys");
        std::fs::write(
            dir.join("npub1alpha.config.toml"),
            "secret_key = \"nsec1x\"",
        )
        .unwrap();

        let npubs = NostaroCli::list_generated_keys(agent).unwrap();
        assert_eq!(
            npubs,
            vec![
                "npub1alpha".to_string(),
                "npub1bravo".to_string(),
                "npub1charlie".to_string()
            ],
            "npub 一覧（ソート済み）だけが返る"
        );
        // nsec 本文は 1 つも一覧に現れない。
        for entry in &npubs {
            assert!(!entry.contains("nsec"), "nsec が一覧に漏れている: {entry}");
        }

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#265 レビュー堅牢化] ディレクトリ / 非 npub な `.nsec` は列挙しない。
    #[test]
    fn test_list_generated_keys_ignores_dirs_and_non_npub() {
        let agent = "agent-list-keys-robust-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let dir = NostaroCli::agent_nostr_dir(agent)
            .unwrap()
            .join("generated-keys");
        std::fs::create_dir_all(&dir).unwrap();

        // 正規の npub 鍵。
        NostaroCli::new()
            .save_generated_key(
                agent,
                &GeneratedKey {
                    nsec: "nsec1ok".to_string(),
                    npub: "npub1good".to_string(),
                    pubkey: "deadbeef".to_string(),
                },
            )
            .unwrap();
        // `.nsec` 拡張子だが npub でない（hex fallback を模す）→ 除外。
        std::fs::write(dir.join("deadbeefhex.nsec"), "nsec1hex").unwrap();
        // `.nsec` 拡張子のディレクトリ → 通常ファイルでないので除外。
        std::fs::create_dir_all(dir.join("weird.nsec")).unwrap();

        let npubs = NostaroCli::list_generated_keys(agent).unwrap();
        assert_eq!(
            npubs,
            vec!["npub1good".to_string()],
            "npub1 で始まる通常ファイルだけを返す"
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_materialize_config_writes_default_relays_without_secret_key() {
        // nostaro は `relays` と `default_relays` の両方を必須とする（#262）。
        // #620: **secret_key 行は書かない**（鍵は実行時に env で注入）。config を読んでも平文の
        // 鍵が目に入らないことを確認する。
        let agent = "agent-materialize-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let path = NostaroCli::materialize_config(
            agent,
            &[
                "wss://x.kojira.io".to_string(),
                "wss://relay.two".to_string(),
            ],
            None,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // secret_key 行が無い（平文の鍵が config に出ない）。
        assert!(
            !content.contains("secret_key"),
            "config に secret_key が書かれている: {content}"
        );
        assert!(
            !content.contains("nsec1"),
            "config に nsec が出ている: {content}"
        );
        // relays と default_relays の両方が同じリレー集合で書かれる。
        assert!(
            content.contains("relays = [\"wss://x.kojira.io\", \"wss://relay.two\"]"),
            "relays missing: {content}"
        );
        assert!(
            content.contains("default_relays = [\"wss://x.kojira.io\", \"wss://relay.two\"]"),
            "default_relays missing: {content}"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_mask_secrets_redacts_nsec_and_secret_key() {
        // config パース失敗時、nostaro が stderr へエコーする先頭行を模す。
        let stderr = "Error: TOML parse error at line 1, column 1\n  \
                      secret_key = \"nsec1supersecretkeymaterial\"\nmissing field `default_relays`";
        let masked = mask_secrets(stderr);
        assert!(
            !masked.contains("nsec1supersecretkeymaterial"),
            "nsec leaked: {masked}"
        );
        assert!(
            masked.contains("<redacted>"),
            "no redaction marker: {masked}"
        );
        // 非秘密の診断情報は残す。
        assert!(masked.contains("missing field `default_relays`"));
        assert!(masked.contains("TOML parse error"));
        // 行途中に現れる nsec トークンも落とす。
        let inline = mask_secrets("prefix nsec1deadbeefcafe suffix");
        assert!(
            !inline.contains("nsec1deadbeefcafe"),
            "inline leaked: {inline}"
        );
        assert!(inline.contains("nsec1<redacted>"));
        assert!(inline.contains("prefix") && inline.contains("suffix"));
    }

    #[test]
    fn test_mask_secrets_handles_gutter_and_hex_forms() {
        // nostaro 0.3.0 の実際のガター付きエラー出力形（行番号 + `|`）を模す。
        // 値は明らかにダミー（実鍵ではない）。
        let gutter = "error: TOML parse error at line 1, column 1\n  \
                      |\n1 | secret_key = \"nsec1dummydummydummydummydummy\"\n  \
                      | ^^^^^^^^^^^^\nmissing field `default_relays`";
        let masked = mask_secrets(gutter);
        assert!(
            !masked.contains("nsec1dummydummydummydummydummy"),
            "gutter nsec leaked: {masked}"
        );
        // 第1層（行伏せ）で secret_key 行の値そのものが残らない。
        assert!(masked.contains("secret_key = \"<redacted>\""));
        // 行番号ガターなど診断の前置きと、別行の診断情報は保持。
        assert!(masked.contains("1 | secret_key"));
        assert!(masked.contains("missing field `default_relays`"));

        // nsec でない 64hex 秘密（bech32 でないので第2層に掛からない）も第1層で行ごと落とす。
        let hex_secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let hex_line = format!("1 | secret_key = \"{hex_secret}\"");
        let masked_hex = mask_secrets(&hex_line);
        assert!(
            !masked_hex.contains(hex_secret),
            "hex secret leaked: {masked_hex}"
        );
        assert!(masked_hex.contains("secret_key = \"<redacted>\""));

        // `secret_key` を含んでも秘密値でない診断行は潰しすぎるが漏れ側には倒れない。
        // 少なくとも他の診断行は残ることを確認（過剰マスクで全滅しない）。
        let mixed =
            "note: unknown field `foo`\nsecret_key = \"nsec1dummy\"\nhelp: add default_relays";
        let masked_mixed = mask_secrets(mixed);
        assert!(masked_mixed.contains("unknown field `foo`"));
        assert!(masked_mixed.contains("help: add default_relays"));
        assert!(!masked_mixed.contains("nsec1dummy"));
    }

    /// #620: `from`（生成鍵）経路は**鍵行なしの本設定**を `--config` に使い、生成鍵は
    /// env で注入する（平文 from-config は作らない）。本設定の relays を継承する。
    #[test]
    fn test_from_command_uses_main_config_and_injects_generated_key() {
        let cli = NostaroCli::new();
        let agent = "agent-from-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        // 本設定（relays を継承させる・鍵行なし）と生成鍵を用意。
        NostaroCli::materialize_config(agent, &["wss://yabu.me".to_string()], None).unwrap();
        let key = GeneratedKey {
            nsec: "nsec1gen".into(),
            npub: "npub1genkey".into(),
            pubkey: "hex".into(),
        };
        cli.save_generated_key(agent, &key).unwrap();

        let cmd = cli.generated_key_command(agent, "npub1genkey").unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // --config は**本設定 config.toml**（from-config ではない）。
        assert!(
            args.iter()
                .any(|a| a.ends_with(&format!("data/agents/{agent}/nostr/config.toml"))),
            "--config が本設定を指していない: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.contains(".config.toml")),
            "from-config (.config.toml) が使われている（平文経路が残っている）: {args:?}"
        );
        // from-config ファイルは作られない。
        assert!(
            !NostaroCli::agent_nostr_dir(agent)
                .unwrap()
                .join("generated-keys/npub1genkey.config.toml")
                .exists(),
            "平文 from-config が作られている"
        );
        // 生成鍵は env で注入される（本鍵ではなく生成鍵）。
        let env_key = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(SECRET_KEY_ENV))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().to_string());
        assert_eq!(
            env_key.as_deref(),
            Some("nsec1gen"),
            "生成鍵が env に載っていない"
        );
        // [#299] `from` 経路も cwd はエージェント workspace。config は絶対パス。
        let cwd = cmd.as_std().get_current_dir().unwrap();
        assert!(
            cwd.ends_with(format!("data/agents/{agent}/workspace")),
            "from 経路の cwd が workspace でない: {}",
            cwd.display()
        );
        assert!(std::path::Path::new(&args[1]).is_absolute(), "{:?}", args);
        // 存在しない npub は拒否（自分が生成した鍵のみ from 指定可）。
        assert!(cli.generated_key_command(agent, "npub1missing").is_err());
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let _ = std::fs::remove_dir_all(cli.agent_workspace_dir(agent).unwrap());
    }

    /// #620 **鍵混同防止**: base_command（本鍵）と generated_key_command（生成鍵）が
    /// **別々の鍵**を env に載せ、共有点 command_with_config には鍵が載らないこと。
    #[test]
    fn test_base_and_generated_inject_different_keys_no_confusion() {
        let agent = "agent-keymix-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://yabu.me".to_string()], None).unwrap();
        let cli = NostaroCli::new().with_main_key_provider(std::sync::Arc::new(|_id: &str| {
            Ok(Zeroizing::new("nsec1MAINkey".to_string()))
        }));
        cli.save_generated_key(
            agent,
            &GeneratedKey {
                nsec: "nsec1GENkey".into(),
                npub: "npub1genmix".into(),
                pubkey: "hex".into(),
            },
        )
        .unwrap();

        let env_of = |cmd: &Command| -> Option<String> {
            cmd.as_std()
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(SECRET_KEY_ENV))
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().to_string())
        };

        // 本鍵経路（post/reply/pubkey/watch）は本鍵を注入。
        let base = cli.base_command(agent).unwrap();
        assert_eq!(env_of(&base).as_deref(), Some("nsec1MAINkey"));
        // 生成鍵経路（from）は生成鍵を注入（本鍵ではない）。
        let gen = cli.generated_key_command(agent, "npub1genmix").unwrap();
        assert_eq!(env_of(&gen).as_deref(), Some("nsec1GENkey"));
        // 共有点 command_with_config には鍵を差さない（provider があっても）。
        let shared = cli.command_with_config(agent, &NostaroCli::agent_config_path(agent).unwrap());
        assert_eq!(
            env_of(&shared),
            None,
            "共有点に鍵が載っている（一律注入は鍵混同事故になる）"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    #[test]
    fn test_parse_generated_key() {
        // 進捗ログが前に混ざっても最後の JSON 行を採る。
        let out = "searching...\nfound after 1234 tries\n{\"nsec\":\"nsec1abc\",\"npub\":\"npub1crab\",\"pubkey\":\"deadbeef\"}";
        let k = parse_generated_key(out).unwrap();
        assert_eq!(k.nsec, "nsec1abc");
        assert_eq!(k.npub, "npub1crab");
        assert_eq!(k.pubkey, "deadbeef");
        // nsec 空 or JSON 無しはエラー。
        assert!(parse_generated_key("no json here").is_err());
        assert!(parse_generated_key("{\"npub\":\"npub1x\"}").is_err());
    }

    #[test]
    fn test_parse_error_never_leaks_secret() {
        // `--json` 非対応版が生鍵を吐く / JSON 破損時でも、エラー文言に nsec を載せない。
        let leaky_plain = "nsec1supersecretkey npub1pub";
        let e = parse_generated_key(leaky_plain).unwrap_err().to_string();
        assert!(!e.contains("nsec1supersecret"), "plain stdout leaked: {e}");
        let leaky_json = "{\"nsec\":\"nsec1supersecretkey\", BROKEN";
        let e = parse_generated_key(leaky_json).unwrap_err().to_string();
        assert!(!e.contains("nsec1supersecret"), "json line leaked: {e}");
    }

    #[test]
    fn test_watch_command_includes_relays_and_filters() {
        let cli = NostaroCli::new();
        let config = NostrConfig {
            relays: vec![],
            filter: crate::config::NostrFilter {
                authors: vec!["npub1abc".to_string()],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        let cmd = cli.build_watch_command("agent-1", &config).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // 既定リレー2つが = 形式のフラグで渡る（config の default に依存しない）。
        assert!(args.contains(&"--relay=wss://yabu.me".to_string()));
        assert!(args.contains(&"--relay=wss://r.kojira.io".to_string()));
        assert!(args.contains(&"--author=npub1abc".to_string()));
        assert!(args.contains(&"--keyword=opencrab".to_string()));
        // kind 未指定 → 既定 1。
        assert!(args.contains(&"--kind=1".to_string()));
        assert!(args.contains(&"--json".to_string()));
        // 条件の結合は OR を明示する（#278）。
        assert!(args.contains(&"--match=any".to_string()));
        // per-agent config が渡る。
        assert!(args
            .iter()
            .any(|a| a.contains("data/agents/agent-1/nostr/config.toml")));
    }

    /// [#278] 受信セマンティクス（条件の結合方法）は **argv に明示**する。
    ///
    /// nostaro の `--match` 既定は `any` だが、既定に寄りかかると nostaro 側が既定を
    /// 変えた瞬間に opencrab の受信が黙って変わる（#278 が起きた原因そのもの）。
    /// フィルタが空でも `--match=any` が 1 つだけ乗り、`--match=all` は決して乗らない。
    #[test]
    fn test_watch_command_pins_match_mode_to_any() {
        let cli = NostaroCli::new();
        let cmd = cli
            .build_watch_command("agent-1", &NostrConfig::default())
            .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args.iter().filter(|a| a.starts_with("--match")).count(),
            1,
            "--match は 1 回だけ渡す: {args:?}"
        );
        assert!(args.contains(&"--match=any".to_string()), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "--match=all"),
            "AND 結合（--match=all）にしない: {args:?}"
        );
    }

    /// [#271/#278] `--no-mention-only` は**絶対に渡さない**。
    ///
    /// nostaro の mention-only 既定（自分宛の p タグを購読）が、フィルタ未指定でも
    /// 購読を「自分宛のみ」に閉じる唯一の仕組み。ここでそれを切ると全ノート購読
    /// （firehose）になる。フィルタの有無に関わらず渡らないことを固定する。
    #[test]
    fn test_watch_command_never_disables_mention_only() {
        let cli = NostaroCli::new();
        let configs = [
            NostrConfig::default(),
            NostrConfig {
                relays: vec![],
                filter: crate::config::NostrFilter {
                    authors: vec!["npub1abc".to_string()],
                    keywords: vec!["opencrab".to_string()],
                    kinds: vec![1, 7],
                },
            },
        ];
        for config in configs {
            let cmd = cli.build_watch_command("agent-1", &config).unwrap();
            let args: Vec<String> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            assert!(
                !args.iter().any(|a| a.starts_with("--no-mention-only")),
                "mention-only を切ると自分宛以外も流れ込む: {args:?}"
            );
            // `--mention-only` も渡さない（nostaro の既定 true に委ねる。明示すると
            // 将来 `--no-mention-only` と併記したときにパースエラーになる）。
            assert!(
                !args.iter().any(|a| a.starts_with("--mention-only")),
                "mention-only は nostaro の既定に委ねる: {args:?}"
            );
        }
    }

    /// [#271] 自動採用（bootstrap）した設定では `--keyword` が 1 つも乗らない。
    ///
    /// #264 の自己ブートストラップが `keywords=[自分の npub]` を自動設定していたため、
    /// 本文に npub 文字列を含まない e/p タグだけの返信が keyword 条件で落ちていた。
    /// 空フィルタなら keyword フラグは組み立てられない（＝nostaro の mention-only
    /// 既定だけが効く）ことを argv で固定する。
    #[test]
    fn test_watch_command_has_no_keyword_when_filter_is_empty() {
        let cli = NostaroCli::new();
        let cmd = cli
            .build_watch_command("agent-1", &NostrConfig::default())
            .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !args.iter().any(|a| a.starts_with("--keyword")),
            "自動設定の keyword は乗せない: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--author")),
            "自動設定の author は乗せない: {args:?}"
        );
        // 絞り込みが無くても watch は張る（mention-only 既定で「自分宛のみ」）。
        assert!(args.contains(&"--kind=1".to_string()), "{args:?}");
    }

    // ------------------------------------------------------------------
    // nostr_run 薄い passthrough（#268）
    // ------------------------------------------------------------------

    /// 各 argv を 1 行ずつ echo する fake nostaro（引数の中身を検証するため）。
    /// シェルは介さず `"$@"` をそのまま出すので、`;` や空白入りの値も 1 引数として現れる。
    #[cfg(unix)]
    fn fake_echo_nostaro() -> (tempfile::TempDir, NostaroCli) {
        let dir = tempfile::tempdir().unwrap();
        let script = crate::test_support::write_fake_nostaro(
            dir.path(),
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
        );
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// stdout / stderr に nsec を吐く fake nostaro（マスク検証用）。`leak_to_stderr` で
    /// 出力先と終了コードを切り替える（stderr なら exit 1 = 失敗経路）。
    #[cfg(unix)]
    fn fake_leaky_nostaro(leak_to_stderr: bool) -> (tempfile::TempDir, NostaroCli) {
        let dir = tempfile::tempdir().unwrap();
        let body = if leak_to_stderr {
            "#!/bin/sh\nprintf 'secret_key = \"nsec1leakedsecretmaterial\"\\n' 1>&2\nexit 1\n"
        } else {
            "#!/bin/sh\nprintf 'secret_key = \"nsec1leakedsecretmaterial\"\\n'\nexit 0\n"
        };
        let script = crate::test_support::write_fake_nostaro(dir.path(), body);
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// config.toml を materialize して「鍵採用済み」状態を作る。
    fn materialize_for(agent: &str) {
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(agent, &["wss://relay.test".to_string()], None).unwrap();
    }

    /// `init` / `watch` / `relay` / `dm` / `event` は materialize の有無に関わらず**拒否**
    /// （鍵管理・受信・リレー設定・DM 送信・任意 kind publish は passthrough の外）。deny
    /// チェックは config 存在チェックより手前なので nostaro を spawn しない。`relay` は
    /// config.toml だけ書き換わって DB と desync し次の gateway start / switch_identity で
    /// 揮発するため塞ぐ（configure_nostr / ダッシュボードの DB 経路に閉じる）。`dm`（#514）は
    /// `nostr_dm` ツール撤去だけでは `nostr_run dm send` から通ってしまう送信のもう一方の
    /// 経路を塞ぐ。`event`（#514）は `nostaro event -k 4 ...` で DM kind を publish して dm
    /// deny を迂回できる穴を塞ぐ（任意 kind publish の丸ごと拒否）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_denies_init_watch_relay_dm_and_event() {
        let agent = "agent-pt-deny";
        materialize_for(agent);
        let (_d, cli) = fake_echo_nostaro();

        for sub in ["init", "watch", "relay", "dm", "event"] {
            let r = cli.run_passthrough(agent, sub, &[]).await;
            assert!(r.is_err(), "{sub} は拒否されるべき");
            let msg = r.unwrap_err().to_string();
            assert!(msg.contains(sub), "拒否理由に {sub} が含まれること: {msg}");
        }
        // relay の拒否理由には opencrab 側で管理する旨を明示する（誘導）。
        let msg = cli
            .run_passthrough(agent, "relay", &["add".to_string(), "wss://x".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("configure_nostr") || msg.contains("ダッシュボード"),
            "relay の拒否理由に opencrab 側の管理経路を含めること: {msg}"
        );
        // #514: dm の拒否理由には代替（Discord）への誘導を含める。`dm send` の形でも塞ぐ。
        let msg = cli
            .run_passthrough(agent, "dm", &["send".to_string(), "npub1x".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("Discord"),
            "dm の拒否理由に代替（Discord）への誘導を含めること: {msg}"
        );
        // #514: event は DM kind を投げられる形（-k 4 -t p,<pk> -c ...）でも拒否される。
        // deny は subcommand 名で行うので引数の中身に依存しない（表記ゆれの取りこぼしが無い）。
        let msg = cli
            .run_passthrough(
                agent,
                "event",
                &[
                    "-k".to_string(),
                    "4".to_string(),
                    "-t".to_string(),
                    "p,deadbeef".to_string(),
                    "-c".to_string(),
                    "secret".to_string(),
                ],
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("event"),
            "event の拒否理由に event が含まれること: {msg}"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// config 未 materialize（鍵未採用）なら nostaro を spawn せず明示エラー。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_errors_when_config_missing() {
        let agent = "agent-pt-noconfig";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let (_d, cli) = fake_echo_nostaro();

        let r = cli
            .run_passthrough(agent, "post", &["hi".to_string()])
            .await;
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("config.toml") || msg.contains("採用"),
            "未 materialize は明示エラー: {msg}"
        );
    }

    /// 素通しは常に `--config <このエージェントの config>` を前置し、subcommand と args を
    /// **1 argv ずつ**そのまま渡す。`;`・空白入りの値もシェル解釈されず 1 引数として届く
    /// （＝シェルインジェクション不可）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_uses_agent_config_and_passes_args_verbatim() {
        let agent = "agent-pt-args";
        materialize_for(agent);
        let (_d, cli) = fake_echo_nostaro();

        let injection = "hello; rm -rf / && echo pwned".to_string();
        // subcommand は実 deny を通る読み取り系（timeline）にする。#514 で `event` は deny
        // なので、素通しの汎用挙動（config 固定・argv verbatim）の検証には使えない。
        let out = cli
            .run_passthrough(
                agent,
                "timeline",
                &["--limit".to_string(), "5".to_string(), injection.clone()],
            )
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // 先頭は必ず --config <このエージェントの config.toml>。
        assert_eq!(lines[0], "--config");
        assert!(
            lines[1].contains(&format!("data/agents/{agent}/nostr/config.toml")),
            "config は常に ctx.agent_id のもの: {}",
            lines[1]
        );
        assert_eq!(lines[2], "timeline");
        assert_eq!(lines[3], "--limit");
        assert_eq!(lines[4], "5");
        // インジェクション文字列は 1 argv として丸ごと届く（分割・実行されない）。
        assert_eq!(lines[5], injection);
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// args で `--config` を上書きさせない（config 固定＝鍵混同防止を回避されない）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_rejects_config_override() {
        let agent = "agent-pt-cfgoverride";
        materialize_for(agent);
        let (_d, cli) = fake_echo_nostaro();

        for bad in [
            vec!["--config".to_string(), "/etc/other".to_string()],
            vec!["--config=/etc/other".to_string()],
        ] {
            let r = cli.run_passthrough(agent, "get", &bad).await;
            assert!(r.is_err(), "--config 上書きは拒否: {bad:?}");
            assert!(r.unwrap_err().to_string().contains("--config"));
        }
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// 成功時の stdout も nsec マスクを通す（config を表示しうる系のサブコマンドで万一
    /// 秘密が混じっても伏せる / 多層防御 #263）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_masks_nsec_in_stdout() {
        let agent = "agent-pt-stdoutmask";
        materialize_for(agent);
        let (_d, cli) = fake_leaky_nostaro(false);

        let out = cli.run_passthrough(agent, "get", &[]).await.unwrap();
        assert!(
            !out.contains("nsec1leakedsecretmaterial"),
            "stdout に nsec が漏れている: {out}"
        );
        assert!(out.contains("<redacted>"), "マスク痕跡が無い: {out}");
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#299] passthrough で起動した nostaro は**エージェント workspace の中**で動き、
    /// そこにある相対パスのファイルを読める。`--config` は cwd を移しても解決できる。
    ///
    /// `nostr_run <sub> --out <相対>` 等が `ws_write` / `execute_shell` の作ったファイルと
    /// 噛み合うことを、fake nostaro の `pwd` / 相対 `cat` / config 存在チェックで固定する。
    /// subcommand は実 deny を通る読み取り系（get）を使う（#514 で `event` は deny）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_runs_in_agent_workspace() {
        let agent = "agent-pt-cwd";
        materialize_for(agent);
        let cli_probe = NostaroCli::new();
        let ws = cli_probe.agent_workspace_dir(agent).unwrap();
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        // エージェントが ws_write / execute_shell で置いたファイルを模す。
        std::fs::write(ws.join("payload.json"), "MARKER_FROM_WORKSPACE").unwrap();

        // cwd と「相対パスの中身」と「--config が読めるか」を出力する fake nostaro。
        let dir = tempfile::tempdir().unwrap();
        let script = crate::test_support::write_fake_nostaro(
            dir.path(),
            "#!/bin/sh\npwd\ncat payload.json 2>/dev/null || printf 'NO_FILE'\nprintf '\\n'\n\
             if [ -f \"$2\" ]; then printf 'CONFIG_OK\\n'; else printf 'CONFIG_MISSING\\n'; fi\n",
        );
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());

        let out = cli
            .run_passthrough(
                agent,
                "get",
                &["--out".to_string(), "payload.json".to_string()],
            )
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            std::path::Path::new(lines[0]).ends_with(format!("data/agents/{agent}/workspace")),
            "nostaro の cwd が workspace でない: {out}"
        );
        assert_eq!(
            lines[1], "MARKER_FROM_WORKSPACE",
            "workspace 相対のファイルが読めていない: {out}"
        );
        assert_eq!(
            lines[2], "CONFIG_OK",
            "cwd を移したあと --config が解決できていない: {out}"
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 失敗時の stderr（nostaro が config 先頭行をエコーする経路）も nsec を伏せる。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_masks_nsec_in_error_output() {
        let agent = "agent-pt-errmask";
        materialize_for(agent);
        let (_d, cli) = fake_leaky_nostaro(true);

        let r = cli.run_passthrough(agent, "get", &[]).await;
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(
            !msg.contains("nsec1leakedsecretmaterial"),
            "エラー出力に nsec が漏れている: {msg}"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }
}
