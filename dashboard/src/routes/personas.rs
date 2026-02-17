use dioxus::prelude::*;
use crate::api::{get_agent, update_soul, PersonalityDto, SoulDto};

#[component]
pub fn PersonaEdit(id: String) -> Element {
    let id_for_load = id.clone();
    let agent = use_resource(move || {
        let id = id_for_load.clone();
        async move { get_agent(id).await }
    });

    let mut persona_name = use_signal(|| String::new());
    let mut openness = use_signal(|| 0.5f32);
    let mut conscientiousness = use_signal(|| 0.5f32);
    let mut extraversion = use_signal(|| 0.5f32);
    let mut agreeableness = use_signal(|| 0.5f32);
    let mut neuroticism = use_signal(|| 0.5f32);
    let mut thinking_primary = use_signal(|| "Analytical".to_string());
    let mut thinking_secondary = use_signal(|| "Practical".to_string());
    let mut thinking_desc = use_signal(|| String::new());
    let mut initialized = use_signal(|| false);
    let mut save_status = use_signal(|| Option::<String>::None);

    // Load initial data
    if let Some(Ok(detail)) = agent.read().as_ref() {
        if !*initialized.read() {
            persona_name.set(detail.persona_name.clone());

            if let Ok(p) = serde_json::from_str::<PersonalityDto>(&detail.personality_json) {
                openness.set(p.openness);
                conscientiousness.set(p.conscientiousness);
                extraversion.set(p.extraversion);
                agreeableness.set(p.agreeableness);
                neuroticism.set(p.neuroticism);
            }

            if let Ok(ts) = serde_json::from_str::<serde_json::Value>(&detail.thinking_style_json) {
                if let Some(p) = ts.get("primary").and_then(|v| v.as_str()) {
                    thinking_primary.set(p.to_string());
                }
                if let Some(s) = ts.get("secondary").and_then(|v| v.as_str()) {
                    thinking_secondary.set(s.to_string());
                }
                if let Some(d) = ts.get("description").and_then(|v| v.as_str()) {
                    thinking_desc.set(d.to_string());
                }
            }

            initialized.set(true);
        }
    }

    let id_for_save = id.clone();

    rsx! {
        div { class: "max-w-4xl mx-auto",
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-6",
                "Edit Persona"
            }

            if !*initialized.read() {
                div { class: "text-center py-12",
                    p { class: "text-gray-500", "Loading..." }
                }
            } else {
                // Persona name
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 mb-6",
                    h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-4",
                        "Persona Name"
                    }
                    input {
                        r#type: "text",
                        class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                        value: "{persona_name}",
                        oninput: move |e| persona_name.set(e.value())
                    }
                }

                // Big Five personality
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 mb-6",
                    h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-4",
                        "Personality (Big Five)"
                    }
                    PersonalitySlider { label: "Openness", value: *openness.read(), on_change: move |v| openness.set(v) }
                    PersonalitySlider { label: "Conscientiousness", value: *conscientiousness.read(), on_change: move |v| conscientiousness.set(v) }
                    PersonalitySlider { label: "Extraversion", value: *extraversion.read(), on_change: move |v| extraversion.set(v) }
                    PersonalitySlider { label: "Agreeableness", value: *agreeableness.read(), on_change: move |v| agreeableness.set(v) }
                    PersonalitySlider { label: "Neuroticism", value: *neuroticism.read(), on_change: move |v| neuroticism.set(v) }
                }

                // Thinking style
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 mb-6",
                    h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-4",
                        "Thinking Style"
                    }
                    div { class: "space-y-4",
                        div {
                            label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                                "Primary"
                            }
                            select {
                                class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                                value: "{thinking_primary}",
                                onchange: move |e| thinking_primary.set(e.value()),
                                option { value: "Analytical", "Analytical" }
                                option { value: "Intuitive", "Intuitive" }
                                option { value: "Practical", "Practical" }
                                option { value: "Creative", "Creative" }
                            }
                        }
                        div {
                            label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                                "Secondary"
                            }
                            select {
                                class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                                value: "{thinking_secondary}",
                                onchange: move |e| thinking_secondary.set(e.value()),
                                option { value: "Analytical", "Analytical" }
                                option { value: "Intuitive", "Intuitive" }
                                option { value: "Practical", "Practical" }
                                option { value: "Creative", "Creative" }
                            }
                        }
                        div {
                            label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                                "Description"
                            }
                            textarea {
                                class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                                rows: "3",
                                value: "{thinking_desc}",
                                oninput: move |e| thinking_desc.set(e.value())
                            }
                        }
                    }
                }

                // Save button
                if let Some(ref status) = *save_status.read() {
                    div { class: "mb-4 p-3 rounded-lg bg-green-50 border border-green-200 text-green-800",
                        "{status}"
                    }
                }

                button {
                    class: "w-full bg-blue-600 text-white py-3 rounded-lg hover:bg-blue-700 font-semibold transition-colors",
                    onclick: move |_| {
                        let agent_id = id_for_save.clone();
                        let soul = SoulDto {
                            persona_name: persona_name.read().clone(),
                            social_style_json: "{}".to_string(),
                            personality: PersonalityDto {
                                openness: *openness.read(),
                                conscientiousness: *conscientiousness.read(),
                                extraversion: *extraversion.read(),
                                agreeableness: *agreeableness.read(),
                                neuroticism: *neuroticism.read(),
                            },
                            thinking_style_primary: thinking_primary.read().clone(),
                            thinking_style_secondary: thinking_secondary.read().clone(),
                            thinking_style_description: thinking_desc.read().clone(),
                        };
                        spawn(async move {
                            match update_soul(agent_id, soul).await {
                                Ok(_) => save_status.set(Some("Saved successfully!".to_string())),
                                Err(e) => save_status.set(Some(format!("Error: {e}"))),
                            }
                        });
                    },
                    "Save"
                }
            }
        }
    }
}

#[component]
fn PersonalitySlider(label: &'static str, value: f32, on_change: EventHandler<f32>) -> Element {
    rsx! {
        div { class: "mb-4",
            div { class: "flex justify-between mb-1",
                span { class: "text-sm font-medium text-gray-700 dark:text-gray-300", "{label}" }
                span { class: "text-sm text-gray-500 dark:text-gray-400", "{value:.2}" }
            }
            input {
                r#type: "range",
                class: "w-full h-2 bg-gray-200 rounded-lg appearance-none cursor-pointer dark:bg-gray-700",
                min: "0",
                max: "1",
                step: "0.05",
                value: "{value}",
                oninput: move |e| {
                    if let Ok(v) = e.value().parse::<f32>() {
                        on_change.call(v);
                    }
                }
            }
        }
    }
}
