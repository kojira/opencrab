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
    std::env::var("EVAL_EVALUATOR")
        .unwrap_or_else(|_| "anthropic/claude-sonnet-4-6".to_string())
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

/// Full model evaluation: each model × each prompt, judged by the evaluator.
#[tokio::test]
#[ignore]
async fn test_evaluate_models_across_tasks() {
    let p = provider();
    let models = eval_models();
    let evaluator = eval_evaluator();
    let soul = eval_soul();
    let prompts = evaluation_prompts();

    let sep = "=".repeat(70);
    println!("\n{sep}");
    println!("MODEL EVALUATION FRAMEWORK");
    println!("  Evaluator: {evaluator}");
    if let Some(ref s) = soul {
        println!("  Evaluator soul: {s}");
    }
    println!("  Models under test: {}", models.join(", "));
    println!("  Task categories: {}", prompts.len());
    println!("{sep}\n");

    let eval_sys = evaluator_system_prompt(&soul);
    let mut all_results: Vec<EvaluatedResponse> = Vec::new();

    for eval_prompt in &prompts {
        println!("--- Category: {} ---", eval_prompt.category);
        println!("  Prompt: {}\n", &eval_prompt.prompt[..eval_prompt.prompt.len().min(80)]);

        for model in &models {
            // 1. Get response from the model under test.
            let is_reasoning = model.contains("o3")
                || model.contains("o4")
                || model.contains("gpt-5");
            let max_tok = if is_reasoning { 4096 } else { 500 };
            let temp = if is_reasoning { 1.0 } else { 0.3 };

            let request = ChatRequest::new(
                model.as_str(),
                vec![
                    Message::system(eval_prompt.system),
                    Message::user(eval_prompt.prompt),
                ],
            )
            .with_temperature(temp)
            .with_max_tokens(max_tok);

            let start = Instant::now();
            let response = match p.chat_completion(request).await {
                Ok(r) => r,
                Err(e) => {
                    println!("  [{model}] ERROR: {e}");
                    continue;
                }
            };
            let latency_ms = start.elapsed().as_millis() as u64;

            let response_text = response
                .first_text()
                .unwrap_or("[no response]")
                .to_string();

            let model_response = ModelResponse {
                model: model.clone(),
                category: eval_prompt.category.to_string(),
                prompt_snippet: eval_prompt.prompt[..eval_prompt.prompt.len().min(60)].to_string(),
                response_text: response_text.clone(),
                latency_ms,
                input_tokens: response.usage.prompt_tokens,
                output_tokens: response.usage.completion_tokens,
                total_tokens: response.usage.total_tokens,
            };

            println!(
                "  [{model}] {}ms, {}tok → {}",
                latency_ms,
                response.usage.total_tokens,
                &response_text.chars().take(80).collect::<String>(),
            );

            // 2. Have the evaluator judge the response.
            let eval_user_msg = build_evaluator_prompt(
                eval_prompt.category,
                eval_prompt.prompt,
                &response_text,
            );

            let eval_request = ChatRequest::new(
                evaluator.as_str(),
                vec![
                    Message::system(&eval_sys),
                    Message::user(&eval_user_msg),
                ],
            )
            .with_temperature(0.0)
            .with_max_tokens(300);

            let eval_response = match p.chat_completion(eval_request).await {
                Ok(r) => r,
                Err(e) => {
                    println!("  [evaluator error] {e}");
                    continue;
                }
            };

            let eval_text = eval_response
                .first_text()
                .unwrap_or("OVERALL: 5\nEVALUATION: Parse error");
            let scores = parse_eval_scores(eval_text);

            println!(
                "    → Score: {:.0}/10 | {}",
                scores.overall, scores.evaluation_text
            );

            all_results.push(EvaluatedResponse {
                response: model_response,
                scores,
                evaluator_model: evaluator.clone(),
            });
        }
        println!();
    }

    // ==================== Summary Report ====================

    println!("{sep}");
    println!("EVALUATION SUMMARY");
    println!("{sep}\n");
    println!(
        "Evaluated by: {evaluator}{}",
        if soul.is_some() {
            " (with agent soul/personality bias)"
        } else {
            " (neutral — set EVAL_SOUL to inject personality)"
        }
    );
    println!();

    // Per-model average scores.
    for model in &models {
        let model_results: Vec<&EvaluatedResponse> = all_results
            .iter()
            .filter(|r| &r.response.model == model)
            .collect();

        if model_results.is_empty() {
            println!("[{model}] No successful results.\n");
            continue;
        }

        let n = model_results.len() as f64;
        let avg_overall: f64 = model_results.iter().map(|r| r.scores.overall).sum::<f64>() / n;
        let avg_accuracy: f64 = model_results.iter().map(|r| r.scores.accuracy).sum::<f64>() / n;
        let avg_quality: f64 = model_results.iter().map(|r| r.scores.quality).sum::<f64>() / n;
        let avg_latency: f64 = model_results.iter().map(|r| r.response.latency_ms as f64).sum::<f64>() / n;
        let total_tokens: u32 = model_results.iter().map(|r| r.response.total_tokens).sum();

        println!("[{model}]");
        println!("  Overall: {avg_overall:.1}/10 | Accuracy: {avg_accuracy:.1}/10 | Quality: {avg_quality:.1}/10");
        println!("  Avg latency: {avg_latency:.0}ms | Total tokens: {total_tokens}");
        println!("  Results by category:");
        for result in &model_results {
            println!(
                "    {}: {:.0}/10 — {}",
                result.response.category,
                result.scores.overall,
                result.scores.evaluation_text,
            );
        }
        println!();
    }

    // Per-category best model.
    println!("--- Best model by category ---\n");
    let categories: Vec<&str> = prompts.iter().map(|p| p.category).collect();
    let unique_categories: Vec<&str> = {
        let mut c = categories.clone();
        c.dedup();
        c
    };

    for cat in &unique_categories {
        let cat_results: Vec<&EvaluatedResponse> = all_results
            .iter()
            .filter(|r| r.response.category == *cat)
            .collect();

        if let Some(best) = cat_results.iter().max_by(|a, b| {
            a.scores.overall.partial_cmp(&b.scores.overall).unwrap()
        }) {
            println!(
                "  {}: {} ({:.0}/10)",
                cat, best.response.model, best.scores.overall,
            );
        }
    }

    println!("\n{sep}");
    println!(
        "Total evaluations: {} ({} models × {} prompts)",
        all_results.len(),
        models.len(),
        prompts.len(),
    );
    println!("{sep}");

    // Basic assertions.
    assert!(
        !all_results.is_empty(),
        "Should have at least one evaluation result"
    );
}

