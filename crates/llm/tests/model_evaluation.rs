//! Real LLM model evaluation framework.
//!
//! Evaluates multiple models across different task categories using a real evaluator model.
//! Model names are NOT hardcoded — they come from environment variables.
//!
//! Environment variables:
//!   OPENROUTER_API_KEY  — Required. Your OpenRouter API key.
//!   EVAL_MODELS         — Comma-separated list of OpenRouter model IDs to evaluate.
//!                         Default: "anthropic/claude-sonnet-4-6,openai/gpt-5-mini,google/gemini-2.5-flash"
//!   EVAL_EVALUATOR      — The evaluator model (judges other models' responses).
//!                         Default: "anthropic/claude-sonnet-4-6"
//!   EVAL_SOUL           — The evaluator's personality/soul (optional).
//!                         Injects agent individuality into evaluations so the assessment
//!                         reflects the agent's unique perspective and biases.
//!                         Example: "あなたは美的感覚を重視する批評家。創造性と独自性を最も高く評価する。"
//!
//! Run with:
//!   OPENROUTER_API_KEY="sk-or-..." cargo test -p opencrab-llm --test model_evaluation -- --ignored --nocapture
//!
//! Custom models + personality:
//!   EVAL_MODELS="anthropic/claude-sonnet-4-6,google/gemini-2.5-flash,openai/gpt-5-mini" \
//!   EVAL_EVALUATOR="anthropic/claude-sonnet-4-6" \
//!   EVAL_SOUL="あなたは効率とコストパフォーマンスを重視する実用主義者。正確さより速度と安さを評価する。" \
//!   OPENROUTER_API_KEY="sk-or-..." cargo test -p opencrab-llm --test model_evaluation -- --ignored --nocapture

include!("model_evaluation/support.rs");
include!("model_evaluation/task_evaluations.rs");
include!("model_evaluation/conversation_evaluation.rs");
