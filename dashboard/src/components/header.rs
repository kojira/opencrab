use dioxus::prelude::*;

#[component]
pub fn Header() -> Element {
    rsx! {
        header { class: "bg-white dark:bg-gray-800 shadow-sm border-b border-gray-200 dark:border-gray-700 px-6 py-4",
            div { class: "flex items-center justify-between",
                // Search
                div { class: "flex-1 max-w-lg",
                    input {
                        r#type: "text",
                        class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-gray-50 dark:bg-gray-700 text-gray-900 dark:text-white focus:ring-2 focus:ring-blue-500 focus:border-transparent",
                        placeholder: "Search agents, sessions, memories..."
                    }
                }

                // Status indicators
                div { class: "flex items-center space-x-4",
                    // DB status
                    div { class: "flex items-center space-x-2",
                        span { class: "w-2 h-2 rounded-full bg-green-500" }
                        span { class: "text-sm text-gray-500 dark:text-gray-400", "DB Connected" }
                    }
                }
            }
        }
    }
}
