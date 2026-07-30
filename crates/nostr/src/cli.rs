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
        let dir = Self::agent_nostr_dir(agent_id)?.join("generated-keys");
        let nsec_path = dir.join(format!("{stem}.nsec"));
        let nsec = std::fs::read_to_string(&nsec_path).map_err(|_| {
            anyhow::anyhow!(
                "指定 npub の鍵が見つかりません（from に指定できるのは、このエージェントが \
                 nostr_generate_key で生成した鍵だけです）"
            )
        })?;
        let nsec = nsec.trim();
        // 本設定を読み、secret_key 行だけ差し替える（relays/blossom を継承）。
        let main_path = Self::agent_config_path(agent_id)?;
        let main_toml = std::fs::read_to_string(&main_path).map_err(|_| {
            anyhow::anyhow!("本設定 (config.toml) がありません。先に Nostr を設定してください")
        })?;
        let from_toml = replace_secret_key_line(&main_toml, nsec);
        let from_config = dir.join(format!("{stem}.config.toml"));
        write_secret_file(&from_config, &from_toml)?;
        let mut cmd = Command::new(&self.binary_path);
        cmd.kill_on_drop(true);
        cmd.arg("--config").arg(&from_config);
        Ok(cmd)
    }

    /// generated key の nsec をサーバ内から読む（**サーバ側専用**。LLM には渡さない）。
    /// identity 乗り換え（本鍵採用）で使う。存在チェック＝「自分が生成した鍵のみ」を担保。
    pub fn read_generated_key(agent_id: &str, npub: &str) -> Result<String> {
        let stem = sanitize_key_stem(npub);
        if stem.is_empty() {
            anyhow::bail!("npub が不正です");
        }
        let path = Self::agent_nostr_dir(agent_id)?
            .join("generated-keys")
            .join(format!("{stem}.nsec"));
        let nsec = std::fs::read_to_string(&path).map_err(|_| {
            anyhow::anyhow!(
                "指定 npub の生成鍵が見つかりません（このエージェントが生成した鍵のみ採用できます）"
            )
        })?;
        Ok(nsec.trim().to_string())
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

    /// `nostaro dm send -- <recipient> "<text>"`（既定 NIP-17）。
    pub async fn dm(
        &self,
        agent_id: &str,
        recipient: &str,
        text: &str,
        from: Option<&str>,
    ) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
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
    /// - `watch`: 受信は per-agent ゲートウェイ管理（bounded フィルタ）が担う。passthrough
    ///   から無制限 watch を上げさせない（かつ長時間ブロックを避ける）。
    /// - `relay`: リレー設定の真実源は opencrab の DB（`agent_nostr_config`）で、config.toml は
    ///   materialize で毎回上書きされる。passthrough から `relay add/remove` すると config.toml
    ///   だけ書き換わって DB と desync し、次の gateway start / switch_identity で黙って揮発する。
    ///   よってリレー管理は opencrab の DB 経路（configure_nostr / ダッシュボード）に閉じる。
    ///   壊れている（揮発する）機能を塞ぐので**非劣化ではない**。
    ///
    /// これ以外のサブコマンドは**そのまま nostaro に委ねる**（Nostr 仕様の判断は
    /// opencrab で再実装せず nostaro に委譲する＝非劣化）。
    pub const PASSTHROUGH_DENIED_SUBCOMMANDS: &'static [&'static str] = &["init", "watch", "relay"];

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
    /// `init`/`watch`/`relay` は拒否し、それ以外は素通しする。config.toml 未 materialize
    /// （鍵未採用）なら nostaro を spawn せず明示エラーを返す。
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
        // deny: 鍵の作成/上書き（init）・無制限受信（watch）・リレー編集（relay）。
        // relay は config.toml だけ書き換えて DB(agent_nostr_config) と desync し次の
        // gateway start / switch_identity で揮発するため塞ぐ。
        if Self::PASSTHROUGH_DENIED_SUBCOMMANDS.contains(&sub) {
            anyhow::bail!(
                "nostr_run では '{sub}' は実行できません（init は nostr_generate_key / \
                 nostr_switch_identity に、watch はゲートウェイ管理に閉じています。リレー設定は \
                 opencrab 側（configure_nostr / ダッシュボード）で管理してください）"
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
        // nostaro 0.3.0 は `relays` と `default_relays` の**両方**を必須フィールドとして
        // 要求する（どちらか一方だけだと `missing field ...` で config パースが失敗し、
        // post/watch/pubkey など Nostr 全操作が止まる。#262）。opencrab は送信/受信リレーを
        // 常にフラグで明示するため両者は同値でよい（config 側 default に依存しない）。
        let mut toml = format!(
            "secret_key = \"{}\"\nrelays = [{}]\ndefault_relays = [{}]\n",
            esc(secret_key),
            relay_list,
            relay_list
        );
        if let Some(b) = blossom_server.filter(|s| !s.is_empty()) {
            toml.push_str(&format!("blossom_server = \"{}\"\n", esc(b)));
        }
        // nsec を含むので 0600 で書く（chmod 前の world-readable 窓を作らない）。
        write_secret_file(&path, &toml)?;
        Ok(path)
    }

    /// LLM が生成した鍵の nsec を**サーバ内に 0600 で保存**する（LLM には返さない）。
    ///
    /// 保存先は per-agent の `data/agents/{id}/nostr/generated-keys/<npub>.nsec`。
    /// ファイル名は npub（無ければ pubkey）を bech32/hex 文字に限定して安全化する
    /// （パストラバーサル/インジェクション防止）。返り値は保存パス。
    pub fn save_generated_key(agent_id: &str, key: &GeneratedKey) -> Result<PathBuf> {
        let dir = Self::agent_nostr_dir(agent_id)?.join("generated-keys");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create key dir: {}", dir.display()))?;
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
        write_secret_file(&path, &key.nsec)?;
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

/// config TOML の `secret_key = "..."` 行だけを差し替える（relays/blossom 等は保つ）。
/// 該当行が無ければ先頭に追加する。値は TOML を壊す文字を除去してから埋め込む。
fn replace_secret_key_line(toml: &str, nsec: &str) -> String {
    let esc: String = nsec
        .chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\n' | '\r'))
        .collect();
    let mut replaced = false;
    let mut out: Vec<String> = toml
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("secret_key") {
                replaced = true;
                format!("secret_key = \"{esc}\"")
            } else {
                l.to_string()
            }
        })
        .collect();
    if !replaced {
        out.insert(0, format!("secret_key = \"{esc}\""));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
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
        let path = NostaroCli::save_generated_key("agent-gen-test", &key).unwrap();
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

    #[test]
    fn test_list_generated_keys_returns_npubs_only() {
        let agent = "agent-list-keys-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        // 生成前（ディレクトリ未作成）は空一覧。
        assert!(NostaroCli::list_generated_keys(agent).unwrap().is_empty());

        // 複数鍵を保存する。
        for npub in ["npub1alpha", "npub1bravo", "npub1charlie"] {
            NostaroCli::save_generated_key(
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
        NostaroCli::save_generated_key(
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
    fn test_replace_secret_key_line() {
        let main =
            "secret_key = \"nsec1old\"\nrelays = [\"wss://a\"]\nblossom_server = \"https://b\"\n";
        let out = replace_secret_key_line(main, "nsec1new");
        assert!(out.contains("secret_key = \"nsec1new\""));
        assert!(!out.contains("nsec1old"));
        // relays/blossom は保持。
        assert!(out.contains("relays = [\"wss://a\"]"));
        assert!(out.contains("blossom_server = \"https://b\""));
        // secret_key 行が無ければ先頭に追加。
        let out2 = replace_secret_key_line("relays = [\"wss://a\"]\n", "nsec1x");
        assert!(out2.starts_with("secret_key = \"nsec1x\""));
        assert!(out2.contains("relays"));
    }

    #[test]
    fn test_materialize_config_writes_default_relays() {
        // nostaro 0.3.0 は `relays` と `default_relays` の両方を必須とする（#262）。
        // materialize_config の出力に両フィールドが含まれ、同値であることを確認する。
        let agent = "agent-materialize-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        let path = NostaroCli::materialize_config(
            agent,
            "nsec1abc",
            &[
                "wss://x.kojira.io".to_string(),
                "wss://relay.two".to_string(),
            ],
            None,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("secret_key = \"nsec1abc\""));
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

    #[test]
    fn test_from_command_uses_generated_key_config() {
        let cli = NostaroCli::new();
        let agent = "agent-from-test";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        // 本設定（relays を継承させる）と生成鍵を用意。
        NostaroCli::materialize_config(agent, "nsec1main", &["wss://yabu.me".to_string()], None)
            .unwrap();
        let key = GeneratedKey {
            nsec: "nsec1gen".into(),
            npub: "npub1genkey".into(),
            pubkey: "hex".into(),
        };
        NostaroCli::save_generated_key(agent, &key).unwrap();

        let cmd = cli.generated_key_command(agent, "npub1genkey").unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // --config が from-config を指す。
        assert!(args
            .iter()
            .any(|a| a.contains("generated-keys/npub1genkey.config.toml")));
        // from-config は生成鍵 + 継承リレー。本鍵は含まない。
        let from_cfg = NostaroCli::agent_nostr_dir(agent)
            .unwrap()
            .join("generated-keys/npub1genkey.config.toml");
        let content = std::fs::read_to_string(&from_cfg).unwrap();
        assert!(content.contains("secret_key = \"nsec1gen\""));
        assert!(content.contains("wss://yabu.me"));
        assert!(!content.contains("nsec1main"));
        // 存在しない npub は拒否（自分が生成した鍵のみ from 指定可）。
        assert!(cli.generated_key_command(agent, "npub1missing").is_err());
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
        // per-agent config が渡る。
        assert!(args
            .iter()
            .any(|a| a.contains("data/agents/agent-1/nostr/config.toml")));
    }

    // ------------------------------------------------------------------
    // nostr_run 薄い passthrough（#268）
    // ------------------------------------------------------------------

    /// 各 argv を 1 行ずつ echo する fake nostaro（引数の中身を検証するため）。
    /// シェルは介さず `"$@"` をそのまま出すので、`;` や空白入りの値も 1 引数として現れる。
    #[cfg(unix)]
    fn fake_echo_nostaro() -> (tempfile::TempDir, NostaroCli) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// stdout / stderr に nsec を吐く fake nostaro（マスク検証用）。`leak_to_stderr` で
    /// 出力先と終了コードを切り替える（stderr なら exit 1 = 失敗経路）。
    #[cfg(unix)]
    fn fake_leaky_nostaro(leak_to_stderr: bool) -> (tempfile::TempDir, NostaroCli) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro.sh");
        let body = if leak_to_stderr {
            "#!/bin/sh\nprintf 'secret_key = \"nsec1leakedsecretmaterial\"\\n' 1>&2\nexit 1\n"
        } else {
            "#!/bin/sh\nprintf 'secret_key = \"nsec1leakedsecretmaterial\"\\n'\nexit 0\n"
        };
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// config.toml を materialize して「鍵採用済み」状態を作る。
    fn materialize_for(agent: &str) {
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::materialize_config(
            agent,
            "nsec1dummy",
            &["wss://relay.test".to_string()],
            None,
        )
        .unwrap();
    }

    /// `init` / `watch` / `relay` は materialize の有無に関わらず**拒否**（鍵管理・受信・
    /// リレー設定は passthrough の外）。deny チェックは config 存在チェックより手前なので
    /// nostaro を spawn しない。`relay` は config.toml だけ書き換わって DB と desync し次の
    /// gateway start / switch_identity で揮発するため塞ぐ（configure_nostr / ダッシュボード
    /// の DB 経路に閉じる）。
    #[cfg(unix)]
    #[tokio::test]
    async fn passthrough_denies_init_and_watch() {
        let agent = "agent-pt-deny";
        materialize_for(agent);
        let (_d, cli) = fake_echo_nostaro();

        for sub in ["init", "watch", "relay"] {
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
        let out = cli
            .run_passthrough(
                agent,
                "event",
                &["--kind".to_string(), "0".to_string(), injection.clone()],
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
        assert_eq!(lines[2], "event");
        assert_eq!(lines[3], "--kind");
        assert_eq!(lines[4], "0");
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
