include!("agents/core.rs");
include!("agents/discord.rs");
include!("agents/memory_index.rs");

#[cfg(test)]
#[path = "agents/extgate_v3_tests.rs"]
mod extgate_v3_tests;
