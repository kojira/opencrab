use dioxus::prelude::*;
use crate::components::{Header, Sidebar};
use crate::routes::*;

#[derive(Routable, Clone, PartialEq, Debug)]
pub enum Route {
    #[layout(AppLayout)]
    #[route("/")]
    Home {},
    #[route("/agents")]
    Agents {},
    #[route("/agents/new")]
    AgentCreate {},
    #[route("/agents/:id")]
    AgentDetail { id: String },
    #[route("/agents/:id/edit")]
    AgentIdentityEdit { id: String },
    #[route("/agents/:id/persona")]
    PersonaEdit { id: String },
    #[route("/skills")]
    Skills {},
    #[route("/memory")]
    Memory {},
    #[route("/sessions")]
    Sessions {},
    #[route("/sessions/:id")]
    SessionDetail { id: String },
    #[route("/workspace/:agent_id")]
    Workspace { agent_id: String },
    #[route("/analytics")]
    Analytics {},
}

#[component]
fn AppLayout() -> Element {
    rsx! {
        div { class: "flex h-screen bg-surface font-sans",
            Sidebar {}
            div { class: "flex-1 flex flex-col overflow-hidden",
                Header {}
                main { class: "flex-1 overflow-y-auto bg-surface p-6",
                    Outlet::<Route> {}
                }
            }
        }
    }
}

#[component]
pub fn App() -> Element {
    rsx! {
        // Tailwind CSS (built by dx serve's integrated Tailwind v3 watcher)
        document::Link {
            rel: "stylesheet",
            href: asset!("/assets/tailwind.css"),
        }
        // Google Fonts: Inter (UI text) + JetBrains Mono (code)
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap",
        }
        // Material Symbols Outlined (icons)
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=Material+Symbols+Outlined:opsz,wght,FILL,GRAD@24,400,0,0",
        }
        Router::<Route> {}
    }
}
