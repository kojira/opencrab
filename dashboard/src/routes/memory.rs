use dioxus::prelude::*;
use crate::api::{get_agents, get_curated_memories, search_session_logs};

#[component]
pub fn Memory() -> Element {
    let agents = use_resource(move || get_agents());
    let mut selected_agent = use_signal(|| Option::<String>::None);
    let mut search_query = use_signal(|| String::new());
    let mut active_tab = use_signal(|| "curated".to_string());

    let agent_id = selected_agent.read().clone();

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

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-6",
                "Memory Explorer"
            }

            // Agent selector
            div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 mb-6",
                label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-2",
                    "Select Agent"
                }
                match &*agents.read() {
                    Some(Ok(agent_list)) => rsx! {
                        select {
                            class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
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
                        p { class: "text-gray-500", "Loading agents..." }
                    },
                }
            }

            if selected_agent.read().is_some() {
                // Tab switcher
                div { class: "flex space-x-1 mb-6",
                    button {
                        class: if *active_tab.read() == "curated" {
                            "px-4 py-2 rounded-lg bg-blue-600 text-white font-medium"
                        } else {
                            "px-4 py-2 rounded-lg bg-gray-200 dark:bg-gray-700 text-gray-700 dark:text-gray-300 font-medium"
                        },
                        onclick: move |_| active_tab.set("curated".to_string()),
                        "Curated Memory"
                    }
                    button {
                        class: if *active_tab.read() == "search" {
                            "px-4 py-2 rounded-lg bg-blue-600 text-white font-medium"
                        } else {
                            "px-4 py-2 rounded-lg bg-gray-200 dark:bg-gray-700 text-gray-700 dark:text-gray-300 font-medium"
                        },
                        onclick: move |_| active_tab.set("search".to_string()),
                        "Search Logs"
                    }
                }

                if *active_tab.read() == "curated" {
                    // Curated memory list
                    match &*curated.read() {
                        Some(Ok(memories)) => rsx! {
                            if memories.is_empty() {
                                div { class: "text-center py-12",
                                    p { class: "text-gray-500", "No curated memories found." }
                                }
                            } else {
                                div { class: "space-y-4",
                                    for memory in memories.iter() {
                                        div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4",
                                            div { class: "flex items-center justify-between mb-2",
                                                span { class: "px-2 py-1 text-xs rounded-full bg-blue-100 text-blue-800 font-medium",
                                                    "{memory.category}"
                                                }
                                                span { class: "text-xs text-gray-400", "{memory.id}" }
                                            }
                                            p { class: "text-gray-900 dark:text-white whitespace-pre-wrap",
                                                "{memory.content}"
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        Some(Err(e)) => rsx! {
                            div { class: "bg-red-50 border border-red-200 rounded-lg p-4",
                                p { class: "text-red-800", "Error: {e}" }
                            }
                        },
                        None => rsx! {
                            div { class: "text-center py-12",
                                p { class: "text-gray-500", "Loading..." }
                            }
                        },
                    }
                } else {
                    // Search interface
                    div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 mb-6",
                        div { class: "flex space-x-2",
                            input {
                                r#type: "text",
                                class: "flex-1 px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                                placeholder: "Search session logs...",
                                value: "{search_query}",
                                oninput: move |e| search_query.set(e.value())
                            }
                            button {
                                class: "px-6 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700",
                                onclick: move |_| {
                                    let agent_id = selected_agent.read().clone().unwrap_or_default();
                                    let query = search_query.read().clone();
                                    if !query.is_empty() {
                                        spawn(async move {
                                            let _result = search_session_logs(agent_id, query).await;
                                        });
                                    }
                                },
                                "Search"
                            }
                        }
                    }
                }
            }
        }
    }
}
