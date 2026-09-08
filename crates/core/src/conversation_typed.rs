//! セッションログから導出する、送信経路とは独立した型付き会話 item。
//!
//! PR1 では shadow 観測と回帰固定だけに用い、既存の flat 会話文字列は変更しない。

use std::collections::{HashMap, HashSet};

use opencrab_llm_types::{
    FunctionCall, Message, MessageContent, Role, ToolCall as MessageToolCall,
};
use serde::Serialize;
use serde_json::{Map, Value};

include!("conversation_typed/model_assembly.rs");
include!("conversation_typed/derivation.rs");
include!("conversation_typed/shadow.rs");

#[cfg(test)]
mod tests;
