use dioxus::prelude::*;
use crate::api::AgentSummary;
use crate::app::Route;

#[component]
pub fn AgentCard(agent: AgentSummary) -> Element {
    let (badge_class, status_icon) = match agent.status.as_str() {
        "active" => ("badge-success", "check_circle"),
        "idle" => ("badge-neutral", "schedule"),
        "error" => ("badge-error", "error"),
        _ => ("badge-neutral", "help"),
    };

    let first_char = agent.name.chars().next().unwrap_or('?');

    rsx! {
        Link {
            to: Route::AgentDetail { id: agent.id.clone() },
            class: "card-elevated block group",
            div { class: "flex items-center gap-4 mb-4",
                // Avatar
                if let Some(ref image) = agent.image_url {
                    img {
                        class: "w-12 h-12 rounded-full object-cover",
                        src: "{image}",
                        alt: "{agent.name}"
                    }
                } else {
                    div { class: "w-12 h-12 rounded-full bg-primary-container flex items-center justify-center",
                        span { class: "text-title-md text-primary-on-container font-semibold", "{first_char}" }
                    }
                }

                div { class: "flex-1 min-w-0",
                    h3 { class: "text-title-md text-on-surface group-hover:text-primary transition-colors truncate",
                        "{agent.name}"
                    }
                    p { class: "text-body-sm text-on-surface-variant truncate",
                        "{agent.persona_name}"
                    }
                }

                span { class: "{badge_class}",
                    span { class: "material-symbols-outlined text-sm mr-0.5", "{status_icon}" }
                    "{agent.status}"
                }
            }

            // Stats
            div { class: "flex items-center gap-4 pt-3 border-t border-outline-variant/50",
                div { class: "flex items-center gap-1.5 text-body-sm text-on-surface-variant",
                    span { class: "material-symbols-outlined text-base", "psychology" }
                    span { "{agent.skill_count} skills" }
                }
                div { class: "flex items-center gap-1.5 text-body-sm text-on-surface-variant",
                    span { class: "material-symbols-outlined text-base", "forum" }
                    span { "{agent.session_count} sessions" }
                }
            }
        }
    }
}
