use super::*;
use std::collections::BTreeSet;

fn selected_acknowledged_address(candidates: Vec<String>, acknowledged: &[&str]) -> Option<String> {
    let acknowledged = acknowledged.iter().copied().collect::<BTreeSet<_>>();
    candidates
        .into_iter()
        .find(|address| acknowledged.contains(address.as_str()))
}

#[test]
fn owner_model_actions_use_a_same_agent_binding_from_an_unbound_channel() {
    let agent = "agent-a";
    let configured = [
        "discord-agent-a-10-300".to_string(),
        "discord-agent-a-10-200".to_string(),
    ];
    let acknowledged = ["discord-agent-a-10-200", "discord-agent-a-10-300"];
    let event = |autocomplete, subcommand: &str, model: Option<&str>| ModelInteractionEvent {
        event_kind: "model_interaction".to_string(),
        interaction_id: "1".to_string(),
        application_id: "2".to_string(),
        token: "test-token".to_string(),
        guild_id: Some("10".to_string()),
        channel_id: "999".to_string(),
        user_id: "100".to_string(),
        autocomplete,
        subcommand: subcommand.to_string(),
        model: model.map(str::to_string),
    };
    let events = [
        event(true, "set", Some("")),
        event(false, "list", None),
        event(false, "set", Some("openai:gpt-6-sol")),
        event(false, "reset", None),
    ];

    for event in events {
        let requested = address_for(
            agent,
            event.guild_id.as_deref().unwrap_or_default(),
            &event.channel_id,
        );
        let selected = selected_acknowledged_address(
            model_command_transport_candidates(agent, &requested, &SaidCaller::Owner, &configured),
            &acknowledged,
        );
        assert_eq!(
            selected.as_deref(),
            Some("discord-agent-a-10-200"),
            "{} must use the deterministic same-agent transport",
            if event.autocomplete {
                "autocomplete"
            } else {
                event.subcommand.as_str()
            }
        );
    }
}

#[test]
fn model_command_transport_prefers_the_exact_acknowledged_binding() {
    let requested = "discord-agent-a-10-999";
    let candidates = model_command_transport_candidates(
        "agent-a",
        requested,
        &SaidCaller::Owner,
        &["discord-agent-a-10-200".to_string()],
    );

    assert_eq!(
        selected_acknowledged_address(candidates, &[requested]).as_deref(),
        Some(requested)
    );
}

#[test]
fn model_command_fallback_never_crosses_agents_or_serves_non_owners() {
    let requested = "discord-agent-a-10-999";
    let configured = [
        "discord-agent-b-10-100".to_string(),
        "discord-agent-a-10-200".to_string(),
    ];

    let owner =
        model_command_transport_candidates("agent-a", requested, &SaidCaller::Owner, &configured);
    assert_eq!(
        selected_acknowledged_address(owner, &["discord-agent-b-10-100"]).as_deref(),
        None
    );

    let non_owner = model_command_transport_candidates(
        "agent-a",
        requested,
        &SaidCaller::TrustedUser,
        &configured,
    );
    assert_eq!(non_owner, [requested]);
}
