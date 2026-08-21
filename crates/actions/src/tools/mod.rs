pub mod config;
pub mod shell;

pub use config::{ShellToolConfig, ToolsConfig};

use crate::dispatcher::ActionDispatcher;
use std::sync::Arc;

/// Config駆動でアクションを登録する
pub fn register_tools_from_config(config: &ToolsConfig, dispatcher: &mut ActionDispatcher) {
    if !config.enabled {
        return;
    }

    if let Some(shell_config) = &config.shell {
        if shell_config.enabled {
            dispatcher.register(Arc::new(shell::ShellToolAction::new(shell_config.clone())));
        }
    }
}
