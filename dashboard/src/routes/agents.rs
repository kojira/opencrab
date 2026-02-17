use dioxus::prelude::*;
use crate::api::{get_agents, get_agent, AgentSummary};
use crate::app::Route;
use crate::components::AgentCard;

#[component]
pub fn Agents() -> Element {
    let agents = use_resource(move || get_agents());

    rsx! {
        div { class: "max-w-7xl mx-auto",
            div { class: "flex items-center justify-between mb-6",
                h1 { class: "text-2xl font-bold text-gray-900 dark:text-white",
                    "Agents"
                }
            }

            match &*agents.read() {
                Some(Ok(agent_list)) => rsx! {
                    if agent_list.is_empty() {
                        div { class: "text-center py-12",
                            p { class: "text-gray-500 dark:text-gray-400 text-lg",
                                "No agents found."
                            }
                            p { class: "text-gray-400 dark:text-gray-500 mt-2",
                                "Create your first agent using the API or CLI."
                            }
                        }
                    } else {
                        div { class: "grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6",
                            for agent in agent_list.iter() {
                                AgentCard { key: "{agent.id}", agent: agent.clone() }
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
        }
    }
}

#[component]
pub fn AgentDetail(id: String) -> Element {
    let id_clone = id.clone();
    let agent = use_resource(move || {
        let id = id_clone.clone();
        async move { get_agent(id).await }
    });

    rsx! {
        div { class: "max-w-4xl mx-auto",
            match &*agent.read() {
                Some(Ok(detail)) => rsx! {
                    // Header
                    div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 mb-6",
                        div { class: "flex items-center space-x-4",
                            div { class: "w-16 h-16 rounded-full bg-blue-500 flex items-center justify-center text-white text-2xl font-bold",
                                "{detail.name.chars().next().unwrap_or('?')}"
                            }
                            div { class: "flex-1",
                                h1 { class: "text-2xl font-bold text-gray-900 dark:text-white",
                                    "{detail.name}"
                                }
                                p { class: "text-gray-500 dark:text-gray-400",
                                    "{detail.persona_name} / {detail.role}"
                                }
                                if let Some(ref org) = detail.organization {
                                    p { class: "text-sm text-gray-400", "{org}" }
                                }
                            }
                        }
                    }

                    // Action buttons
                    div { class: "grid grid-cols-1 md:grid-cols-3 gap-4 mb-6",
                        Link {
                            to: Route::PersonaEdit { id: id.clone() },
                            class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 hover:shadow-lg transition-shadow text-center block",
                            h3 { class: "font-semibold text-gray-900 dark:text-white", "Edit Persona" }
                            p { class: "text-sm text-gray-500", "Personality & thinking style" }
                        }
                        Link {
                            to: Route::Skills {},
                            class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 hover:shadow-lg transition-shadow text-center block",
                            h3 { class: "font-semibold text-gray-900 dark:text-white", "Manage Skills" }
                            p { class: "text-sm text-gray-500", "Enable/disable skills" }
                        }
                        Link {
                            to: Route::Workspace { agent_id: id.clone() },
                            class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 hover:shadow-lg transition-shadow text-center block",
                            h3 { class: "font-semibold text-gray-900 dark:text-white", "Workspace" }
                            p { class: "text-sm text-gray-500", "Browse agent files" }
                        }
                    }

                    // Identity details
                    div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6",
                        h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-4",
                            "Identity"
                        }
                        div { class: "space-y-3",
                            DetailRow { label: "Agent ID", value: detail.id.clone() }
                            DetailRow { label: "Name", value: detail.name.clone() }
                            DetailRow { label: "Role", value: detail.role.clone() }
                            if let Some(ref title) = detail.job_title {
                                DetailRow { label: "Job Title", value: title.clone() }
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
        }
    }
}

#[component]
fn DetailRow(label: &'static str, value: String) -> Element {
    rsx! {
        div { class: "flex items-center",
            span { class: "w-32 text-sm font-medium text-gray-500 dark:text-gray-400", "{label}" }
            span { class: "text-gray-900 dark:text-white", "{value}" }
        }
    }
}
