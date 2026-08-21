use serde::{Deserialize, Serialize};

/// Cognitive thinking style preference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingStyle {
    /// Primary thinking mode (e.g., "Analytical", "Intuitive", "Practical").
    pub primary: String,
    /// Secondary thinking mode.
    pub secondary: String,
    /// Free-form description of this thinking style combination.
    pub description: String,
}

impl Default for ThinkingStyle {
    fn default() -> Self {
        Self {
            primary: "Analytical".to_string(),
            secondary: "Practical".to_string(),
            description: "Balanced analytical and practical thinking".to_string(),
        }
    }
}

/// The soul of an agent: personality, values, and cognitive style.
///
/// This is the deepest layer of an agent's character that shapes how it
/// communicates, reasons, and interacts with others.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Soul {
    /// Display name for this persona.
    pub persona_name: String,
    /// Cognitive style preferences.
    pub thinking_style: ThinkingStyle,
    /// Additional custom traits as key-value pairs.
    pub custom_traits: Option<serde_json::Value>,
}

impl Soul {
    /// Create a new Soul with the given persona name and default traits.
    pub fn new(persona_name: impl Into<String>) -> Self {
        Self {
            persona_name: persona_name.into(),
            thinking_style: ThinkingStyle::default(),
            custom_traits: None,
        }
    }

    /// Build a context string describing this soul for LLM prompts.
    pub fn build_context(&self) -> String {
        let mut ctx = String::new();

        ctx.push_str(&format!("## Persona: {}\n\n", self.persona_name));

        ctx.push_str("### Thinking Style\n");
        ctx.push_str(&format!(
            "- Primary: {}\n- Secondary: {}\n- {}\n",
            self.thinking_style.primary,
            self.thinking_style.secondary,
            self.thinking_style.description,
        ));

        if let Some(ref traits) = self.custom_traits {
            ctx.push_str("\n### Custom Traits\n");
            ctx.push_str(&format!(
                "{}\n",
                serde_json::to_string_pretty(traits).unwrap_or_default()
            ));
        }

        ctx
    }
}
