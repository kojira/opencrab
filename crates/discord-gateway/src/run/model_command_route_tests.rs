use super::*;

#[test]
fn model_command_is_answered_only_in_a_configured_channel() {
    let configured = [
        "discord-agent-a-10-200".to_string(),
        "discord-agent-a-10-300".to_string(),
    ];

    assert_eq!(
        model_command_address("discord-agent-a-10-300", &configured),
        Some("discord-agent-a-10-300")
    );
    // Another gateway sharing the Discord application owns this channel; stay silent.
    assert_eq!(
        model_command_address("discord-agent-a-10-999", &configured),
        None
    );
}

#[test]
fn model_command_never_borrows_another_agents_channel() {
    let configured = ["discord-agent-b-10-100".to_string()];
    let requested = address_for("agent-a", "10", "100");

    assert_eq!(model_command_address(&requested, &configured), None);
}