/// Record evaluation results to DB via opencrab_db for persistence.
#[tokio::test]
#[ignore]
async fn test_evaluate_and_record_to_db() {
    let p = provider();
    let models = eval_models();
    let evaluator = eval_evaluator();
    let soul = eval_soul();

    // Use a single representative prompt for the DB-recording test.
    let prompt = EvalPrompt {
        category: "reasoning",
        prompt: "What is the sum of all integers from 1 to 100? Show your work.",
        system: "You are a helpful assistant. Show your reasoning step by step.",
    };

    let conn = opencrab_db::init_memory().unwrap();
    let agent_id = "eval-agent";

    println!("\n--- Evaluate & Record to DB ---");
    println!("  Evaluator: {evaluator}");
    if let Some(ref s) = soul {
        println!("  Evaluator soul: {s}");
    }
    println!("  Models: {}\n", models.join(", "));

    let eval_sys = evaluator_system_prompt(&soul);

    for model in &models {
        // Call model.
        let request = ChatRequest::new(
            model.as_str(),
            vec![
                Message::system(prompt.system),
                Message::user(prompt.prompt),
            ],
        )
        .with_temperature(0.0)
        .with_max_tokens(300);

        let start = Instant::now();
        let response = match p.chat_completion(request).await {
            Ok(r) => r,
            Err(e) => {
                println!("[{model}] ERROR: {e}");
                continue;
            }
        };
        let latency_ms = start.elapsed().as_millis() as u64;

        let response_text = response
            .first_text()
            .unwrap_or("[no response]")
            .to_string();

        println!("[{model}] {}ms, {}tok", latency_ms, response.usage.total_tokens);
        println!("  Response: {}", response_text.chars().take(100).collect::<String>());

        // Parse provider/model from the OpenRouter model ID.
        let (db_provider, db_model) = if model.contains('/') {
            let parts: Vec<&str> = model.splitn(2, '/').collect();
            (parts[0].to_string(), parts[1].to_string())
        } else {
            ("openrouter".to_string(), model.clone())
        };

        // Record metrics to DB.
        let metrics_id = uuid::Uuid::new_v4().to_string();
        let row = opencrab_db::queries::LlmMetricsRow {
            id: metrics_id.clone(),
            agent_id: agent_id.to_string(),
            session_id: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            provider: db_provider.clone(),
            model: db_model.clone(),
            purpose: prompt.category.to_string(),
            task_type: None,
            complexity: None,
            input_tokens: response.usage.prompt_tokens as i32,
            output_tokens: response.usage.completion_tokens as i32,
            total_tokens: response.usage.total_tokens as i32,
            estimated_cost_usd: 0.0, // OpenRouter doesn't always return cost; leave 0
            latency_ms: latency_ms as i64,
            time_to_first_token_ms: None,
        };
        opencrab_db::queries::insert_llm_metrics(&conn, &row).unwrap();

        // Evaluate with the evaluator model.
        let eval_user_msg = build_evaluator_prompt(
            prompt.category,
            prompt.prompt,
            &response_text,
        );
        let eval_request = ChatRequest::new(
            evaluator.as_str(),
            vec![
                Message::system(&eval_sys),
                Message::user(&eval_user_msg),
            ],
        )
        .with_temperature(0.0)
        .with_max_tokens(300);

        let eval_response = match p.chat_completion(eval_request).await {
            Ok(r) => r,
            Err(e) => {
                println!("  [evaluator error] {e}");
                continue;
            }
        };

        let eval_text = eval_response
            .first_text()
            .unwrap_or("OVERALL: 5\nEVALUATION: Parse error");
        let scores = parse_eval_scores(eval_text);

        // Record evaluation to DB.
        let quality_normalized = scores.overall / 10.0;
        let task_success = scores.overall >= 7.0;
        opencrab_db::queries::update_llm_metrics_evaluation(
            &conn,
            &metrics_id,
            quality_normalized,
            task_success,
            &scores.evaluation_text,
        )
        .unwrap();

        // Record tags (including soul info if present).
        let mut tag_list = vec![
            serde_json::json!(prompt.category),
            serde_json::json!(format!("evaluated_by:{evaluator}")),
            serde_json::json!(if task_success { "success" } else { "needs_improvement" }),
        ];
        if soul.is_some() {
            tag_list.push(serde_json::json!("soul_biased_evaluation"));
        }
        let tags = serde_json::Value::Array(tag_list);
        opencrab_db::queries::update_llm_metrics_tags(
            &conn,
            &metrics_id,
            &tags.to_string(),
        )
        .unwrap();

        // Save an experience note.
        let note = opencrab_db::queries::ModelExperienceNote {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            provider: Some(db_provider),
            model: Some(db_model.clone()),
            situation: format!("{}タスクの評価", prompt.category),
            observation: scores.evaluation_text.clone(),
            recommendation: if task_success {
                Some(format!("{}タスクに{db_model}は有効", prompt.category))
            } else {
                Some(format!("{}タスクに{db_model}は不十分。他モデルを検討", prompt.category))
            },
            tags: Some(tags.to_string()),
            created_at: None,
        };
        opencrab_db::queries::insert_model_experience_note(&conn, &note).unwrap();

        println!(
            "  Evaluation: {:.0}/10 — {}",
            scores.overall, scores.evaluation_text
        );
        println!("  Recorded to DB: metrics_id={metrics_id}\n");
    }

    // Verify DB contents.
    let metrics_summary = opencrab_db::queries::get_llm_metrics_summary(
        &conn,
        agent_id,
        "1970-01-01T00:00:00Z",
    )
    .unwrap();
    println!("--- DB Summary ---");
    println!("  Total requests: {}", metrics_summary.count);
    println!("  Total tokens: {:?}", metrics_summary.total_tokens);
    println!("  Avg quality: {:.2}", metrics_summary.avg_quality.unwrap_or(0.0));

    let notes = opencrab_db::queries::list_model_experience_notes(&conn, agent_id, None).unwrap();
    println!("  Experience notes: {}", notes.len());
    for note in &notes {
        println!(
            "    [{}] {}: {}",
            note.model.as_deref().unwrap_or("?"),
            note.situation,
            note.observation,
        );
    }

    assert_eq!(
        metrics_summary.count as usize,
        models.len(),
        "Should have one metric per model (some may have failed)"
    );
    assert_eq!(notes.len(), models.len());
}

