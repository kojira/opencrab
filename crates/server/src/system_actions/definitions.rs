impl SystemGatewayActions {
    /// 描画面の有無に依存しないツール定義。
    fn always_own_definitions() -> Vec<GatewayActionDef> {
        vec![
            configure_llm_provider_definition(),
            manage_allowed_commands_definition(),
            #[cfg(feature = "nostr")]
            configure_nostr_definition(),
            configure_self_definition(),
            configure_mcp_server_definition(),
            #[cfg(feature = "nostr")]
            nostr_generate_key_definition(),
            #[cfg(feature = "nostr")]
            nostr_list_keys_definition(),
            #[cfg(feature = "nostr")]
            nostr_switch_identity_definition(),
            spawn_subtask_definition(),
            cancel_subtask_definition(),
            steer_subtask_definition(),
            rebuild_memory_index_definition(),
            report_progress_definition(),
            update_memory_index_config_definition(),
            add_allowed_command_definition(),
            list_allowed_commands_definition(),
            remove_allowed_command_definition(),
            create_skill_definition(),
            update_heartbeat_instructions_definition(),
            read_heartbeat_instructions_definition(),
            #[cfg(feature = "nostr")]
            get_my_nostr_relay_definition(),
            #[cfg(feature = "nostr")]
            set_my_nostr_relay_definition(),
            get_my_heartbeat_definition(),
            set_my_heartbeat_definition(),
            run_my_heartbeat_definition(),
            get_my_schedules_definition(),
            set_my_schedule_definition(),
            update_my_schedule_definition(),
            delete_my_schedule_definition(),
            get_default_subtask_webhook_definition(),
            set_default_subtask_webhook_definition(),
            list_subtask_webhooks_definition(),
            get_default_webhook_definition(),
            set_default_webhook_definition(),
            list_webhooks_definition(),
            request_peer_review_definition(),
        ]
    }
}
