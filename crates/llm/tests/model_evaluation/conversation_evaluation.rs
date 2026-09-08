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
                "[司会] これが最終ラウンドです。議論のまとめと最も重要な気づきを述べてください。",
            ));
        }

        // Reasoning models (e.g. gpt-5-mini, o3/o4) consume tokens for internal
        // chain-of-thought. Need at least 4096 to leave room for visible output.
        let is_reasoning = model.contains("o3") || model.contains("o4") || model.contains("gpt-5");
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

        let text = response.first_text().unwrap_or("[no response]").to_string();

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
    let theme = std::env::var("EVAL_THEME").unwrap_or_else(|_| {
        "AIエージェントが自律的にスキルを獲得し自己改善することの可能性と危険性".to_string()
    });

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
        println!(
            "  Stats: {}ms total, {}tok total, {:.0}ms/turn avg",
            total_latency,
            total_tokens,
            total_latency as f64 / num_turns as f64
        );

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
            vec![Message::system(&eval_sys), Message::user(&eval_user)],
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
        println!(
            "    Coherence: {:.0}/10 | Character: {:.0}/10 | Depth: {:.0}/10",
            scores.coherence, scores.character, scores.depth
        );
        println!(
            "    Interaction: {:.0}/10 | Insight: {:.0}/10 | Overall: {:.0}/10",
            scores.interaction, scores.insight, scores.overall
        );
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

    println!(
        "{:<40} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>8}",
        "Model", "Coh", "Char", "Depth", "Inter", "Insght", "TOTAL", "Latency", "Tokens"
    );
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
    if let Some(best) = results
        .iter()
        .max_by(|a, b| a.scores.overall.partial_cmp(&b.scores.overall).unwrap())
    {
        println!(
            "WINNER: {} (Overall: {:.0}/10)",
            best.model, best.scores.overall
        );
        println!(
            "  Best agent in winning conversation: {}",
            best.scores.best_agent
        );
        println!("  {}", best.scores.evaluation_text);
    }

    println!("\n{sep}");
    println!(
        "Evaluated by: {evaluator}{}",
        if soul.is_some() {
            " (with soul bias)"
        } else {
            ""
        }
    );
    println!("{sep}");

    assert!(
        !results.is_empty(),
        "Should have at least one conversation result"
    );
}