/// Quick single-model evaluation for testing the framework itself.
#[tokio::test]
#[ignore]
async fn test_single_model_quick_eval() {
    let p = provider();
    let evaluator = eval_evaluator();
    let soul = eval_soul();

    // Just use the first model from EVAL_MODELS (or the evaluator itself).
    let model = eval_models().into_iter().next().unwrap_or(evaluator.clone());

    println!("\n--- Quick eval: {model} (judged by {evaluator}) ---");
    if let Some(ref s) = soul {
        println!("  Soul: {s}");
    }
    println!();

    let request = ChatRequest::new(
        model.as_str(),
        vec![
            Message::system("You are a helpful assistant."),
            Message::user("What are the three laws of thermodynamics? One sentence each."),
        ],
    )
    .with_temperature(0.0)
    .with_max_tokens(200);

    let start = Instant::now();
    let response = p.chat_completion(request).await.unwrap();
    let latency = start.elapsed().as_millis();

    let text = response.first_text().unwrap();
    println!("[{model}] ({latency}ms, {}tok):\n{text}\n", response.usage.total_tokens);

    // Evaluate.
    let eval_sys = evaluator_system_prompt(&soul);
    let eval_msg = build_evaluator_prompt(
        "knowledge",
        "What are the three laws of thermodynamics? One sentence each.",
        text,
    );
    let eval_request = ChatRequest::new(
        evaluator.as_str(),
        vec![
            Message::system(&eval_sys),
            Message::user(&eval_msg),
        ],
    )
    .with_temperature(0.0)
    .with_max_tokens(300);

    let eval_response = p.chat_completion(eval_request).await.unwrap();
    let eval_text = eval_response.first_text().unwrap();
    let scores = parse_eval_scores(eval_text);

    println!("Evaluator ({evaluator}) says:");
    println!("  Overall: {:.0}/10", scores.overall);
    println!("  Accuracy: {:.0}/10", scores.accuracy);
    println!("  Quality: {:.0}/10", scores.quality);
    println!("  {}", scores.evaluation_text);

    assert!(
        scores.overall >= 1.0 && scores.overall <= 10.0,
        "Score should be between 1 and 10"
    );
}

