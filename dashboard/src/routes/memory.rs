use dioxus::prelude::*;
use crate::api::{get_agents, get_curated_memories, search_session_logs, SessionLogDto, get_memory_index_config, update_memory_index_config};

#[component]
pub fn Memory() -> Element {
    let agents = use_resource(move || get_agents());
    let mut selected_agent = use_signal(|| Option::<String>::None);
    let mut search_query = use_signal(|| String::new());
    let mut active_tab = use_signal(|| "curated".to_string());
    let mut config_batch_size = use_signal(|| 50i64);
    let mut config_threshold = use_signal(|| 20i64);
    let mut config_save_status = use_signal(|| Option::<String>::None);
    let mut search_results = use_signal(|| Option::<Result<Vec<SessionLogDto>, String>>::None);

    let agent_id = selected_agent.read().clone();
    let agent_id_for_config = agent_id.clone();

    let curated = use_resource(move || {
        let agent_id = agent_id.clone();
        async move {
            if let Some(id) = agent_id {
                get_curated_memories(id).await
            } else {
                Ok(vec![])
            }
        }
    });

    let config_resource = use_resource(move || {
        let agent_id = agent_id_for_config.clone();
        async move {
            if let Some(id) = agent_id {
                get_memory_index_config(id).await.ok()
            } else {
                None
            }
        }
    });

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "page-title mb-6", "Memory Explorer" }

            // Agent selector
            div { class: "card-elevated mb-6",
                label { class: "block text-label-lg text-on-surface mb-2",
                    span { class: "flex items-center gap-1.5",
                        span { class: "material-symbols-outlined text-lg", "smart_toy" }
                        "Select Agent"
                    }
                }
                match &*agents.read() {
                    Some(Ok(agent_list)) => rsx! {
                        select {
                            class: "select-outlined",
                            onchange: move |e| {
                                let val = e.value();
                                if val.is_empty() {
                                    selected_agent.set(None);
                                } else {
                                    selected_agent.set(Some(val));
                                }
                            },
                            option { value: "", "-- Select an agent --" }
                            for agent in agent_list.iter() {
                                option { value: "{agent.id}", "{agent.name}" }
                            }
                        }
                    },
                    _ => rsx! {
                        p { class: "text-body-md text-on-surface-variant", "Loading agents..." }
                    },
                }
            }

            if selected_agent.read().is_some() {
                // Segmented tab switcher
                div { class: "flex justify-center mb-6",
                    div { class: "segmented-group",
                        button {
                            class: if *active_tab.read() == "curated" { "segmented-btn-active" } else { "segmented-btn" },
                            onclick: move |_| active_tab.set("curated".to_string()),
                            span { class: "material-symbols-outlined text-lg mr-1.5", "auto_awesome" }
                            "Curated Memory"
                        }
                        button {
                            class: if *active_tab.read() == "search" { "segmented-btn-active" } else { "segmented-btn" },
                            onclick: move |_| active_tab.set("search".to_string()),
                            span { class: "material-symbols-outlined text-lg mr-1.5", "search" }
                            "Search Logs"
                        }
                        button {
                            class: if *active_tab.read() == "settings" { "segmented-btn-active" } else { "segmented-btn" },
                            onclick: move |_| {
                                active_tab.set("settings".to_string());
                                // configリソースから値を同期
                                let cfg_read = config_resource.peek();
                                if let Some(Some(ref cfg)) = *cfg_read {
                                    config_batch_size.set(cfg.batch_size);
                                    config_threshold.set(cfg.threshold);
                                }
                            },
                            span { class: "material-symbols-outlined text-lg mr-1.5", "settings" }
                            "Index Settings"
                        }
                    }
                }

                if *active_tab.read() == "curated" {
                    // Curated memory list
                    match &*curated.read() {
                        Some(Ok(memories)) => rsx! {
                            if memories.is_empty() {
                                div { class: "empty-state",
                                    span { class: "material-symbols-outlined empty-state-icon", "memory" }
                                    p { class: "empty-state-text", "No curated memories found." }
                                }
                            } else {
                                div { class: "space-y-3",
                                    for memory in memories.iter() {
                                        div { class: "card-outlined",
                                            div { class: "flex items-center justify-between mb-3",
                                                span { class: "badge-info",
                                                    span { class: "material-symbols-outlined text-sm mr-0.5", "label" }
                                                    "{memory.category}"
                                                }
                                                span { class: "text-label-sm text-on-surface-variant font-mono", "{memory.id}" }
                                            }
                                            p { class: "text-body-lg text-on-surface whitespace-pre-wrap",
                                                "{memory.content}"
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        Some(Err(e)) => rsx! {
                            div { class: "card-outlined border-error bg-error-container/30 p-4",
                                div { class: "flex items-center gap-2",
                                    span { class: "material-symbols-outlined text-error", "error" }
                                    p { class: "text-body-lg text-error-on-container", "Error: {e}" }
                                }
                            }
                        },
                        None => rsx! {
                            div { class: "empty-state",
                                p { class: "text-body-lg text-on-surface-variant", "Loading..." }
                            }
                        },
                    }
                } else if *active_tab.read() == "settings" {
                    div { class: "card-elevated",
                        h2 { class: "text-title-lg text-on-surface mb-4",
                            span { class: "material-symbols-outlined text-xl mr-2 align-middle", "tune" }
                            "Memory Index Configuration"
                        }
                        p { class: "text-body-md text-on-surface-variant mb-6",
                            "Configure compaction batch size and threshold for this agent. Minimum values are enforced automatically."
                        }

                        div { class: "space-y-6",
                            div {
                                label { class: "block text-label-lg text-on-surface mb-1", "Batch Size" }
                                p { class: "text-body-sm text-on-surface-variant mb-2",
                                    "Number of logs to process per index build (min: 10)"
                                }
                                input {
                                    r#type: "number",
                                    class: "input-outlined w-40",
                                    min: "10",
                                    value: "{config_batch_size}",
                                    oninput: move |e| {
                                        if let Ok(v) = e.value().parse::<i64>() {
                                            config_batch_size.set(v);
                                        }
                                    }
                                }
                            }

                            div {
                                label { class: "block text-label-lg text-on-surface mb-1", "Threshold" }
                                p { class: "text-body-sm text-on-surface-variant mb-2",
                                    "Minimum unindexed logs before auto-build triggers (min: 5)"
                                }
                                input {
                                    r#type: "number",
                                    class: "input-outlined w-40",
                                    min: "5",
                                    value: "{config_threshold}",
                                    oninput: move |e| {
                                        if let Ok(v) = e.value().parse::<i64>() {
                                            config_threshold.set(v);
                                        }
                                    }
                                }
                            }

                            div { class: "flex items-center gap-4",
                                button {
                                    class: "btn-filled",
                                    onclick: move |_| {
                                        let agent_id_val = selected_agent.read().clone().unwrap_or_default();
                                        let bs = *config_batch_size.read();
                                        let thr = *config_threshold.read();
                                        spawn(async move {
                                            match update_memory_index_config(agent_id_val, bs, thr).await {
                                                Ok(_) => config_save_status.set(Some("✅ Saved!".to_string())),
                                                Err(e) => config_save_status.set(Some(format!("❌ Error: {e}"))),
                                            }
                                        });
                                    },
                                    span { class: "material-symbols-outlined text-xl", "save" }
                                    "Save Settings"
                                }
                                if let Some(status) = config_save_status.read().as_ref() {
                                    span { class: "text-body-md", "{status}" }
                                }
                            }
                        }
                    }
                } else {
                    // Search interface
                    div { class: "card-elevated mb-6",
                        div { class: "flex gap-3",
                            div { class: "relative flex-1",
                                span { class: "material-symbols-outlined absolute left-3 top-1/2 -translate-y-1/2 text-on-surface-variant",
                                    "search"
                                }
                                input {
                                    r#type: "text",
                                    class: "input-outlined pl-11",
                                    placeholder: "Search session logs...",
                                    value: "{search_query}",
                                    oninput: move |e| search_query.set(e.value())
                                }
                            }
                            button {
                                class: "btn-filled",
                                onclick: move |_| {
                                    let agent_id = selected_agent.read().clone().unwrap_or_default();
                                    let query = search_query.read().clone();
                                    if !query.is_empty() {
                                        spawn(async move {
                                            let result = search_session_logs(agent_id, query).await;
                                            search_results.set(Some(result.map_err(|e| e.to_string())));
                                        });
                                    }
                                },
                                span { class: "material-symbols-outlined text-xl", "search" }
                                "Search"
                            }
                        }
                    }

                    // Search results
                    match &*search_results.read() {
                        Some(Ok(results)) => rsx! {
                            if results.is_empty() {
                                div { class: "empty-state",
                                    span { class: "material-symbols-outlined empty-state-icon", "search_off" }
                                    p { class: "empty-state-text", "No results found." }
                                }
                            } else {
                                div { class: "space-y-3",
                                    p { class: "text-label-lg text-on-surface-variant mb-2",
                                        span { class: "material-symbols-outlined text-lg mr-1 align-middle", "info" }
                                        "{results.len()} result(s) found"
                                    }
                                    for log in results.iter() {
                                        div { class: "card-outlined",
                                            div { class: "flex justify-between mb-2",
                                                div { class: "flex items-center gap-2",
                                                    span { class: "material-symbols-outlined text-lg text-primary", "person" }
                                                    span { class: "text-label-lg text-on-surface",
                                                        "{log.speaker_id.as_deref().unwrap_or(\"unknown\")}"
                                                    }
                                                }
                                                div { class: "flex items-center gap-2",
                                                    span { class: "badge-neutral text-label-sm", "{log.log_type}" }
                                                    span { class: "text-body-sm text-on-surface-variant", "{log.created_at}" }
                                                }
                                            }
                                            p { class: "text-body-lg text-on-surface whitespace-pre-wrap pl-8",
                                                "{log.content}"
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        Some(Err(e)) => rsx! {
                            div { class: "card-outlined border-error bg-error-container/30 p-4",
                                div { class: "flex items-center gap-2",
                                    span { class: "material-symbols-outlined text-error", "error" }
                                    p { class: "text-body-lg text-error-on-container", "Error: {e}" }
                                }
                            }
                        },
                        None => rsx! {},
                    }
                }
            } else {
                div { class: "empty-state",
                    span { class: "material-symbols-outlined empty-state-icon", "memory" }
                    p { class: "empty-state-text", "Select an agent to explore memories" }
                }
            }
        }
    }
}
