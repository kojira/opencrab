use dioxus::prelude::*;
use crate::api::{get_agents, get_skills_filtered, toggle_skill, update_skill_info, archive_skill, merge_skills};
use crate::components::SkillEditor;

#[component]
pub fn Skills() -> Element {
    let agents = use_resource(move || get_agents());
    let mut selected_agent = use_signal(|| Option::<String>::None);
    let mut skills_version = use_signal(|| 0u32);
    let mut show_archived = use_signal(|| false);

    let agent_id = selected_agent.read().clone();
    let _version = *skills_version.read();
    let include_archived = *show_archived.read();

    let skills = use_resource(move || {
        let agent_id = agent_id.clone();
        let _v = _version;
        let archived = include_archived;
        async move {
            if let Some(id) = agent_id {
                get_skills_filtered(id, archived).await
            } else {
                Ok(vec![])
            }
        }
    });

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "page-title mb-6", "Skills Management" }

            // Agent selector
            div { class: "card-elevated mb-6",
                label { class: "block text-label-lg text-on-surface mb-2",
                    span { class: "flex items-center gap-1.5",
                        span { class: "material-symbols-outlined text-lg", "smart_toy" }
                        "Select Agent"
                    }
                }
                match &*agents.read() {
                    Some(Ok(agent_list)) => rsx! {
                        select {
                            class: "select-outlined",
                            onchange: move |e| {
                                let val = e.value();
                                if val.is_empty() {
                                    selected_agent.set(None);
                                } else {
                                    selected_agent.set(Some(val));
                                }
                            },
                            option { value: "", "-- Select an agent --" }
                            for agent in agent_list.iter() {
                                option { value: "{agent.id}", "{agent.name}" }
                            }
                        }
                    },
                    _ => rsx! {
                        p { class: "text-body-md text-on-surface-variant", "Loading agents..." }
                    },
                }
            }

            // Archive toggle
            if selected_agent.read().is_some() {
                div { class: "flex items-center gap-3 mb-4",
                    label { class: "flex items-center gap-2 text-body-md text-on-surface-variant cursor-pointer",
                        input {
                            r#type: "checkbox",
                            checked: include_archived,
                            onchange: move |e| {
                                show_archived.set(e.checked());
                                skills_version += 1;
                            },
                        }
                        span { class: "material-symbols-outlined text-base", "archive" }
                        "Show archived skills"
                    }
                }
            }

            // Skills list
            if selected_agent.read().is_some() {
                match &*skills.read() {
                    Some(Ok(skill_list)) => rsx! {
                        if skill_list.is_empty() {
                            div { class: "empty-state",
                                span { class: "material-symbols-outlined empty-state-icon", "psychology" }
                                p { class: "empty-state-text", "No skills found for this agent." }
                            }
                        } else {
                            div { class: "space-y-3",
                                for skill in skill_list.iter() {
                                    SkillEditor {
                                        key: "{skill.id}",
                                        skill: skill.clone(),
                                        all_skills: skill_list.clone(),
                                        on_toggle: move |(skill_id, active): (String, bool)| {
                                            spawn(async move {
                                                let _ = toggle_skill(skill_id, active).await;
                                                skills_version += 1;
                                            });
                                        },
                                        on_update: move |(skill_id, name, desc, guidance): (String, Option<String>, Option<String>, Option<String>)| {
                                            spawn(async move {
                                                let _ = update_skill_info(skill_id, name, desc, guidance, None).await;
                                                skills_version += 1;
                                            });
                                        },
                                        on_archive: move |(skill_id, archived): (String, bool)| {
                                            spawn(async move {
                                                let _ = archive_skill(skill_id, archived).await;
                                                skills_version += 1;
                                            });
                                        },
                                        on_merge: move |(source_id, target_id): (String, String)| {
                                            spawn(async move {
                                                let _ = merge_skills(source_id, target_id).await;
                                                skills_version += 1;
                                            });
                                        },
                                    }
                                }
                            }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "card-outlined border-error bg-error-container/30 p-4",
                            div { class: "flex items-center gap-2",
                                span { class: "material-symbols-outlined text-error", "error" }
                                p { class: "text-body-lg text-error-on-container", "Error: {e}" }
                            }
                        }
                    },
                    None => rsx! {
                        div { class: "empty-state",
                            p { class: "text-body-lg text-on-surface-variant", "Loading skills..." }
                        }
                    },
                }
            } else {
                div { class: "empty-state",
                    span { class: "material-symbols-outlined empty-state-icon", "psychology" }
                    p { class: "empty-state-text", "Select an agent to manage skills" }
                }
            }
        }
    }
}
