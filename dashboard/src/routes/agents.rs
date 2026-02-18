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
                h1 { class: "text-2xl font-bold text-gray-900 dark:text-white",
                    "Agents"
                }
                Link {
                    to: Route::AgentCreate {},
                    class: "px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 font-medium",
                    "New Agent"
                }
            }

            match &*agents.read() {
                Some(Ok(agent_list)) => rsx! {
                    if agent_list.is_empty() {
                        div { class: "text-center py-12",
                            p { class: "text-gray-500 dark:text-gray-400 text-lg",
                                "No agents found."
                            }
                            p { class: "text-gray-400 dark:text-gray-500 mt-2",
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
                    div { class: "bg-red-50 border border-red-200 rounded-lg p-4",
                        p { class: "text-red-800", "Error: {e}" }
                    }
                },
                None => rsx! {
                    div { class: "text-center py-12",
                        p { class: "text-gray-500", "Loading..." }
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
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-6",
                "Create New Agent"
            }

            div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 space-y-6",
                // Name
                div {
                    label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                        "Name *"
                    }
                    input {
                        r#type: "text",
                        class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                        placeholder: "e.g. Kai",
                        value: "{name}",
                        oninput: move |e| name.set(e.value())
                    }
                }

                // Role
                div {
                    label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                        "Role"
                    }
                    select {
                        class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
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
                    label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                        "Persona Name"
                    }
                    input {
                        r#type: "text",
                        class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                        placeholder: "e.g. Pragmatic Engineer (defaults to name)",
                        value: "{persona_name}",
                        oninput: move |e| persona_name.set(e.value())
                    }
                }

                // Error message
                if let Some(ref err) = *error_msg.read() {
                    div { class: "p-3 rounded-lg bg-red-50 border border-red-200 text-red-800",
                        "{err}"
                    }
                }

                // Buttons
                div { class: "flex space-x-3",
                    button {
                        class: "flex-1 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 font-medium disabled:opacity-50",
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
                        if *saving.read() { "Creating..." } else { "Create" }
                    }
                    Link {
                        to: Route::Agents {},
                        class: "px-6 py-2 bg-gray-200 dark:bg-gray-700 text-gray-700 dark:text-gray-300 rounded-lg hover:bg-gray-300 font-medium text-center",
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
                    // Header
                    div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 mb-6",
                        div { class: "flex items-center space-x-4",
                            div { class: "w-16 h-16 rounded-full bg-blue-500 flex items-center justify-center text-white text-2xl font-bold",
                                "{detail.name.chars().next().unwrap_or('?')}"
                            }
                            div { class: "flex-1",
                                h1 { class: "text-2xl font-bold text-gray-900 dark:text-white",
                                    "{detail.name}"
                                }
                                p { class: "text-gray-500 dark:text-gray-400",
                                    "{detail.persona_name} / {detail.role}"
                                }
                                if let Some(ref org) = detail.organization {
                                    p { class: "text-sm text-gray-400", "{org}" }
                                }
                            }
                            // Edit / Delete buttons
                            div { class: "flex space-x-2",
                                Link {
                                    to: Route::AgentIdentityEdit { id: id.clone() },
                                    class: "px-3 py-1.5 text-sm bg-gray-200 dark:bg-gray-700 text-gray-700 dark:text-gray-300 rounded-lg hover:bg-gray-300",
                                    "Edit"
                                }
                                button {
                                    class: "px-3 py-1.5 text-sm bg-red-100 text-red-700 rounded-lg hover:bg-red-200",
                                    onclick: move |_| show_delete_confirm.set(true),
                                    "Delete"
                                }
                            }
                        }
                    }

                    // Delete confirmation modal
                    if *show_delete_confirm.read() {
                        div { class: "fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50",
                            div { class: "bg-white dark:bg-gray-800 rounded-lg shadow-xl p-6 max-w-sm mx-4",
                                h3 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-2",
                                    "Delete Agent?"
                                }
                                p { class: "text-gray-600 dark:text-gray-400 mb-4",
                                    "This will permanently delete the agent and all associated data (soul, skills, memories)."
                                }
                                div { class: "flex space-x-3",
                                    button {
                                        class: "flex-1 py-2 bg-red-600 text-white rounded-lg hover:bg-red-700 font-medium",
                                        onclick: move |_| {
                                            let agent_id = id_for_delete.clone();
                                            spawn(async move {
                                                if let Ok(true) = delete_agent(agent_id).await {
                                                    nav.push(Route::Agents {});
                                                }
                                            });
                                        },
                                        "Delete"
                                    }
                                    button {
                                        class: "flex-1 py-2 bg-gray-200 dark:bg-gray-700 text-gray-700 dark:text-gray-300 rounded-lg hover:bg-gray-300 font-medium",
                                        onclick: move |_| show_delete_confirm.set(false),
                                        "Cancel"
                                    }
                                }
                            }
                        }
                    }

                    // Action cards
                    div { class: "grid grid-cols-1 md:grid-cols-3 gap-4 mb-6",
                        Link {
                            to: Route::PersonaEdit { id: id.clone() },
                            class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 hover:shadow-lg transition-shadow text-center block",
                            h3 { class: "font-semibold text-gray-900 dark:text-white", "Edit Persona" }
                            p { class: "text-sm text-gray-500", "Personality & thinking style" }
                        }
                        Link {
                            to: Route::Skills {},
                            class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 hover:shadow-lg transition-shadow text-center block",
                            h3 { class: "font-semibold text-gray-900 dark:text-white", "Manage Skills" }
                            p { class: "text-sm text-gray-500", "Enable/disable skills" }
                        }
                        Link {
                            to: Route::Workspace { agent_id: id.clone() },
                            class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 hover:shadow-lg transition-shadow text-center block",
                            h3 { class: "font-semibold text-gray-900 dark:text-white", "Workspace" }
                            p { class: "text-sm text-gray-500", "Browse agent files" }
                        }
                    }

                    // Identity details
                    div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6",
                        h2 { class: "text-lg font-semibold text-gray-900 dark:text-white mb-4",
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
                    div { class: "bg-red-50 border border-red-200 rounded-lg p-4",
                        p { class: "text-red-800", "Error: {e}" }
                    }
                },
                None => rsx! {
                    div { class: "text-center py-12",
                        p { class: "text-gray-500", "Loading..." }
                    }
                },
            }
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
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-6",
                "Edit Identity"
            }

            if !*initialized.read() {
                div { class: "text-center py-12",
                    p { class: "text-gray-500", "Loading..." }
                }
            } else {
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-6 space-y-6",
                    div {
                        label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                            "Name"
                        }
                        input {
                            r#type: "text",
                            class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                            value: "{name}",
                            oninput: move |e| name.set(e.value())
                        }
                    }

                    div {
                        label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                            "Role"
                        }
                        select {
                            class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                            value: "{role}",
                            onchange: move |e| role.set(e.value()),
                            option { value: "discussant", "Discussant" }
                            option { value: "facilitator", "Facilitator" }
                            option { value: "observer", "Observer" }
                            option { value: "mentor", "Mentor" }
                        }
                    }

                    div {
                        label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                            "Job Title"
                        }
                        input {
                            r#type: "text",
                            class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                            placeholder: "Optional",
                            value: "{job_title}",
                            oninput: move |e| job_title.set(e.value())
                        }
                    }

                    div {
                        label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                            "Organization"
                        }
                        input {
                            r#type: "text",
                            class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                            placeholder: "Optional",
                            value: "{organization}",
                            oninput: move |e| organization.set(e.value())
                        }
                    }

                    // Status message
                    if let Some(ref status) = *save_status.read() {
                        div { class: "p-3 rounded-lg bg-green-50 border border-green-200 text-green-800",
                            "{status}"
                        }
                    }

                    // Buttons
                    div { class: "flex space-x-3",
                        button {
                            class: "flex-1 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 font-medium disabled:opacity-50",
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
                            if *saving.read() { "Saving..." } else { "Save" }
                        }
                        Link {
                            to: Route::AgentDetail { id: id.clone() },
                            class: "px-6 py-2 bg-gray-200 dark:bg-gray-700 text-gray-700 dark:text-gray-300 rounded-lg hover:bg-gray-300 font-medium text-center",
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
        div { class: "flex items-center",
            span { class: "w-32 text-sm font-medium text-gray-500 dark:text-gray-400", "{label}" }
            span { class: "text-gray-900 dark:text-white", "{value}" }
        }
    }
}
