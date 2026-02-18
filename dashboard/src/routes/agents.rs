use dioxus::prelude::*;
use crate::api::{get_agents, get_agent, create_agent, update_identity, delete_agent};
use crate::app::Route;
use crate::components::AgentCard;

// ── Agents List ──

#[component]
pub fn Agents() -> Element {
    let agents = use_resource(move || get_agents());

    rsx! {
        div { class: "max-w-7xl mx-auto",
            div { class: "flex items-center justify-between mb-6",
                h1 { class: "page-title", "Agents" }
                Link {
                    to: Route::AgentCreate {},
                    class: "btn-filled",
                    span { class: "material-symbols-outlined text-xl", "add" }
                    "New Agent"
                }
            }

            match &*agents.read() {
                Some(Ok(agent_list)) => rsx! {
                    if agent_list.is_empty() {
                        div { class: "empty-state",
                            span { class: "material-symbols-outlined empty-state-icon", "smart_toy" }
                            p { class: "empty-state-text", "No agents found." }
                            p { class: "text-body-sm text-on-surface-variant mt-2",
                                "Create your first agent to get started."
                            }
                        }
                    } else {
                        div { class: "grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6",
                            for agent in agent_list.iter() {
                                AgentCard { key: "{agent.id}", agent: agent.clone() }
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
                        p { class: "text-body-lg text-on-surface-variant", "Loading..." }
                    }
                },
            }
        }
    }
}

// ── Agent Create ──

#[component]
pub fn AgentCreate() -> Element {
    let nav = navigator();
    let mut name = use_signal(|| String::new());
    let mut role = use_signal(|| "discussant".to_string());
    let mut persona_name = use_signal(|| String::new());
    let mut error_msg = use_signal(|| Option::<String>::None);
    let mut saving = use_signal(|| false);

    rsx! {
        div { class: "max-w-2xl mx-auto",
            div { class: "flex items-center gap-3 mb-6",
                Link {
                    to: Route::Agents {},
                    class: "btn-text p-2",
                    span { class: "material-symbols-outlined", "arrow_back" }
                }
                h1 { class: "page-title", "Create New Agent" }
            }

            div { class: "card-elevated space-y-6",
                // Name
                div {
                    label { class: "block text-label-lg text-on-surface mb-2", "Name *" }
                    input {
                        r#type: "text",
                        class: "input-outlined",
                        placeholder: "e.g. Kai",
                        value: "{name}",
                        oninput: move |e| name.set(e.value())
                    }
                }

                // Role
                div {
                    label { class: "block text-label-lg text-on-surface mb-2", "Role" }
                    select {
                        class: "select-outlined",
                        value: "{role}",
                        onchange: move |e| role.set(e.value()),
                        option { value: "discussant", "Discussant" }
                        option { value: "facilitator", "Facilitator" }
                        option { value: "observer", "Observer" }
                        option { value: "mentor", "Mentor" }
                    }
                }

                // Persona name
                div {
                    label { class: "block text-label-lg text-on-surface mb-2", "Persona Name" }
                    input {
                        r#type: "text",
                        class: "input-outlined",
                        placeholder: "e.g. Pragmatic Engineer (defaults to name)",
                        value: "{persona_name}",
                        oninput: move |e| persona_name.set(e.value())
                    }
                }

                // Error message
                if let Some(ref err) = *error_msg.read() {
                    div { class: "flex items-center gap-2 p-4 rounded-md bg-error-container",
                        span { class: "material-symbols-outlined text-error", "error" }
                        p { class: "text-body-md text-error-on-container", "{err}" }
                    }
                }

                // Buttons
                div { class: "flex gap-3 pt-2",
                    button {
                        class: "btn-filled flex-1",
                        disabled: *saving.read(),
                        onclick: move |_| {
                            let n = name.read().clone();
                            if n.is_empty() {
                                error_msg.set(Some("Name is required.".to_string()));
                                return;
                            }
                            let r = role.read().clone();
                            let p = persona_name.read().clone();
                            saving.set(true);
                            spawn(async move {
                                match create_agent(n, r, p).await {
                                    Ok(agent) => {
                                        nav.push(Route::AgentDetail { id: agent.id });
                                    }
                                    Err(e) => {
                                        error_msg.set(Some(format!("Error: {e}")));
                                        saving.set(false);
                                    }
                                }
                            });
                        },
                        if *saving.read() {
                            span { class: "material-symbols-outlined animate-spin text-xl", "progress_activity" }
                            "Creating..."
                        } else {
                            span { class: "material-symbols-outlined text-xl", "add" }
                            "Create"
                        }
                    }
                    Link {
                        to: Route::Agents {},
                        class: "btn-outlined",
                        "Cancel"
                    }
                }
            }
        }
    }
}

// ── Agent Detail ──

#[component]
pub fn AgentDetail(id: String) -> Element {
    let nav = navigator();
    let id_for_load = id.clone();
    let id_for_delete = id.clone();
    let agent = use_resource(move || {
        let id = id_for_load.clone();
        async move { get_agent(id).await }
    });
    let mut show_delete_confirm = use_signal(|| false);

    rsx! {
        div { class: "max-w-4xl mx-auto",
            match &*agent.read() {
                Some(Ok(detail)) => rsx! {
                    // Agent header card
                    div { class: "card-elevated mb-6",
                        div { class: "flex items-center gap-5",
                            div { class: "w-16 h-16 rounded-full bg-primary-container flex items-center justify-center",
                                span { class: "text-headline-sm text-primary-on-container font-semibold",
                                    "{detail.name.chars().next().unwrap_or('?')}"
                                }
                            }
                            div { class: "flex-1 min-w-0",
                                h1 { class: "text-headline-sm text-on-surface font-medium truncate",
                                    "{detail.name}"
                                }
                                p { class: "text-body-lg text-on-surface-variant",
                                    "{detail.persona_name} / {detail.role}"
                                }
                                if let Some(ref org) = detail.organization {
                                    p { class: "text-body-sm text-on-surface-variant", "{org}" }
                                }
                            }
                            div { class: "flex items-center gap-2",
                                Link {
                                    to: Route::AgentIdentityEdit { id: id.clone() },
                                    class: "btn-tonal",
                                    span { class: "material-symbols-outlined text-xl", "edit" }
                                    "Edit"
                                }
                                button {
                                    class: "btn-outlined border-error text-error hover:bg-error-container/30",
                                    onclick: move |_| show_delete_confirm.set(true),
                                    span { class: "material-symbols-outlined text-xl", "delete" }
                                    "Delete"
                                }
                            }
                        }
                    }

                    // Delete confirmation dialog
                    if *show_delete_confirm.read() {
                        div { class: "scrim",
                            div { class: "dialog",
                                div { class: "flex items-center gap-3 mb-4",
                                    span { class: "material-symbols-outlined text-2xl text-error", "warning" }
                                    h3 { class: "text-title-lg text-on-surface", "Delete Agent?" }
                                }
                                p { class: "text-body-lg text-on-surface-variant mb-6",
                                    "This will permanently delete the agent and all associated data (soul, skills, memories)."
                                }
                                div { class: "flex gap-3 justify-end",
                                    button {
                                        class: "btn-outlined",
                                        onclick: move |_| show_delete_confirm.set(false),
                                        "Cancel"
                                    }
                                    button {
                                        class: "btn-danger",
                                        onclick: move |_| {
                                            let agent_id = id_for_delete.clone();
                                            spawn(async move {
                                                if let Ok(true) = delete_agent(agent_id).await {
                                                    nav.push(Route::Agents {});
                                                }
                                            });
                                        },
                                        span { class: "material-symbols-outlined text-xl", "delete" }
                                        "Delete"
                                    }
                                }
                            }
                        }
                    }

                    // Action cards
                    div { class: "grid grid-cols-1 md:grid-cols-3 gap-4 mb-6",
                        ActionCard {
                            to: Route::PersonaEdit { id: id.clone() },
                            icon: "face",
                            title: "Edit Persona",
                            description: "Personality & thinking style"
                        }
                        ActionCard {
                            to: Route::Skills {},
                            icon: "psychology",
                            title: "Manage Skills",
                            description: "Enable/disable skills"
                        }
                        ActionCard {
                            to: Route::Workspace { agent_id: id.clone() },
                            icon: "folder_open",
                            title: "Workspace",
                            description: "Browse agent files"
                        }
                    }

                    // Identity details
                    div { class: "card-outlined",
                        h2 { class: "section-title flex items-center gap-2",
                            span { class: "material-symbols-outlined text-xl text-primary", "badge" }
                            "Identity"
                        }
                        div { class: "space-y-3",
                            DetailRow { label: "Agent ID", value: detail.id.clone() }
                            DetailRow { label: "Name", value: detail.name.clone() }
                            DetailRow { label: "Role", value: detail.role.clone() }
                            if let Some(ref title) = detail.job_title {
                                DetailRow { label: "Job Title", value: title.clone() }
                            }
                            if let Some(ref org) = detail.organization {
                                DetailRow { label: "Organization", value: org.clone() }
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
                        p { class: "text-body-lg text-on-surface-variant", "Loading..." }
                    }
                },
            }
        }
    }
}

#[component]
fn ActionCard(to: Route, icon: &'static str, title: &'static str, description: &'static str) -> Element {
    rsx! {
        Link {
            to: to,
            class: "card-elevated text-center group",
            span { class: "material-symbols-outlined text-3xl text-primary mb-2 group-hover:scale-110 transition-transform", "{icon}" }
            h3 { class: "text-title-md text-on-surface mb-1", "{title}" }
            p { class: "text-body-sm text-on-surface-variant", "{description}" }
        }
    }
}

// ── Agent Identity Edit ──

#[component]
pub fn AgentIdentityEdit(id: String) -> Element {
    let nav = navigator();
    let id_for_load = id.clone();
    let id_for_save = id.clone();
    let agent = use_resource(move || {
        let id = id_for_load.clone();
        async move { get_agent(id).await }
    });

    let mut name = use_signal(|| String::new());
    let mut role = use_signal(|| String::new());
    let mut job_title = use_signal(|| String::new());
    let mut organization = use_signal(|| String::new());
    let mut initialized = use_signal(|| false);
    let mut save_status = use_signal(|| Option::<String>::None);
    let mut saving = use_signal(|| false);

    // Load initial data
    if let Some(Ok(detail)) = agent.read().as_ref() {
        if !*initialized.read() {
            name.set(detail.name.clone());
            role.set(detail.role.clone());
            job_title.set(detail.job_title.clone().unwrap_or_default());
            organization.set(detail.organization.clone().unwrap_or_default());
            initialized.set(true);
        }
    }

    rsx! {
        div { class: "max-w-2xl mx-auto",
            div { class: "flex items-center gap-3 mb-6",
                Link {
                    to: Route::AgentDetail { id: id.clone() },
                    class: "btn-text p-2",
                    span { class: "material-symbols-outlined", "arrow_back" }
                }
                h1 { class: "page-title", "Edit Identity" }
            }

            if !*initialized.read() {
                div { class: "empty-state",
                    p { class: "text-body-lg text-on-surface-variant", "Loading..." }
                }
            } else {
                div { class: "card-elevated space-y-6",
                    div {
                        label { class: "block text-label-lg text-on-surface mb-2", "Name" }
                        input {
                            r#type: "text",
                            class: "input-outlined",
                            value: "{name}",
                            oninput: move |e| name.set(e.value())
                        }
                    }

                    div {
                        label { class: "block text-label-lg text-on-surface mb-2", "Role" }
                        select {
                            class: "select-outlined",
                            value: "{role}",
                            onchange: move |e| role.set(e.value()),
                            option { value: "discussant", "Discussant" }
                            option { value: "facilitator", "Facilitator" }
                            option { value: "observer", "Observer" }
                            option { value: "mentor", "Mentor" }
                        }
                    }

                    div {
                        label { class: "block text-label-lg text-on-surface mb-2", "Job Title" }
                        input {
                            r#type: "text",
                            class: "input-outlined",
                            placeholder: "Optional",
                            value: "{job_title}",
                            oninput: move |e| job_title.set(e.value())
                        }
                    }

                    div {
                        label { class: "block text-label-lg text-on-surface mb-2", "Organization" }
                        input {
                            r#type: "text",
                            class: "input-outlined",
                            placeholder: "Optional",
                            value: "{organization}",
                            oninput: move |e| organization.set(e.value())
                        }
                    }

                    // Status message
                    if let Some(ref status) = *save_status.read() {
                        div { class: "flex items-center gap-2 p-4 rounded-md bg-success-container",
                            span { class: "material-symbols-outlined text-success", "check_circle" }
                            p { class: "text-body-md text-success-on-container", "{status}" }
                        }
                    }

                    // Buttons
                    div { class: "flex gap-3 pt-2",
                        button {
                            class: "btn-filled flex-1",
                            disabled: *saving.read(),
                            onclick: move |_| {
                                let agent_id = id_for_save.clone();
                                let n = name.read().clone();
                                let r = role.read().clone();
                                let jt = job_title.read().clone();
                                let org = organization.read().clone();
                                saving.set(true);
                                spawn(async move {
                                    let jt_opt = if jt.is_empty() { None } else { Some(jt) };
                                    let org_opt = if org.is_empty() { None } else { Some(org) };
                                    match update_identity(agent_id.clone(), n, r, jt_opt, org_opt).await {
                                        Ok(_) => {
                                            nav.push(Route::AgentDetail { id: agent_id });
                                        }
                                        Err(e) => {
                                            save_status.set(Some(format!("Error: {e}")));
                                            saving.set(false);
                                        }
                                    }
                                });
                            },
                            if *saving.read() {
                                span { class: "material-symbols-outlined animate-spin text-xl", "progress_activity" }
                                "Saving..."
                            } else {
                                span { class: "material-symbols-outlined text-xl", "save" }
                                "Save"
                            }
                        }
                        Link {
                            to: Route::AgentDetail { id: id.clone() },
                            class: "btn-outlined",
                            "Cancel"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn DetailRow(label: &'static str, value: String) -> Element {
    rsx! {
        div { class: "flex items-center py-2",
            span { class: "w-36 text-label-lg text-on-surface-variant", "{label}" }
            span { class: "text-body-lg text-on-surface font-mono", "{value}" }
        }
    }
}
