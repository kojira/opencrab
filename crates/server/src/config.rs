use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use opencrab_llm::providers::*;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;

include!("config/base.rs");
include!("config/maintenance.rs");
include!("config/llm_gateway_database.rs");
include!("config/loading.rs");
include!("config/overrides.rs");
include!("config/router.rs");

#[cfg(test)]
mod tests {
    use super::*;

    include!("config/tests/support_loading.rs");
    include!("config/tests/overrides.rs");
    include!("config/tests/parsing_defaults_webhooks.rs");
    include!("config/tests/router_providers.rs");
}
