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
        println!(
            "  Prompt: {}\n",
            &eval_prompt.prompt[..eval_prompt.prompt.len().min(80)]
        );

        for model in &models {
            // 1. Get response from the model under test.
            let is_reasoning =
                model.contains("o3") || model.contains("o4") || model.contains("gpt-5");
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

            let response_text = response.first_text().unwrap_or("[no response]").to_string();

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
                response_text.chars().take(80).collect::<String>(),
            );

            // 2. Have the evaluator judge the response.
            let eval_user_msg =
                build_evaluator_prompt(eval_prompt.category, eval_prompt.prompt, &response_text);

            let eval_request = ChatRequest::new(
                evaluator.as_str(),
                vec![Message::system(&eval_sys), Message::user(&eval_user_msg)],
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
        let avg_latency: f64 = model_results
            .iter()
            .map(|r| r.response.latency_ms as f64)
            .sum::<f64>()
            / n;
        let total_tokens: u32 = model_results.iter().map(|r| r.response.total_tokens).sum();

        println!("[{model}]");
        println!("  Overall: {avg_overall:.1}/10 | Accuracy: {avg_accuracy:.1}/10 | Quality: {avg_quality:.1}/10");
        println!("  Avg latency: {avg_latency:.0}ms | Total tokens: {total_tokens}");
        println!("  Results by category:");
        for result in &model_results {
            println!(
                "    {}: {:.0}/10 — {}",
                result.response.category, result.scores.overall, result.scores.evaluation_text,
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

        if let Some(best) = cat_results
            .iter()
            .max_by(|a, b| a.scores.overall.partial_cmp(&b.scores.overall).unwrap())
        {
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
            vec![Message::system(prompt.system), Message::user(prompt.prompt)],
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

        let response_text = response.first_text().unwrap_or("[no response]").to_string();

        println!(
            "[{model}] {}ms, {}tok",
            latency_ms, response.usage.total_tokens
        );
        println!(
            "  Response: {}",
            response_text.chars().take(100).collect::<String>()
        );

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
        let eval_user_msg = build_evaluator_prompt(prompt.category, prompt.prompt, &response_text);
        let eval_request = ChatRequest::new(
            evaluator.as_str(),
            vec![Message::system(&eval_sys), Message::user(&eval_user_msg)],
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
            serde_json::json!(if task_success {
                "success"
            } else {
                "needs_improvement"
            }),
        ];
        if soul.is_some() {
            tag_list.push(serde_json::json!("soul_biased_evaluation"));
        }
        let tags = serde_json::Value::Array(tag_list);
        opencrab_db::queries::update_llm_metrics_tags(&conn, &metrics_id, &tags.to_string())
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
                Some(format!(
                    "{}タスクに{db_model}は不十分。他モデルを検討",
                    prompt.category
                ))
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
    let metrics_summary =
        opencrab_db::queries::get_llm_metrics_summary(&conn, agent_id, "1970-01-01T00:00:00Z")
            .unwrap();
    println!("--- DB Summary ---");
    println!("  Total requests: {}", metrics_summary.count);
    println!("  Total tokens: {:?}", metrics_summary.total_tokens);
    println!(
        "  Avg quality: {:.2}",
        metrics_summary.avg_quality.unwrap_or(0.0)
    );

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
    let model = eval_models()
        .into_iter()
        .next()
        .unwrap_or(evaluator.clone());

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
    println!(
        "[{model}] ({latency}ms, {}tok):\n{text}\n",
        response.usage.total_tokens
    );

    // Evaluate.
    let eval_sys = evaluator_system_prompt(&soul);
    let eval_msg = build_evaluator_prompt(
        "knowledge",
        "What are the three laws of thermodynamics? One sentence each.",
        text,
    );
    let eval_request = ChatRequest::new(
        evaluator.as_str(),
        vec![Message::system(&eval_sys), Message::user(&eval_msg)],
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
    let model = eval_models()
        .into_iter()
        .next()
        .unwrap_or(evaluator.clone());

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
    println!(
        "Response: {}\n",
        response_text.chars().take(120).collect::<String>()
    );

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
            vec![Message::system(&eval_sys), Message::user(&eval_msg)],
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

        let eval_text = eval_response
            .first_text()
            .unwrap_or("OVERALL: 5\nEVALUATION: error");
        let scores = parse_eval_scores(eval_text);

        println!("{sep}");
        println!("Evaluator soul: {label}");
        println!(
            "  Overall: {:.0}/10 | Accuracy: {:.0}/10 | Quality: {:.0}/10",
            scores.overall, scores.accuracy, scores.quality
        );
        println!("  {}", scores.evaluation_text);
    }
    println!("{sep}");
    println!(
        "\nDifferent souls should produce different scores/perspectives for the same response."
    );
}

// ==================== Multi-Agent Conversation Evaluation ====================

