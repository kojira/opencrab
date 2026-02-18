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
            h1 { class: "page-title mb-8", "Dashboard" }

            // Stat cards
            div { class: "grid grid-cols-1 md:grid-cols-3 gap-6 mb-8",
                StatCard {
                    icon: "smart_toy",
                    icon_bg: "bg-primary-container",
                    icon_color: "text-primary",
                    label: "Total Agents",
                    value: format!("{agent_count}")
                }
                StatCard {
                    icon: "forum",
                    icon_bg: "bg-tertiary-container",
                    icon_color: "text-tertiary",
                    label: "Total Sessions",
                    value: format!("{session_count}")
                }
                StatCard {
                    icon: "stream",
                    icon_bg: "bg-success-container",
                    icon_color: "text-success",
                    label: "Active Sessions",
                    value: format!("{active_sessions}")
                }
            }

            // Quick links
            h2 { class: "section-title", "Quick Actions" }
            div { class: "grid grid-cols-1 md:grid-cols-2 gap-4",
                QuickLink {
                    to: Route::Agents {},
                    icon: "smart_toy",
                    title: "Agent Management",
                    description: "Create, configure, and manage autonomous agents"
                }
                QuickLink {
                    to: Route::Sessions {},
                    icon: "forum",
                    title: "Session Monitor",
                    description: "Watch real-time conversations and agent interactions"
                }
                QuickLink {
                    to: Route::Memory {},
                    icon: "memory",
                    title: "Memory Explorer",
                    description: "Browse and search agent memories and session logs"
                }
                QuickLink {
                    to: Route::Analytics {},
                    icon: "analytics",
                    title: "Analytics & Metrics",
                    description: "LLM costs, quality scores, and usage analytics"
                }
            }
        }
    }
}

#[component]
fn StatCard(icon: &'static str, icon_bg: &'static str, icon_color: &'static str, label: &'static str, value: String) -> Element {
    rsx! {
        div { class: "card-elevated",
            div { class: "flex items-center gap-4",
                div { class: "w-12 h-12 rounded-lg {icon_bg} flex items-center justify-center",
                    span { class: "material-symbols-outlined text-2xl {icon_color}", "{icon}" }
                }
                div {
                    p { class: "text-body-md text-on-surface-variant", "{label}" }
                    p { class: "text-headline-md text-on-surface font-semibold", "{value}" }
                }
            }
        }
    }
}

#[component]
fn QuickLink(to: Route, icon: &'static str, title: &'static str, description: &'static str) -> Element {
    rsx! {
        Link {
            to: to,
            class: "card-elevated flex items-start gap-4 group",
            div { class: "w-10 h-10 rounded-lg bg-primary-container flex items-center justify-center shrink-0 group-hover:bg-primary group-hover:text-primary-on transition-colors",
                span { class: "material-symbols-outlined text-xl text-primary group-hover:text-primary-on transition-colors", "{icon}" }
            }
            div {
                h3 { class: "text-title-md text-on-surface group-hover:text-primary transition-colors mb-1",
                    "{title}"
                }
                p { class: "text-body-md text-on-surface-variant",
                    "{description}"
                }
            }
        }
    }
}