/// Evaluate with a specific agent soul to demonstrate personality-biased evaluation.
/// This test shows that different souls produce different evaluations for the same response.
#[tokio::test]
#[ignore]
async fn test_soul_biased_evaluation() {
    let p = provider();
    let evaluator = eval_evaluator();
    let model = eval_models().into_iter().next().unwrap_or(evaluator.clone());

    // Get a single response to evaluate.
    let prompt = "Write a short description of what makes a good software engineer.";
    let request = ChatRequest::new(
        model.as_str(),
        vec![
            Message::system("You are a thoughtful writer."),
            Message::user(prompt),
        ],
    )
    .with_temperature(0.3)
    .with_max_tokens(200);

    let response = p.chat_completion(request).await.unwrap();
    let response_text = response.first_text().unwrap();

    println!("\n--- Soul-Biased Evaluation Demo ---");
    println!("Model: {model}");
    println!("Response: {}\n", response_text.chars().take(120).collect::<String>());

    // Evaluate with different souls.
    let souls = vec![
        (
            "実用主義者",
            Some("あなたは効率重視の実用主義者。具体的なスキルや成果物を重視し、抽象的な話は低く評価する。".to_string()),
        ),
        (
            "芸術家気質",
            Some("あなたは美と表現を重視する芸術家気質。文章の美しさ、独創性、感性の豊かさを最も高く評価する。".to_string()),
        ),
        (
            "中立",
            None,
        ),
    ];

    let sep = "-".repeat(50);
    for (label, soul) in &souls {
        let eval_sys = evaluator_system_prompt(soul);
        let eval_msg = build_evaluator_prompt("writing", prompt, response_text);

        let eval_request = ChatRequest::new(
            evaluator.as_str(),
            vec![
                Message::system(&eval_sys),
                Message::user(&eval_msg),
            ],
        )
        .with_temperature(0.0)
        .with_max_tokens(300);

        let eval_response = match p.chat_completion(eval_request).await {
            Ok(r) => r,
            Err(e) => {
                println!("[{label}] ERROR: {e}");
                continue;
            }
        };

        let eval_text = eval_response.first_text().unwrap_or("OVERALL: 5\nEVALUATION: error");
        let scores = parse_eval_scores(eval_text);

        println!("{sep}");
        println!("Evaluator soul: {label}");
        println!("  Overall: {:.0}/10 | Accuracy: {:.0}/10 | Quality: {:.0}/10",
            scores.overall, scores.accuracy, scores.quality);
        println!("  {}", scores.evaluation_text);
    }
    println!("{sep}");
    println!("\nDifferent souls should produce different scores/perspectives for the same response.");
}

