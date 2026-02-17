use dioxus::prelude::*;
use crate::app::Route;

#[component]
pub fn Sidebar() -> Element {
    rsx! {
        nav { class: "w-64 bg-white dark:bg-gray-800 shadow-lg flex flex-col",
            // Logo
            div { class: "p-6 border-b border-gray-200 dark:border-gray-700",
                h1 { class: "text-2xl font-bold text-blue-600 dark:text-blue-400",
                    "OpenCrab"
                }
                p { class: "text-sm text-gray-500 dark:text-gray-400 mt-1",
                    "Agent Framework"
                }
            }

            // Navigation
            div { class: "flex-1 p-4 space-y-1",
                SidebarLink { to: Route::Home {}, label: "Home", icon: "H" }
                SidebarLink { to: Route::Agents {}, label: "Agents", icon: "A" }
                SidebarLink { to: Route::Skills {}, label: "Skills", icon: "S" }
                SidebarLink { to: Route::Memory {}, label: "Memory", icon: "M" }
                SidebarLink { to: Route::Sessions {}, label: "Sessions", icon: "C" }
                SidebarLink { to: Route::Analytics {}, label: "Analytics", icon: "G" }
            }

            // Footer
            div { class: "p-4 border-t border-gray-200 dark:border-gray-700",
                p { class: "text-xs text-gray-400",
                    "OpenCrab v0.1.0"
                }
            }
        }
    }
}

#[component]
fn SidebarLink(to: Route, label: &'static str, icon: &'static str) -> Element {
    rsx! {
        Link {
            to: to,
            class: "flex items-center space-x-3 px-3 py-2 rounded-lg text-gray-700 dark:text-gray-300 hover:bg-gray-100 dark:hover:bg-gray-700 transition-colors",
            span { class: "w-8 h-8 rounded-lg bg-gray-200 dark:bg-gray-600 flex items-center justify-center text-sm font-bold",
                "{icon}"
            }
            span { class: "font-medium", "{label}" }
        }
    }
}
