use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 実行環境変数のうち、core プロセス自身が持つ資格情報として**binding から上書きさせない**名前。
///
/// これらは「その binding の持ち主アカウント」ではなく、core が外向きに使うプロセスの資格情報
/// である。binding 宣言で差し替えられると、core の LLM 呼び出しが別アカウントの鍵で走る。
/// それ以外の名前は gateway の領分なので、宣言された値をそのまま渡す（core は解釈しない）。
pub const CORE_OWNED_ENV_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CURSOR_API_KEY",
    "GOOGLE_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// 名前が core 自身の資格情報か（[`CORE_OWNED_ENV_NAMES`]）。
pub fn is_core_owned_env(name: &str) -> bool {
    CORE_OWNED_ENV_NAMES
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(name))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum CommandPermission {
    Owner,
    #[default]
    Agent,
    CoAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandConfig {
    pub name: String,
    #[serde(default)]
    pub permission: CommandPermission,
    pub timeout_secs: Option<u64>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub shell: Option<ShellToolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellToolConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 呼び出し時の `timeout_secs` 引数で指定できる上限（秒）。
    /// LLM 由来の引数を無制限に信用しない（イベント/リソースの占有防止）。
    #[serde(default = "default_max_timeout")]
    pub max_timeout_secs: u64,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub inherit_env: bool,
    /// `inherit_env = false` のときに子へ渡す環境変数。`PATH` は設定に無くても必ず渡す
    /// （[`ShellToolConfig::effective_env_vars`]）。許可コマンドを裸の名前で起動するには
    /// 子の `PATH` が要り、無いと許可済みでも `No such file or directory` になるため。
    #[serde(default = "default_allowed_env_vars")]
    pub allowed_env_vars: Vec<String>,
    /// Deprecated / no longer enforced: execute_shell no longer truncates stdout/stderr
    /// at the source. Full output is preserved and the gateway chunks losslessly.
    /// Kept only for config backward-compatibility (existing TOML files keep parsing).
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
    // NEW: structured command list with permissions
    #[serde(default)]
    pub commands: Vec<CommandConfig>,
    /// D-1091: この binding の gateway が宣言した環境変数（名前→値）。`execute_shell` の子へ
    /// そのまま渡す。core は値も名前も解釈しない。
    ///
    /// 設定ファイルからは設定できない（`serde(skip)`）。gateway が自分の binding について
    /// だけ宣言する値であり、TOML に書けると「どの binding の値か」が失われる。
    #[serde(skip)]
    pub run_env: BTreeMap<String, String>,
}

impl ShellToolConfig {
    /// `inherit_env = false` のときに子へ実際に渡す環境変数名。`allowed_env_vars` に
    /// `PATH` が無ければ先頭へ足す（D-1089）。他の変数は設定どおりに限定したまま
    /// （`env_clear()` 後にこれだけを渡す）。
    pub fn effective_env_vars(&self) -> Vec<String> {
        let mut vars = self.allowed_env_vars.clone();
        if !vars.iter().any(|name| name == "PATH") {
            vars.insert(0, "PATH".to_string());
        }
        vars
    }

    /// Returns all effective commands (merging allowed_commands as Agent-level)
    pub fn effective_commands(&self) -> Vec<CommandConfig> {
        let mut cmds: Vec<CommandConfig> = self.commands.clone();
        for name in &self.allowed_commands {
            if !cmds.iter().any(|c| &c.name == name) {
                cmds.push(CommandConfig {
                    name: name.clone(),
                    permission: CommandPermission::Agent,
                    timeout_secs: None,
                    description: None,
                });
            }
        }
        cmds
    }

    /// debug ログに載せる名前（値は絶対に含めない）。
    ///
    /// ログは子プロセスとは別の読者を持つ。宣言値は binding の資格情報なので、名前だけを出す。
    /// この関数がログ payload の唯一の源であることをテストが固定する。
    pub fn declared_env_log_names(&self) -> Vec<String> {
        self.declared_env_split().0
    }

    /// 宣言された環境変数を「実際に渡す名前」と「core 所有として拒否した名前」に分ける。
    ///
    /// 拒否するのは [`CORE_OWNED_ENV_NAMES`] だけ。値は返さない（名前だけをログに出す）。
    pub fn declared_env_split(&self) -> (Vec<String>, Vec<String>) {
        let mut applied = Vec::new();
        let mut refused = Vec::new();
        for name in self.run_env.keys() {
            if is_core_owned_env(name) || name.trim().is_empty() {
                refused.push(name.clone());
            } else {
                applied.push(name.clone());
            }
        }
        (applied, refused)
    }
}

impl Default for ShellToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_commands: vec![],
            timeout_secs: 120,
            max_timeout_secs: 1800,
            working_dir: None,
            inherit_env: false,
            allowed_env_vars: vec!["PATH".to_string(), "HOME".to_string(), "LANG".to_string()],
            max_output_bytes: 65536,
            commands: vec![],
            run_env: BTreeMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    // 30 は git clone / cargo build / npm install 等の実用コマンドに短すぎ、
    // かつ呼び出し側から延ばす手段が無かった（バックグラウンド起動+ポーリングの
    // 回避策を強いていた）。イベントループはもうブロックされない（#86）ので、
    // 余裕を持たせる。
    120
}
fn default_max_timeout() -> u64 {
    // spawn_subtask の既定 timeout_secs=1800 と揃える。
    1800
}
fn default_max_output() -> usize {
    65536
}
fn default_allowed_env_vars() -> Vec<String> {
    vec!["PATH".to_string(), "HOME".to_string(), "LANG".to_string()]
}
