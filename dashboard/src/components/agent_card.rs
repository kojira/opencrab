use dioxus::prelude::*;
use crate::api::AgentSummary;
use crate::app::Route;

#[component]
pub fn AgentCard(agent: AgentSummary) -> Element {
    let status_class = match agent.status.as_str() {
        "active" => "bg-green-100 text-green-800",
        "idle" => "bg-gray-100 text-gray-800",
        "error" => "bg-red-100 text-red-800",
        _ => "bg-gray-100 text-gray-800",
    };

    let first_char = agent.name.chars().next().unwrap_or('?');

    rsx! {
        div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 hover:shadow-lg transition-shadow",
            div { class: "flex items-center space-x-4",
                // Avatar
                if let Some(ref image) = agent.image_url {
                    img {
                        class: "w-12 h-12 rounded-full",
                        src: "{image}",
                        alt: "{agent.name}"
                    }
                } else {
                    div { class: "w-12 h-12 rounded-full bg-blue-500 flex items-center justify-center text-white font-bold",
                        "{first_char}"
                    }
                }

                div { class: "flex-1",
                    h3 { class: "text-lg font-semibold text-gray-900 dark:text-white",
                        "{agent.name}"
                    }
                    p { class: "text-sm text-gray-500 dark:text-gray-400",
                        "{agent.persona_name}"
                    }
                }

                // Status badge
                span { class: "px-2 py-1 text-xs rounded-full {status_class}",
                    "{agent.status}"
                }
            }

            // Skill count / Session count
            div { class: "mt-4 flex justify-between text-sm text-gray-500",
                span { "Skills: {agent.skill_count}" }
                span { "Sessions: {agent.session_count}" }
            }

            // Detail link
            Link {
                to: Route::AgentDetail { id: agent.id.clone() },
                class: "mt-4 block text-center text-blue-600 hover:text-blue-800",
                "View Details →"
            }
        }
    }
}