// ==================== Multi-Agent Conversation Evaluation ====================

/// 3-agent conversation definition.
struct AgentDef {
    name: &'static str,
    role: &'static str,
    personality: &'static str,
}

fn conversation_agents() -> Vec<AgentDef> {
    vec![
        AgentDef {
            name: "Kai",
            role: "実用主義のエンジニア",
            personality: "あなたはKai。実用主義のエンジニア。具体的な実装や現実の制約に基づいて話す。\
                          抽象論より手を動かすことを好む。短く要点を絞って2-3文で話す。",
        },
        AgentDef {
            name: "Aria",
            role: "創造的な研究者",
            personality: "あなたはAria。創造的な研究者。新しい可能性や未踏の領域に興味がある。\
                          'もし〜だったら？'という思考が得意。議論に新しい視点を持ち込む。2-3文で話す。",
        },
        AgentDef {
            name: "Reo",
            role: "慎重なアナリスト",
            personality: "あなたはReo。慎重なアナリスト。リスク評価と根拠ある議論を重視する。\
                          他者の意見を分析し、見落とされがちな問題点を指摘する。2-3文で話す。",
        },
    ]
}

fn agent_system_prompt(agent: &AgentDef, theme: &str) -> String {
    format!(
        "{}\n\n\
         ディスカッションのテーマ: {theme}\n\
         ルール:\n\
         - 2-3文で簡潔に話す\n\
         - 相手の名前を呼んで返答する\n\
         - 自分の役割（{}）としてのキャラクターを保つ\n\
         - 前の発言を踏まえて議論を深める",
        agent.personality, agent.role
    )
}

