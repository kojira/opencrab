//! nostaro（自作 Nostr CLI）を subprocess 制御するラッパー。
//!
//! codex/cursor プロバイダと同じ「別コマンドを spawn して制御」パターン。鍵の共有
//! 事故を防ぐため、エージェント毎に **一意な config パス**（`data/agents/{id}/nostr/
//! config.toml`）を `--config` で明示指定する（`resolve_agent_workspace` と同じ検証
//! 経路で組む）。リレー/フィルタは watch のフラグで渡し、nostaro の config 側 default
//! に依存しない（指定リレー以外に繋がせない）。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use opencrab_core::workspace::resolve_agent_workspace;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::config::NostrConfig;

const DEFAULT_NOSTARO_PATH: &str = "nostaro";
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// vanity 生成は探索に時間がかかるため、通常操作より長い timeout を使う。
const DEFAULT_VANITY_TIMEOUT_SECS: u64 = 60;

/// vanity prefix に使える文字集合（bech32 小文字。npub の `npub1` 以降に現れる）。
/// `1` `b` `i` `o` は bech32 に存在しないので除外される。
const BECH32_CHARSET: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// vanity prefix の最大長。探索コストは 32^len で指数的に増える。同期リクエストを
/// 長くブロックしないよう保守的に 3 文字（期待 ~32^3≈3万試行＝ほぼ即時）に制限する。
/// より長い vanity は nostaro を直接使う。
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
    if prefix.chars().count() > MAX_VANITY_PREFIX_LEN {
        anyhow::bail!(
            "vanity prefix が長すぎます（最大 {} 文字）。長い prefix は探索が終わりません",
            MAX_VANITY_PREFIX_LEN
        );
    }
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
#[derive(Debug, Clone)]
pub struct NostaroCli {
    binary_path: String,
    timeout: Duration,
    vanity_timeout: Duration,
    /// vanity 生成の同時実行を絞るゲート。`Arc` 共有なので clone 間で同じ制限が効く
    /// （HTTP ルートも LLM ツール経由も同じ 1 本のゲートを通る = 長時間 nostaro
    /// プロセスを並列に溢れさせない）。
    vanity_gate: Arc<Semaphore>,
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
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            vanity_timeout: Duration::from_secs(DEFAULT_VANITY_TIMEOUT_SECS),
            vanity_gate: Arc::new(Semaphore::new(1)),
        }
    }

    pub fn with_binary_path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        if !p.trim().is_empty() {
            self.binary_path = p;
        }
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.timeout = Duration::from_secs(secs);
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

    /// 共通の base command（`nostaro --config <per-agent> <subcommand>...`）。
    fn base_command(&self, agent_id: &str) -> Result<Command> {
        let config_path = Self::agent_config_path(agent_id)?;
        // 親ディレクトリを用意（鍵 config の置き場所）。
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut cmd = Command::new(&self.binary_path);
        cmd.kill_on_drop(true);
        cmd.arg("--config").arg(&config_path);
        Ok(cmd)
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
            anyhow::bail!("nostaro failed ({}): {}", output.status, stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// `nostaro post -- "<text>"` — 新規ノート投稿。
    ///
    /// 全ての positional 引数の前に `--`（オプション終端）を置く。target/text/recipient
    /// 等はモデル/受信イベント由来で、`-` 始まりの値をフラグと誤解釈させない
    /// （引数インジェクション対策）。
    pub async fn post(&self, agent_id: &str, text: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("post").arg("--").arg(text);
        self.run(cmd).await
    }

    /// `nostaro reply -- <target> "<text>"` — 返信。target は note1.../hex id。
    pub async fn reply(&self, agent_id: &str, target: &str, text: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("reply").arg("--").arg(target).arg(text);
        self.run(cmd).await
    }

    /// `nostaro dm send -- <recipient> "<text>"`（既定 NIP-17）。
    pub async fn dm(&self, agent_id: &str, recipient: &str, text: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("dm").arg("send").arg("--").arg(recipient).arg(text);
        self.run(cmd).await
    }

    /// `nostaro zap -m <message> -- <recipient> <amount>`。
    /// `-m` は positional の前（`--` 後に置くと value 扱いされるため）。
    pub async fn zap(
        &self,
        agent_id: &str,
        recipient: &str,
        amount: u64,
        message: Option<&str>,
    ) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("zap");
        if let Some(m) = message.filter(|s| !s.is_empty()) {
            // 値も = 形式で確実に束ねる（`-` 始まりのメッセージ対策）。
            cmd.arg(format!("-m={m}"));
        }
        cmd.arg("--").arg(recipient).arg(amount.to_string());
        self.run(cmd).await
    }

    /// `nostaro upload -- <path>` — Blossom アップロード。返り値は URL。
    pub async fn upload(&self, agent_id: &str, path: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("upload").arg("--").arg(path);
        self.run(cmd).await
    }

    /// `nostaro pubkey` — このエージェント（config）の公開鍵（hex）を返す。
    /// 自分の投稿への自己返信ループを防ぐために使う。
    pub async fn pubkey(&self, agent_id: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
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

    /// per-agent の nostaro config.toml を DB 由来の秘密鍵/リレーから materialize する。
    ///
    /// nsec を含むため**パーミッションを 0600** に落とす（他者に読ませない）。relays は
    /// 送信（post/reply）が publish するリレー。受信は watch のフラグで別途明示する。
    pub fn materialize_config(
        agent_id: &str,
        secret_key: &str,
        relays: &[String],
        blossom_server: Option<&str>,
    ) -> Result<PathBuf> {
        let path = Self::agent_config_path(agent_id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create nostr dir: {}", parent.display()))?;
        }
        // TOML 文字列を壊す/追記させない文字（`"` `\` 改行）を除去する（値は
        // nsec/URL 前提なので本来含まれない。防御的にサニタイズ）。
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
        let mut toml = format!(
            "secret_key = \"{}\"\nrelays = [{}]\n",
            esc(secret_key),
            relay_list
        );
        if let Some(b) = blossom_server.filter(|s| !s.is_empty()) {
            toml.push_str(&format!("blossom_server = \"{}\"\n", esc(b)));
        }
        // nsec を含むので、作成時から 0600 で開く（chmod 前の world-readable 窓を作らない）。
        // 失敗は握りつぶさない。
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("failed to open nostaro config: {}", path.display()))?;
            f.write_all(toml.as_bytes())
                .with_context(|| format!("failed to write nostaro config: {}", path.display()))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, toml)
                .with_context(|| format!("failed to write nostaro config: {}", path.display()))?;
        }
        Ok(path)
    }

    /// watch 用の Command を組む（spawn はループ側が行い、stdout の JSONL を読む）。
    ///
    /// リレー/フィルタは**必ずフラグで明示**して渡す（config の default に依存しない
    /// ＝指定リレー以外へ繋がせない）。`--json` で JSONL を stdout に出させる。
    pub fn build_watch_command(&self, agent_id: &str, config: &NostrConfig) -> Result<Command> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("watch").arg("--json");
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
                                                         // 長すぎ（探索が終わらない）は拒否（cap=3）。
        assert!(validate_vanity_prefix("cafe").is_err());
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
        // per-agent config が渡る。
        assert!(args
            .iter()
            .any(|a| a.contains("data/agents/agent-1/nostr/config.toml")));
    }
}
