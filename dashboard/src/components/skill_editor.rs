use dioxus::prelude::*;
use crate::api::SkillDto;

#[component]
pub fn SkillEditor(skill: SkillDto, on_toggle: EventHandler<(String, bool)>) -> Element {
    let effectiveness_pct = skill.effectiveness.map(|e| (e * 100.0) as i32).unwrap_or(0);
    let source_badge = match skill.source_type.as_str() {
        "standard" => "bg-blue-100 text-blue-800",
        "acquired" => "bg-purple-100 text-purple-800",
        _ => "bg-gray-100 text-gray-800",
    };
    let is_active = skill.is_active;
    let skill_id = skill.id.clone();

    rsx! {
        div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 border border-gray-200 dark:border-gray-700",
            div { class: "flex items-start justify-between",
                div { class: "flex-1",
                    div { class: "flex items-center space-x-2",
                        h3 { class: "font-semibold text-gray-900 dark:text-white",
                            "{skill.name}"
                        }
                        span { class: "px-2 py-0.5 text-xs rounded-full {source_badge}",
                            "{skill.source_type}"
                        }
                    }
                    p { class: "mt-1 text-sm text-gray-500 dark:text-gray-400",
                        "{skill.description}"
                    }
                }

                // Toggle switch
                button {
                    class: if is_active {
                        "relative inline-flex h-6 w-11 items-center rounded-full bg-blue-600"
                    } else {
                        "relative inline-flex h-6 w-11 items-center rounded-full bg-gray-300"
                    },
                    onclick: move |_| {
                        on_toggle.call((skill_id.clone(), !is_active));
                    },
                    span {
                        class: if is_active {
                            "inline-block h-4 w-4 transform rounded-full bg-white transition translate-x-6"
                        } else {
                            "inline-block h-4 w-4 transform rounded-full bg-white transition translate-x-1"
                        }
                    }
                }
            }

            // Stats
            div { class: "mt-3 flex items-center space-x-4 text-xs text-gray-500",
                span { "Used: {skill.usage_count} times" }
                if skill.effectiveness.is_some() {
                    span { "Effectiveness: {effectiveness_pct}%" }
                }
            }
        }
    }
}