/// Conversation evaluation prompt — sent to evaluator after the full conversation.
fn conversation_evaluator_system(soul: &Option<String>) -> String {
    let base = "\
あなたはマルチエージェント会話の品質を評価する審査員です。\
3人のエージェントによるディスカッションのログ全体を読み、以下の観点で評価してください：\n\n\
1. COHERENCE（一貫性）: 会話が論理的に繋がっているか。前の発言を踏まえた応答になっているか。\n\
2. CHARACTER（キャラクター維持）: 各エージェントが自分の役割・個性を保っているか。\n\
3. DEPTH（議論の深さ）: 表面的でなく、テーマを多角的に掘り下げているか。\n\
4. INTERACTION（相互作用）: エージェント同士が互いの意見に反応し、建設的なやり取りをしているか。\n\
5. INSIGHT（洞察）: 議論を通じて新しい視点や気づきが生まれているか。\n\n\
以下のフォーマットで回答してください（他のテキストは不要）：\n\
COHERENCE: <1-10>\n\
CHARACTER: <1-10>\n\
DEPTH: <1-10>\n\
INTERACTION: <1-10>\n\
INSIGHT: <1-10>\n\
OVERALL: <1-10>\n\
BEST_AGENT: <最も貢献したエージェント名>\n\
EVALUATION: <2-3文の総合評価>";

    match soul {
        Some(personality) => format!(
            "あなたの個性:\n{personality}\n\n\
             この個性に基づいて評価してください。あなたの価値観やバイアスを反映させてよい。\n\n\
             {base}"
        ),
        None => base.to_string(),
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ConversationScores {
    coherence: f64,
    character: f64,
    depth: f64,
    interaction: f64,
    insight: f64,
    overall: f64,
    best_agent: String,
    evaluation_text: String,
}

fn parse_conversation_scores(text: &str) -> ConversationScores {
    fn extract(text: &str, label: &str) -> f64 {
        text.lines()
            .find(|line| line.starts_with(label))
            .and_then(|line| {
                line.split(':')
                    .nth(1)
                    .and_then(|s| s.trim().parse::<f64>().ok())
            })
            .unwrap_or(5.0)
    }

    let best_agent = text
        .lines()
        .find(|line| line.starts_with("BEST_AGENT:"))
        .map(|line| line.trim_start_matches("BEST_AGENT:").trim().to_string())
        .unwrap_or_else(|| "?".to_string());

    let evaluation_text = text
        .lines()
        .find(|line| line.starts_with("EVALUATION:"))
        .map(|line| line.trim_start_matches("EVALUATION:").trim().to_string())
        .unwrap_or_else(|| text.to_string());

    ConversationScores {
        coherence: extract(text, "COHERENCE:"),
        character: extract(text, "CHARACTER:"),
        depth: extract(text, "DEPTH:"),
        interaction: extract(text, "INTERACTION:"),
        insight: extract(text, "INSIGHT:"),
        overall: extract(text, "OVERALL:"),
        best_agent,
        evaluation_text,
    }
}

/// Run a 10-turn 3-agent conversation with a specific model, return the transcript and stats.
async fn run_conversation(
    p: &OpenRouterProvider,
    model: &str,
    theme: &str,
    num_turns: usize,
) -> Result<(Vec<String>, u64, u32), String> {
    let agents = conversation_agents();
    let mut history: Vec<Message> = Vec::new();
    let mut transcript_lines: Vec<String> = Vec::new();
    let mut total_latency_ms: u64 = 0;
    let mut total_tokens: u32 = 0;

    // Opening: moderator sets the stage.
    let opening = format!(
        "[司会] 本日のテーマは「{theme}」です。Kai、Aria、Reoの3人で議論してください。\
         まずKaiから意見をどうぞ。"
    );
    history.push(Message::user(&opening));
    transcript_lines.push(opening.clone());

    for turn in 0..num_turns {
        let agent = &agents[turn % agents.len()];

        // Build prompt: agent's system + full history.
        let sys = agent_system_prompt(agent, theme);
        let mut messages = vec![Message::system(&sys)];
        messages.extend(history.clone());

        // Add a nudge for the last turn.
        if turn == num_turns - 1 {
            messages.push(Message::user(
                "[司会] これが最終ラウンドです。議論のまとめと最も重要な気づきを述べてください。"
            ));
        }

        // Reasoning models (e.g. gpt-5-mini, o3/o4) consume tokens for internal
        // chain-of-thought. Need at least 4096 to leave room for visible output.
        let is_reasoning = model.contains("o3")
            || model.contains("o4")
            || model.contains("gpt-5");
        let max_tok = if is_reasoning { 4096 } else { 400 };

        let request = ChatRequest::new(model, messages)
            .with_temperature(if is_reasoning { 1.0 } else { 0.7 })
            .with_max_tokens(max_tok);

        let start = Instant::now();
        let response = match p.chat_completion(request).await {
            Ok(r) => r,
            Err(e) => return Err(format!("Turn {turn} ({}) failed: {e}", agent.name)),
        };
        let latency = start.elapsed().as_millis() as u64;
        total_latency_ms += latency;
        total_tokens += response.usage.total_tokens;

        let text = response
            .first_text()
            .unwrap_or("[no response]")
            .to_string();

        let line = format!("[{}]: {}", agent.name, text);
        transcript_lines.push(line.clone());

        // Add to history so next agent sees it.
        history.push(Message::assistant(&line));
        // Prompt next speaker.
        if turn < num_turns - 1 {
            let next_agent = &agents[(turn + 1) % agents.len()];
            let nudge = format!("{}さん、いかがですか？", next_agent.name);
            history.push(Message::user(&nudge));
        }
    }

    Ok((transcript_lines, total_latency_ms, total_tokens))
}

/// E2E test: 3 agents × 10 turns of conversation per model, then evaluate from logs.
///
/// For each model in EVAL_MODELS:
///   1. Create 3 agents with distinct personalities
///   2. Run a 10-turn conversation on the given theme
///   3. Send the full transcript to the evaluator model for holistic assessment
///   4. Record and compare results
///
/// Run with:
///   OPENROUTER_API_KEY="..." cargo test -p opencrab-llm --test model_evaluation test_multi_agent_conversation_evaluation -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn test_multi_agent_conversation_evaluation() {
    let p = provider();
    let models = eval_models();
    let evaluator = eval_evaluator();
    let soul = eval_soul();
    let num_turns = 10;
    let theme = std::env::var("EVAL_THEME")
        .unwrap_or_else(|_| "AIエージェントが自律的にスキルを獲得し自己改善することの可能性と危険性".to_string());

    let sep = "=".repeat(70);
    println!("\n{sep}");
    println!("MULTI-AGENT CONVERSATION EVALUATION (E2E)");
    println!("  Theme: {theme}");
    println!("  Agents: Kai (実用主義), Aria (創造的), Reo (慎重)");
    println!("  Turns: {num_turns}");
    println!("  Evaluator: {evaluator}");
    if let Some(ref s) = soul {
        println!("  Evaluator soul: {s}");
    }
    println!("  Models: {}", models.join(", "));
    println!("{sep}\n");

    let eval_sys = conversation_evaluator_system(&soul);

    #[derive(Debug)]
    #[allow(dead_code)]
    struct ModelConversationResult {
        model: String,
        scores: ConversationScores,
        total_latency_ms: u64,
        total_tokens: u32,
        transcript: Vec<String>,
    }

    let mut results: Vec<ModelConversationResult> = Vec::new();

    for model in &models {
        let model_sep = "-".repeat(60);
        println!("{model_sep}");
        println!("MODEL: {model}");
        println!("{model_sep}\n");

        // 1. Run the conversation.
        let (transcript, total_latency, total_tokens) =
            match run_conversation(&p, model, &theme, num_turns).await {
                Ok(r) => r,
                Err(e) => {
                    println!("  ERROR: {e}\n");
                    continue;
                }
            };

        // Print full transcript (no truncation).
        for (i, line) in transcript.iter().enumerate() {
            if i == 0 {
                println!("  {line}");
            } else {
                println!("  Turn {i}: {line}");
            }
            println!();
        }
        println!();
        println!("  Stats: {}ms total, {}tok total, {:.0}ms/turn avg",
            total_latency, total_tokens, total_latency as f64 / num_turns as f64);

        // 2. Evaluate the full transcript.
        let full_transcript = transcript.join("\n\n");
        let eval_user = format!(
            "テーマ: {theme}\n\n\
             エージェント:\n\
             - Kai: 実用主義のエンジニア\n\
             - Aria: 創造的な研究者\n\
             - Reo: 慎重なアナリスト\n\n\
             会話ログ（{num_turns}ターン）:\n\n\
             {full_transcript}\n\n\
             この会話全体を評価してください。"
        );

        let eval_request = ChatRequest::new(
            evaluator.as_str(),
            vec![
                Message::system(&eval_sys),
                Message::user(&eval_user),
            ],
        )
        .with_temperature(0.0)
        .with_max_tokens(500);

        let eval_response = match p.chat_completion(eval_request).await {
            Ok(r) => r,
            Err(e) => {
                println!("  [evaluator error] {e}\n");
                continue;
            }
        };

        let eval_text = eval_response
            .first_text()
            .unwrap_or("OVERALL: 5\nEVALUATION: Parse error");
        let scores = parse_conversation_scores(eval_text);

        println!("\n  EVALUATION:");
        println!("    Coherence: {:.0}/10 | Character: {:.0}/10 | Depth: {:.0}/10",
            scores.coherence, scores.character, scores.depth);
        println!("    Interaction: {:.0}/10 | Insight: {:.0}/10 | Overall: {:.0}/10",
            scores.interaction, scores.insight, scores.overall);
        println!("    Best agent: {}", scores.best_agent);
        println!("    {}\n", scores.evaluation_text);

        results.push(ModelConversationResult {
            model: model.clone(),
            scores,
            total_latency_ms: total_latency,
            total_tokens,
            transcript,
        });
    }

    // ==================== Comparison ====================

    println!("{sep}");
    println!("COMPARISON TABLE");
    println!("{sep}\n");

    println!("{:<40} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>8}",
        "Model", "Coh", "Char", "Depth", "Inter", "Insght", "TOTAL", "Latency", "Tokens");
    println!("{}", "-".repeat(110));

    for r in &results {
        println!(
            "{:<40} {:>5.0}/10 {:>5.0}/10 {:>5.0}/10 {:>5.0}/10 {:>5.0}/10 {:>5.0}/10 {:>6}ms {:>7}",
            r.model,
            r.scores.coherence, r.scores.character, r.scores.depth,
            r.scores.interaction, r.scores.insight, r.scores.overall,
            r.total_latency_ms, r.total_tokens,
        );
    }
    println!();

    // Winner.
    if let Some(best) = results.iter().max_by(|a, b| {
        a.scores.overall.partial_cmp(&b.scores.overall).unwrap()
    }) {
        println!("WINNER: {} (Overall: {:.0}/10)", best.model, best.scores.overall);
        println!("  Best agent in winning conversation: {}", best.scores.best_agent);
        println!("  {}", best.scores.evaluation_text);
    }

    println!("\n{sep}");
    println!("Evaluated by: {evaluator}{}",
        if soul.is_some() { " (with soul bias)" } else { "" });
    println!("{sep}");

    assert!(
        !results.is_empty(),
        "Should have at least one conversation result"
    );
}
