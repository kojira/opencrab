use dioxus::prelude::*;
use crate::api::{get_agents, get_skills, toggle_skill};
use crate::components::SkillEditor;

#[component]
pub fn Skills() -> Element {
    let agents = use_resource(move || get_agents());
    let mut selected_agent = use_signal(|| Option::<String>::None);
    let mut skills_version = use_signal(|| 0u32);

    let agent_id = selected_agent.read().clone();
    let _version = *skills_version.read();

    let skills = use_resource(move || {
        let agent_id = agent_id.clone();
        let _v = _version;
        async move {
            if let Some(id) = agent_id {
                get_skills(id).await
            } else {
                Ok(vec![])
            }
        }
    });

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-6",
                "Skills Management"
            }

            // Agent selector
            div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 mb-6",
                label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-2",
                    "Select Agent"
                }
                match &*agents.read() {
                    Some(Ok(agent_list)) => rsx! {
                        select {
                            class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
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
                        p { class: "text-gray-500", "Loading agents..." }
                    },
                }
            }

            // Skills list
            if selected_agent.read().is_some() {
                match &*skills.read() {
                    Some(Ok(skill_list)) => rsx! {
                        if skill_list.is_empty() {
                            div { class: "text-center py-12",
                                p { class: "text-gray-500 dark:text-gray-400",
                                    "No skills found for this agent."
                                }
                            }
                        } else {
                            div { class: "space-y-4",
                                for skill in skill_list.iter() {
                                    SkillEditor {
                                        key: "{skill.id}",
                                        skill: skill.clone(),
                                        on_toggle: move |(skill_id, active): (String, bool)| {
                                            spawn(async move {
                                                let _ = toggle_skill(skill_id, active).await;
                                                skills_version += 1;
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "bg-red-50 border border-red-200 rounded-lg p-4",
                            p { class: "text-red-800", "Error: {e}" }
                        }
                    },
                    None => rsx! {
                        div { class: "text-center py-12",
                            p { class: "text-gray-500", "Loading skills..." }
                        }
                    },
                }
            }
        }
    }
}
