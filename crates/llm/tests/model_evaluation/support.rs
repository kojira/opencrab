
use std::time::Instant;

use opencrab_llm::message::*;
use opencrab_llm::providers::openrouter::OpenRouterProvider;
use opencrab_llm::traits::LlmProvider;

// ==================== Configuration ====================

fn api_key() -> String {
    std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set")
}

fn provider() -> OpenRouterProvider {
    OpenRouterProvider::new(api_key()).with_title("OpenCrab Model Evaluation")
}

/// Read target models from EVAL_MODELS env var.
fn eval_models() -> Vec<String> {
    std::env::var("EVAL_MODELS")
        .unwrap_or_else(|_| {
            "anthropic/claude-sonnet-4-6,openai/gpt-5-mini,google/gemini-2.5-flash".to_string()
        })
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Read evaluator model from EVAL_EVALUATOR env var.
fn eval_evaluator() -> String {
    std::env::var("EVAL_EVALUATOR").unwrap_or_else(|_| "anthropic/claude-sonnet-4-6".to_string())
}

/// Read evaluator's soul/personality from EVAL_SOUL env var.
/// When set, this injects the agent's individuality into the evaluation,
/// making the assessment reflect that agent's unique perspective and biases.
fn eval_soul() -> Option<String> {
    std::env::var("EVAL_SOUL").ok().filter(|s| !s.is_empty())
}

// ==================== Evaluation Prompts ====================

struct EvalPrompt {
    category: &'static str,
    prompt: &'static str,
    system: &'static str,
}

/// Hardcoded evaluation prompts — the ONLY thing that should be hardcoded.
fn evaluation_prompts() -> Vec<EvalPrompt> {
    vec![
        EvalPrompt {
            category: "reasoning",
            prompt: "A farmer has 17 sheep. All but 9 die. How many sheep are left? \
                     Explain your reasoning step by step.",
            system: "You are a helpful assistant. Think carefully and show your reasoning.",
        },
        EvalPrompt {
            category: "reasoning",
            prompt: "If it takes 5 machines 5 minutes to make 5 widgets, \
                     how long would it take 100 machines to make 100 widgets? \
                     Think step by step before giving your answer.",
            system: "You are a helpful assistant. Think carefully and show your reasoning.",
        },
        EvalPrompt {
            category: "creative",
            prompt: "Write a haiku about the feeling of debugging code at 3am. \
                     Make it evocative and original.",
            system: "You are a creative writer. Produce original, evocative writing.",
        },
        EvalPrompt {
            category: "analysis",
            prompt: "Compare the trade-offs between microservices and monolithic architecture \
                     for a startup with 5 engineers building a B2B SaaS product. \
                     Be specific and consider their constraints.",
            system: "You are a senior software architect. Give practical, nuanced advice.",
        },
        EvalPrompt {
            category: "instruction_following",
            prompt: "List exactly 3 benefits of test-driven development. \
                     Format each as a single sentence starting with a number. \
                     Do not add any introduction or conclusion.",
            system: "You are a helpful assistant. Follow instructions precisely.",
        },
    ]
}

// ==================== Evaluator Logic ====================

/// Build the evaluator's system prompt.
/// When EVAL_SOUL is set, the agent's personality is injected,
/// making the evaluation reflect that agent's unique perspective.
fn evaluator_system_prompt(soul: &Option<String>) -> String {
    let base = "\
You are an evaluator of AI model responses. \
You will be given an original prompt and a model's response. \
Evaluate the response on these dimensions:\n\
1. Accuracy: Is the answer correct and factually sound?\n\
2. Relevance: Does it address the prompt directly?\n\
3. Quality: Is it well-written, clear, and appropriately detailed?\n\
4. Instruction following: Did it follow the format/constraints requested?\n\n\
Respond in this exact format (no other text):\n\
ACCURACY: <score 1-10>\n\
RELEVANCE: <score 1-10>\n\
QUALITY: <score 1-10>\n\
INSTRUCTION_FOLLOWING: <score 1-10>\n\
OVERALL: <score 1-10>\n\
EVALUATION: <1-2 sentence free-text evaluation>";

    match soul {
        Some(personality) => format!(
            "あなたの個性:\n{personality}\n\n\
             この個性に基づいて評価してください。あなたの価値観やバイアスを評価に反映させてよい。\n\n\
             {base}"
        ),
        None => base.to_string(),
    }
}

fn build_evaluator_prompt(category: &str, original_prompt: &str, response: &str) -> String {
    format!(
        "Task category: {category}\n\n\
         Original prompt:\n{original_prompt}\n\n\
         Model's response:\n{response}\n\n\
         Please evaluate the response."
    )
}

/// Parsed evaluation result from the evaluator model.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct EvalScores {
    accuracy: f64,
    relevance: f64,
    quality: f64,
    instruction_following: f64,
    overall: f64,
    evaluation_text: String,
}

fn parse_eval_scores(text: &str) -> EvalScores {
    fn extract_score(text: &str, label: &str) -> f64 {
        text.lines()
            .find(|line| line.starts_with(label))
            .and_then(|line| {
                line.split(':')
                    .nth(1)
                    .and_then(|s| s.trim().parse::<f64>().ok())
            })
            .unwrap_or(5.0) // default to mid score if parsing fails
    }

    let evaluation_text = text
        .lines()
        .find(|line| line.starts_with("EVALUATION:"))
        .map(|line| line.trim_start_matches("EVALUATION:").trim().to_string())
        .unwrap_or_else(|| text.to_string());

    EvalScores {
        accuracy: extract_score(text, "ACCURACY:"),
        relevance: extract_score(text, "RELEVANCE:"),
        quality: extract_score(text, "QUALITY:"),
        instruction_following: extract_score(text, "INSTRUCTION_FOLLOWING:"),
        overall: extract_score(text, "OVERALL:"),
        evaluation_text,
    }
}

// ==================== Result Types ====================

#[derive(Debug)]
#[allow(dead_code)]
struct ModelResponse {
    model: String,
    category: String,
    prompt_snippet: String,
    response_text: String,
    latency_ms: u64,
    input_tokens: u32,
    output_tokens: u32,
    total_tokens: u32,
}

#[derive(Debug)]
#[allow(dead_code)]
struct EvaluatedResponse {
    response: ModelResponse,
    scores: EvalScores,
    evaluator_model: String,
}

// ==================== Tests ====================

