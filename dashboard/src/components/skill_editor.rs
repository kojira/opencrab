use dioxus::prelude::*;
use crate::api::SkillDto;

#[component]
pub fn SkillEditor(skill: SkillDto, on_toggle: EventHandler<(String, bool)>) -> Element {
    let effectiveness_pct = skill.effectiveness.map(|e| (e * 100.0) as i32).unwrap_or(0);
    let source_badge = match skill.source_type.as_str() {
        "standard" => "badge-info",
        "acquired" => "bg-tertiary-container text-tertiary-on-container badge",
        _ => "badge-neutral",
    };
    let is_active = skill.is_active;
    let skill_id = skill.id.clone();

    rsx! {
        div { class: "card-outlined",
            div { class: "flex items-start justify-between gap-4",
                div { class: "flex-1 min-w-0",
                    div { class: "flex items-center gap-2 mb-1",
                        span { class: "material-symbols-outlined text-xl text-primary", "extension" }
                        h3 { class: "text-title-md text-on-surface truncate",
                            "{skill.name}"
                        }
                        span { class: "{source_badge}", "{skill.source_type}" }
                    }
                    p { class: "text-body-md text-on-surface-variant ml-8",
                        "{skill.description}"
                    }
                }

                // M3 Switch
                button {
                    class: if is_active { "switch-active" } else { "switch" },
                    onclick: move |_| {
                        on_toggle.call((skill_id.clone(), !is_active));
                    },
                    span {
                        class: if is_active { "switch-thumb-active" } else { "switch-thumb" }
                    }
                }
            }

            // Stats row
            div { class: "mt-3 pt-3 border-t border-outline-variant/50 flex items-center gap-6 ml-8",
                div { class: "flex items-center gap-1.5 text-body-sm text-on-surface-variant",
                    span { class: "material-symbols-outlined text-base", "repeat" }
                    span { "Used {skill.usage_count} times" }
                }
                if skill.effectiveness.is_some() {
                    div { class: "flex items-center gap-1.5 text-body-sm text-on-surface-variant",
                        span { class: "material-symbols-outlined text-base", "speed" }
                        span { "Effectiveness: {effectiveness_pct}%" }
                    }
                }
            }
        }
    }
}
