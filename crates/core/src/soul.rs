use serde::{Deserialize, Serialize};

/// Social style based on assertiveness and responsiveness dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SocialStyle {
    /// How assertive the agent is (0.0 = passive, 1.0 = very assertive).
    pub assertiveness: f32,
    /// How emotionally responsive the agent is (0.0 = reserved, 1.0 = very responsive).
    pub responsiveness: f32,
    /// Human-readable style name (e.g., "Analytical", "Driver", "Expressive", "Amiable").
    pub style_name: String,
}

impl Default for SocialStyle {
    fn default() -> Self {
        Self {
            assertiveness: 0.5,
            responsiveness: 0.5,
            style_name: "Balanced".to_string(),
        }
    }
}

/// Big Five personality traits, each scored from 0.0 to 1.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Personality {
    pub openness: f32,
    pub conscientiousness: f32,
    pub extraversion: f32,
    pub agreeableness: f32,
    pub neuroticism: f32,
}

impl Default for Personality {
    fn default() -> Self {
        Self {
            openness: 0.5,
            conscientiousness: 0.5,
            extraversion: 0.5,
            agreeableness: 0.5,
            neuroticism: 0.5,
        }
    }
}

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
    /// Social interaction style.
    pub social_style: SocialStyle,
    /// Big Five personality traits.
    pub personality: Personality,
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
            social_style: SocialStyle::default(),
            personality: Personality::default(),
            thinking_style: ThinkingStyle::default(),
            custom_traits: None,
        }
    }

    /// Build a context string describing this soul for LLM prompts.
    pub fn build_context(&self) -> String {
        let mut ctx = String::new();

        ctx.push_str(&format!("## Persona: {}\n\n", self.persona_name));

        ctx.push_str("### Social Style\n");
        ctx.push_str(&format!(
            "- Style: {} (assertiveness: {:.1}, responsiveness: {:.1})\n\n",
            self.social_style.style_name,
            self.social_style.assertiveness,
            self.social_style.responsiveness,
        ));

        ctx.push_str("### Personality (Big Five)\n");
        ctx.push_str(&format!(
            "- Openness: {:.1}\n- Conscientiousness: {:.1}\n- Extraversion: {:.1}\n- Agreeableness: {:.1}\n- Neuroticism: {:.1}\n\n",
            self.personality.openness,
            self.personality.conscientiousness,
            self.personality.extraversion,
            self.personality.agreeableness,
            self.personality.neuroticism,
        ));

        ctx.push_str("### Thinking Style\n");
        ctx.push_str(&format!(
            "- Primary: {}\n- Secondary: {}\n- {}\n",
            self.thinking_style.primary,
            self.thinking_style.secondary,
            self.thinking_style.description,
        ));

        if let Some(ref traits) = self.custom_traits {
            ctx.push_str("\n### Custom Traits\n");
            ctx.push_str(&format!("{}\n", serde_json::to_string_pretty(traits).unwrap_or_default()));
        }

        ctx
    }
}
