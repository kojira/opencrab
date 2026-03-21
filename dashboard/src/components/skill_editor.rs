use dioxus::prelude::*;
use crate::api::SkillDto;

#[component]
pub fn SkillEditor(
    skill: SkillDto,
    all_skills: Vec<SkillDto>,
    on_toggle: EventHandler<(String, bool)>,
    on_update: EventHandler<(String, Option<String>, Option<String>, Option<String>)>,
    on_archive: EventHandler<(String, bool)>,
    on_merge: EventHandler<(String, String)>,
) -> Element {
    let effectiveness_pct = skill.effectiveness.map(|e| (e * 100.0) as i32).unwrap_or(0);
    let source_badge = match skill.source_type.as_str() {
        "standard" => "badge-info",
        "acquired" => "bg-tertiary-container text-tertiary-on-container badge",
        _ => "badge-neutral",
    };
    let is_active = skill.is_active;
    let is_archived = skill.archived;
    let skill_id = skill.id.clone();
    let skill_id_toggle = skill.id.clone();
    let skill_id_archive = skill.id.clone();

    let mut editing = use_signal(|| false);
    let mut edit_name = use_signal(|| skill.name.clone());
    let mut edit_description = use_signal(|| skill.description.clone());
    let mut edit_guidance = use_signal(|| skill.guidance.clone());
    let mut show_merge = use_signal(|| false);
    let mut merge_target = use_signal(|| String::new());

    let merge_candidates: Vec<SkillDto> = all_skills
        .iter()
        .filter(|s| s.id != skill.id && !s.archived)
        .cloned()
        .collect();

    rsx! {
        div {
            class: if is_archived { "card-outlined opacity-60" } else { "card-outlined" },
            div { class: "flex items-start justify-between gap-4",
                div { class: "flex-1 min-w-0",
                    if *editing.read() {
                        // Inline edit form
                        div { class: "space-y-2",
                            input {
                                class: "input-outlined w-full",
                                r#type: "text",
                                value: "{edit_name}",
                                oninput: move |e| edit_name.set(e.value()),
                            }
                            textarea {
                                class: "input-outlined w-full",
                                rows: "2",
                                value: "{edit_description}",
                                oninput: move |e| edit_description.set(e.value()),
                            }
                            textarea {
                                class: "input-outlined w-full",
                                rows: "2",
                                placeholder: "Guidance...",
                                value: "{edit_guidance}",
                                oninput: move |e| edit_guidance.set(e.value()),
                            }
                            div { class: "flex gap-2",
                                button {
                                    class: "btn-filled-sm",
                                    onclick: {
                                        let skill_id = skill_id.clone();
                                        move |_| {
                                            let name = Some(edit_name.read().clone());
                                            let desc = Some(edit_description.read().clone());
                                            let guidance = Some(edit_guidance.read().clone());
                                            on_update.call((skill_id.clone(), name, desc, guidance));
                                            editing.set(false);
                                        }
                                    },
                                    "Save"
                                }
                                button {
                                    class: "btn-outlined-sm",
                                    onclick: move |_| editing.set(false),
                                    "Cancel"
                                }
                            }
                        }
                    } else {
                        div { class: "flex items-center gap-2 mb-1",
                            span { class: "material-symbols-outlined text-xl text-primary", "extension" }
                            h3 { class: "text-title-md text-on-surface truncate",
                                "{skill.name}"
                            }
                            span { class: "{source_badge}", "{skill.source_type}" }
                            if is_archived {
                                span { class: "badge bg-error-container text-error", "archived" }
                            }
                        }
                        p { class: "text-body-md text-on-surface-variant ml-8",
                            "{skill.description}"
                        }
                        if !skill.guidance.is_empty() {
                            p { class: "text-body-sm text-on-surface-variant/70 ml-8 mt-1 italic",
                                "Guidance: {skill.guidance}"
                            }
                        }
                    }
                }

                div { class: "flex items-center gap-2",
                    if !is_archived {
                        // Edit button
                        button {
                            class: "icon-btn-sm",
                            title: "Edit",
                            onclick: move |_| editing.set(true),
                            span { class: "material-symbols-outlined text-base", "edit" }
                        }
                        // Archive button
                        button {
                            class: "icon-btn-sm",
                            title: "Archive",
                            onclick: {
                                let sid = skill_id_archive.clone();
                                move |_| on_archive.call((sid.clone(), true))
                            },
                            span { class: "material-symbols-outlined text-base", "archive" }
                        }
                        // Merge button
                        button {
                            class: "icon-btn-sm",
                            title: "Merge",
                            onclick: move |_| show_merge.set(!show_merge()),
                            span { class: "material-symbols-outlined text-base", "merge" }
                        }
                        // M3 Switch
                        button {
                            class: if is_active { "switch-active" } else { "switch" },
                            onclick: {
                                let sid = skill_id_toggle.clone();
                                move |_| on_toggle.call((sid.clone(), !is_active))
                            },
                            span {
                                class: if is_active { "switch-thumb-active" } else { "switch-thumb" }
                            }
                        }
                    } else {
                        // Restore button
                        button {
                            class: "btn-outlined-sm",
                            onclick: {
                                let sid = skill_id_archive.clone();
                                move |_| on_archive.call((sid.clone(), false))
                            },
                            span { class: "material-symbols-outlined text-base mr-1", "unarchive" }
                            "Restore"
                        }
                    }
                }
            }

            // Merge UI
            if *show_merge.read() && !merge_candidates.is_empty() {
                div { class: "mt-3 pt-3 border-t border-outline-variant/50 ml-8",
                    p { class: "text-label-md text-on-surface mb-2", "Merge into:" }
                    div { class: "flex gap-2 items-center",
                        select {
                            class: "select-outlined flex-1",
                            onchange: move |e| merge_target.set(e.value()),
                            option { value: "", "-- Select target skill --" }
                            for candidate in merge_candidates.iter() {
                                option { value: "{candidate.id}", "{candidate.name}" }
                            }
                        }
                        button {
                            class: "btn-filled-sm",
                            disabled: merge_target.read().is_empty(),
                            onclick: {
                                let sid = skill_id.clone();
                                move |_| {
                                    let target = merge_target.read().clone();
                                    if !target.is_empty() {
                                        on_merge.call((sid.clone(), target));
                                        show_merge.set(false);
                                    }
                                }
                            },
                            "Merge"
                        }
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
