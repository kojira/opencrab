use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum CommandPermission {
    Owner,
    Agent,
    CoAgent,
}

impl Default for CommandPermission {
    fn default() -> Self {
        CommandPermission::Agent
    }
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
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub inherit_env: bool,
    #[serde(default = "default_allowed_env_vars")]
    pub allowed_env_vars: Vec<String>,
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
    // NEW: structured command list with permissions
    #[serde(default)]
    pub commands: Vec<CommandConfig>,
}

impl ShellToolConfig {
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
}

impl Default for ShellToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_commands: vec![],
            timeout_secs: 30,
            working_dir: None,
            inherit_env: false,
            allowed_env_vars: vec!["PATH".to_string(), "HOME".to_string(), "LANG".to_string()],
            max_output_bytes: 65536,
            commands: vec![],
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    30
}
fn default_max_output() -> usize {
    65536
}
fn default_allowed_env_vars() -> Vec<String> {
    vec!["PATH".to_string(), "HOME".to_string(), "LANG".to_string()]
}
