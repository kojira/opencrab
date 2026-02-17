use dioxus::prelude::*;
use crate::components::{Header, Sidebar};
use crate::routes::*;

#[derive(Routable, Clone, PartialEq, Debug)]
pub enum Route {
    #[route("/")]
    Home {},
    #[route("/agents")]
    Agents {},
    #[route("/agents/:id")]
    AgentDetail { id: String },
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
pub fn App() -> Element {
    rsx! {
        div { class: "flex h-screen bg-gray-100 dark:bg-gray-900",
            Sidebar {}
            div { class: "flex-1 flex flex-col overflow-hidden",
                Header {}
                main { class: "flex-1 overflow-y-auto p-6",
                    Router::<Route> {}
                }
            }
        }
    }
}
