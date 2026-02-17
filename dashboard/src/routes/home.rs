use dioxus::prelude::*;
use crate::api::{get_agents, get_sessions};
use crate::app::Route;

#[component]
pub fn Home() -> Element {
    let agents = use_resource(move || get_agents());
    let sessions = use_resource(move || get_sessions());

    let agent_count = agents
        .read()
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|a| a.len())
        .unwrap_or(0);

    let session_count = sessions
        .read()
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|s| s.len())
        .unwrap_or(0);

    let active_sessions = sessions
        .read()
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|s| s.iter().filter(|s| s.status == "active").count())
        .unwrap_or(0);

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "text-3xl font-bold text-gray-900 dark:text-white mb-8",
                "Dashboard"
            }

            // Stats cards
            div { class: "grid grid-cols-1 md:grid-cols-3 gap-6 mb-8",
                // Agents
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6",
                    div { class: "flex items-center space-x-4",
                        div { class: "w-12 h-12 rounded-lg bg-blue-100 dark:bg-blue-900 flex items-center justify-center",
                            span { class: "text-2xl font-bold text-blue-600 dark:text-blue-400", "A" }
                        }
                        div {
                            p { class: "text-sm text-gray-500 dark:text-gray-400", "Total Agents" }
                            p { class: "text-2xl font-bold text-gray-900 dark:text-white", "{agent_count}" }
                        }
                    }
                }

                // Sessions
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6",
                    div { class: "flex items-center space-x-4",
                        div { class: "w-12 h-12 rounded-lg bg-green-100 dark:bg-green-900 flex items-center justify-center",
                            span { class: "text-2xl font-bold text-green-600 dark:text-green-400", "S" }
                        }
                        div {
                            p { class: "text-sm text-gray-500 dark:text-gray-400", "Total Sessions" }
                            p { class: "text-2xl font-bold text-gray-900 dark:text-white", "{session_count}" }
                        }
                    }
                }

                // Active sessions
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6",
                    div { class: "flex items-center space-x-4",
                        div { class: "w-12 h-12 rounded-lg bg-purple-100 dark:bg-purple-900 flex items-center justify-center",
                            span { class: "text-2xl font-bold text-purple-600 dark:text-purple-400", "L" }
                        }
                        div {
                            p { class: "text-sm text-gray-500 dark:text-gray-400", "Active Sessions" }
                            p { class: "text-2xl font-bold text-gray-900 dark:text-white", "{active_sessions}" }
                        }
                    }
                }
            }

            // Quick links
            div { class: "grid grid-cols-1 md:grid-cols-2 gap-6",
                Link {
                    to: Route::Agents {},
                    class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 hover:shadow-lg transition-shadow block",
                    h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-2",
                        "Agent Management"
                    }
                    p { class: "text-gray-500 dark:text-gray-400",
                        "Create, configure, and manage autonomous agents"
                    }
                }
                Link {
                    to: Route::Sessions {},
                    class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 hover:shadow-lg transition-shadow block",
                    h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-2",
                        "Session Monitor"
                    }
                    p { class: "text-gray-500 dark:text-gray-400",
                        "Watch real-time conversations and agent interactions"
                    }
                }
                Link {
                    to: Route::Memory {},
                    class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 hover:shadow-lg transition-shadow block",
                    h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-2",
                        "Memory Explorer"
                    }
                    p { class: "text-gray-500 dark:text-gray-400",
                        "Browse and search agent memories and session logs"
                    }
                }
                Link {
                    to: Route::Analytics {},
                    class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 hover:shadow-lg transition-shadow block",
                    h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-2",
                        "Analytics & Metrics"
                    }
                    p { class: "text-gray-500 dark:text-gray-400",
                        "LLM costs, quality scores, and usage analytics"
                    }
                }
            }
        }
    }
}
