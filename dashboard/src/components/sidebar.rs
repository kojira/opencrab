use dioxus::prelude::*;
use crate::app::Route;

#[component]
pub fn Sidebar() -> Element {
    let route = use_route::<Route>();

    let is_home = matches!(route, Route::Home {});
    let is_agents = matches!(
        route,
        Route::Agents {}
            | Route::AgentCreate {}
            | Route::AgentDetail { .. }
            | Route::AgentIdentityEdit { .. }
            | Route::PersonaEdit { .. }
            | Route::Workspace { .. }
    );
    let is_skills = matches!(route, Route::Skills {});
    let is_memory = matches!(route, Route::Memory {});
    let is_sessions = matches!(route, Route::Sessions {} | Route::SessionDetail { .. });
    let is_analytics = matches!(route, Route::Analytics {});

    rsx! {
        nav { class: "w-72 bg-surface-container flex flex-col border-r border-outline-variant",
            // Logo
            div { class: "px-7 py-6",
                div { class: "flex items-center gap-3",
                    div { class: "w-10 h-10 rounded-lg bg-primary flex items-center justify-center",
                        span { class: "material-symbols-outlined text-primary-on text-xl", "code" }
                    }
                    div {
                        h1 { class: "text-title-lg text-on-surface font-semibold", "OpenCrab" }
                        p { class: "text-label-sm text-on-surface-variant", "Agent Framework" }
                    }
                }
            }

            // Divider
            div { class: "mx-4 h-px bg-outline-variant" }

            // Navigation
            div { class: "flex-1 px-3 py-4 space-y-1",
                SidebarLink { to: Route::Home {}, label: "Dashboard", icon: "dashboard", active: is_home }
                SidebarLink { to: Route::Agents {}, label: "Agents", icon: "smart_toy", active: is_agents }
                SidebarLink { to: Route::Skills {}, label: "Skills", icon: "psychology", active: is_skills }
                SidebarLink { to: Route::Memory {}, label: "Memory", icon: "memory", active: is_memory }
                SidebarLink { to: Route::Sessions {}, label: "Sessions", icon: "forum", active: is_sessions }
                SidebarLink { to: Route::Analytics {}, label: "Analytics", icon: "analytics", active: is_analytics }
            }

            // Footer
            div { class: "px-7 py-4 border-t border-outline-variant",
                p { class: "text-label-sm text-on-surface-variant",
                    "OpenCrab v0.1.0"
                }
            }
        }
    }
}

#[component]
fn SidebarLink(to: Route, label: &'static str, icon: &'static str, active: bool) -> Element {
    let class = if active { "nav-item-active" } else { "nav-item" };

    rsx! {
        Link {
            to: to,
            class: "{class}",
            span { class: "material-symbols-outlined text-xl", "{icon}" }
            span { "{label}" }
        }
    }
}
