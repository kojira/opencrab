use dioxus::prelude::*;
use crate::api::{get_agent, update_soul, PersonalityDto, SoulDto};
use crate::app::Route;

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
            div { class: "flex items-center gap-3 mb-6",
                Link {
                    to: Route::AgentDetail { id: id.clone() },
                    class: "btn-text p-2",
                    span { class: "material-symbols-outlined", "arrow_back" }
                }
                h1 { class: "page-title", "Edit Persona" }
            }

            if !*initialized.read() {
                div { class: "empty-state",
                    p { class: "text-body-lg text-on-surface-variant", "Loading..." }
                }
            } else {
                // Persona name section
                div { class: "card-outlined mb-6",
                    h2 { class: "section-title flex items-center gap-2",
                        span { class: "material-symbols-outlined text-xl text-primary", "face" }
                        "Persona Name"
                    }
                    input {
                        r#type: "text",
                        class: "input-outlined",
                        value: "{persona_name}",
                        oninput: move |e| persona_name.set(e.value())
                    }
                }

                // Big Five personality section
                div { class: "card-outlined mb-6",
                    h2 { class: "section-title flex items-center gap-2",
                        span { class: "material-symbols-outlined text-xl text-primary", "psychology" }
                        "Personality (Big Five)"
                    }
                    div { class: "space-y-5",
                        PersonalitySlider { label: "Openness", value: *openness.read(), on_change: move |v| openness.set(v) }
                        PersonalitySlider { label: "Conscientiousness", value: *conscientiousness.read(), on_change: move |v| conscientiousness.set(v) }
                        PersonalitySlider { label: "Extraversion", value: *extraversion.read(), on_change: move |v| extraversion.set(v) }
                        PersonalitySlider { label: "Agreeableness", value: *agreeableness.read(), on_change: move |v| agreeableness.set(v) }
                        PersonalitySlider { label: "Neuroticism", value: *neuroticism.read(), on_change: move |v| neuroticism.set(v) }
                    }
                }

                // Thinking style section
                div { class: "card-outlined mb-6",
                    h2 { class: "section-title flex items-center gap-2",
                        span { class: "material-symbols-outlined text-xl text-primary", "lightbulb" }
                        "Thinking Style"
                    }
                    div { class: "space-y-5",
                        div {
                            label { class: "block text-label-lg text-on-surface mb-2", "Primary" }
                            select {
                                class: "select-outlined",
                                value: "{thinking_primary}",
                                onchange: move |e| thinking_primary.set(e.value()),
                                option { value: "Analytical", "Analytical" }
                                option { value: "Intuitive", "Intuitive" }
                                option { value: "Practical", "Practical" }
                                option { value: "Creative", "Creative" }
                            }
                        }
                        div {
                            label { class: "block text-label-lg text-on-surface mb-2", "Secondary" }
                            select {
                                class: "select-outlined",
                                value: "{thinking_secondary}",
                                onchange: move |e| thinking_secondary.set(e.value()),
                                option { value: "Analytical", "Analytical" }
                                option { value: "Intuitive", "Intuitive" }
                                option { value: "Practical", "Practical" }
                                option { value: "Creative", "Creative" }
                            }
                        }
                        div {
                            label { class: "block text-label-lg text-on-surface mb-2", "Description" }
                            textarea {
                                class: "input-outlined min-h-[80px]",
                                rows: "3",
                                value: "{thinking_desc}",
                                oninput: move |e| thinking_desc.set(e.value())
                            }
                        }
                    }
                }

                // Status message
                if let Some(ref status) = *save_status.read() {
                    div { class: "flex items-center gap-2 p-4 rounded-md bg-success-container mb-6",
                        span { class: "material-symbols-outlined text-success", "check_circle" }
                        p { class: "text-body-md text-success-on-container", "{status}" }
                    }
                }

                // Save button
                button {
                    class: "btn-filled w-full py-3",
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
                    span { class: "material-symbols-outlined text-xl", "save" }
                    "Save"
                }
            }
        }
    }
}

#[component]
fn PersonalitySlider(label: &'static str, value: f32, on_change: EventHandler<f32>) -> Element {
    let pct = (value * 100.0) as i32;

    rsx! {
        div {
            div { class: "flex justify-between mb-2",
                span { class: "text-label-lg text-on-surface", "{label}" }
                span { class: "text-label-md text-primary font-mono", "{value:.2}" }
            }
            div { class: "relative",
                input {
                    r#type: "range",
                    class: "m3-slider",
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
                // Progress bar visual
                div { class: "absolute top-1/2 left-0 h-1 bg-primary rounded-full pointer-events-none -translate-y-1/2",
                    style: "width: {pct}%"
                }
            }
        }
    }
}
