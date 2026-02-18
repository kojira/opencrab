use dioxus::prelude::*;
use crate::api::WorkspaceEntryDto;
use crate::app::Route;

#[server]
pub async fn list_workspace(agent_id: String, path: String) -> Result<Vec<WorkspaceEntryDto>, ServerFnError> {
    let ws = opencrab_core::workspace::Workspace::new(&agent_id, "data")
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    let entries = ws.list_dir(&path)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(entries
        .into_iter()
        .map(|e| WorkspaceEntryDto {
            name: e.name,
            is_dir: e.is_dir,
            size: e.size,
        })
        .collect())
}

#[server]
pub async fn read_workspace_file(agent_id: String, path: String) -> Result<String, ServerFnError> {
    let ws = opencrab_core::workspace::Workspace::new(&agent_id, "data")
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    ws.read_file(&path)
        .map_err(|e| ServerFnError::new(e.to_string()))
}

#[server]
pub async fn write_workspace_file(
    agent_id: String,
    path: String,
    content: String,
) -> Result<(), ServerFnError> {
    let ws = opencrab_core::workspace::Workspace::new(&agent_id, "data")
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    ws.write_file(&path, &content)
        .map_err(|e| ServerFnError::new(e.to_string()))
}

#[component]
pub fn Workspace(agent_id: String) -> Element {
    let agent_id_for_list = agent_id.clone();
    let mut current_path = use_signal(|| String::new());
    let mut selected_file = use_signal(|| Option::<String>::None);
    let mut file_content = use_signal(|| Option::<String>::None);
    let mut editing = use_signal(|| false);
    let mut edit_content = use_signal(|| String::new());

    let path = current_path.read().clone();
    let aid = agent_id_for_list.clone();

    let entries = use_resource(move || {
        let agent_id = aid.clone();
        let path = path.clone();
        async move { list_workspace(agent_id, path).await }
    });

    rsx! {
        div { class: "max-w-7xl mx-auto",
            div { class: "flex items-center gap-3 mb-2",
                Link {
                    to: Route::AgentDetail { id: agent_id.clone() },
                    class: "btn-text p-2",
                    span { class: "material-symbols-outlined", "arrow_back" }
                }
                h1 { class: "page-title", "Workspace" }
            }
            div { class: "flex items-center gap-2 text-body-md text-on-surface-variant mb-6",
                span { class: "material-symbols-outlined text-lg", "smart_toy" }
                span { "Agent: " }
                span { class: "font-mono text-on-surface", "{agent_id}" }
            }

            div { class: "grid grid-cols-1 lg:grid-cols-2 gap-6",
                // File tree panel
                div { class: "card-outlined overflow-hidden",
                    div { class: "px-4 py-3 border-b border-outline-variant bg-surface-container-high",
                        div { class: "flex items-center gap-2",
                            span { class: "material-symbols-outlined text-lg text-primary", "folder" }
                            if !current_path.read().is_empty() {
                                button {
                                    class: "btn-text text-body-sm p-1",
                                    onclick: move |_| {
                                        let path = current_path.read().clone();
                                        let parent = path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
                                        current_path.set(parent);
                                    },
                                    span { class: "material-symbols-outlined text-lg", "arrow_upward" }
                                    "Up"
                                }
                            }
                            span { class: "text-label-lg text-on-surface-variant",
                                "/{current_path}"
                            }
                        }
                    }

                    div { class: "p-1",
                        match &*entries.read() {
                            Some(Ok(entry_list)) => rsx! {
                                if entry_list.is_empty() {
                                    div { class: "p-8 text-center",
                                        span { class: "material-symbols-outlined text-3xl text-on-surface-variant/40 mb-2", "folder_off" }
                                        p { class: "text-body-md text-on-surface-variant", "Empty directory" }
                                    }
                                } else {
                                    for entry in entry_list.iter() {
                                        FileEntry {
                                            entry: entry.clone(),
                                            on_click: {
                                                let agent_id = agent_id.clone();
                                                move |(name, is_dir): (String, bool)| {
                                                    if is_dir {
                                                        let new_path = if current_path.read().is_empty() {
                                                            name
                                                        } else {
                                                            format!("{}/{}", current_path.read(), name)
                                                        };
                                                        current_path.set(new_path);
                                                    } else {
                                                        let file_path = if current_path.read().is_empty() {
                                                            name.clone()
                                                        } else {
                                                            format!("{}/{}", current_path.read(), name)
                                                        };
                                                        selected_file.set(Some(name));
                                                        let aid = agent_id.clone();
                                                        let fp = file_path.clone();
                                                        spawn(async move {
                                                            match read_workspace_file(aid, fp).await {
                                                                Ok(content) => {
                                                                    file_content.set(Some(content.clone()));
                                                                    edit_content.set(content);
                                                                    editing.set(false);
                                                                }
                                                                Err(e) => {
                                                                    file_content.set(Some(format!("Error: {e}")));
                                                                }
                                                            }
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                            Some(Err(e)) => rsx! {
                                div { class: "p-4",
                                    div { class: "flex items-center gap-2",
                                        span { class: "material-symbols-outlined text-error", "error" }
                                        p { class: "text-body-md text-error", "Error: {e}" }
                                    }
                                }
                            },
                            None => rsx! {
                                div { class: "p-8 text-center",
                                    p { class: "text-body-md text-on-surface-variant", "Loading..." }
                                }
                            },
                        }
                    }
                }

                // File viewer / editor panel
                div { class: "card-outlined overflow-hidden",
                    div { class: "px-4 py-3 border-b border-outline-variant bg-surface-container-high flex items-center justify-between",
                        div { class: "flex items-center gap-2",
                            span { class: "material-symbols-outlined text-lg text-primary",
                                if selected_file.read().is_some() { "description" } else { "draft" }
                            }
                            span { class: "text-label-lg text-on-surface",
                                if let Some(ref name) = *selected_file.read() {
                                    "{name}"
                                } else {
                                    "No file selected"
                                }
                            }
                        }
                        if selected_file.read().is_some() {
                            button {
                                class: if *editing.read() { "btn-outlined text-body-sm py-1.5 px-3" } else { "btn-tonal text-body-sm py-1.5 px-3" },
                                onclick: move |_| {
                                    let current = *editing.read();
                                    editing.set(!current);
                                },
                                span { class: "material-symbols-outlined text-lg",
                                    if *editing.read() { "close" } else { "edit" }
                                }
                                if *editing.read() { "Cancel" } else { "Edit" }
                            }
                        }
                    }

                    div { class: "p-4",
                        if let Some(ref content) = *file_content.read() {
                            if *editing.read() {
                                textarea {
                                    class: "w-full h-96 px-3 py-2 font-mono text-body-sm border border-outline rounded-md
                                            bg-surface text-on-surface focus:border-primary focus:ring-2 focus:ring-primary/20 focus:outline-none",
                                    value: "{edit_content}",
                                    oninput: move |e| edit_content.set(e.value())
                                }
                                button {
                                    class: "btn-filled mt-3",
                                    onclick: {
                                        let agent_id = agent_id.clone();
                                        move |_| {
                                            let aid = agent_id.clone();
                                            let file_path = if current_path.read().is_empty() {
                                                selected_file.read().clone().unwrap_or_default()
                                            } else {
                                                format!("{}/{}", current_path.read(), selected_file.read().clone().unwrap_or_default())
                                            };
                                            let content = edit_content.read().clone();
                                            spawn(async move {
                                                let _ = write_workspace_file(aid, file_path, content.clone()).await;
                                                file_content.set(Some(content));
                                                editing.set(false);
                                            });
                                        }
                                    },
                                    span { class: "material-symbols-outlined text-xl", "save" }
                                    "Save"
                                }
                            } else {
                                pre { class: "h-96 overflow-auto font-mono text-body-sm text-on-surface whitespace-pre-wrap p-2 rounded-md bg-surface-container-high",
                                    "{content}"
                                }
                            }
                        } else {
                            div { class: "text-center py-16",
                                span { class: "material-symbols-outlined text-5xl text-on-surface-variant/30 mb-3", "description" }
                                p { class: "text-body-lg text-on-surface-variant", "Select a file to view" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn FileEntry(entry: WorkspaceEntryDto, on_click: EventHandler<(String, bool)>) -> Element {
    let (icon, icon_color) = if entry.is_dir {
        ("folder", "text-warning")
    } else {
        let ext = entry.name.rsplit('.').next().unwrap_or("");
        match ext {
            "rs" => ("code", "text-primary"),
            "toml" | "json" | "yaml" | "yml" => ("settings", "text-tertiary"),
            "md" | "txt" => ("article", "text-secondary"),
            _ => ("draft", "text-on-surface-variant"),
        }
    };

    let name = entry.name.clone();
    let is_dir = entry.is_dir;

    rsx! {
        button {
            class: "w-full flex items-center gap-3 px-3 py-2.5 rounded-md
                    hover:bg-secondary-container/40 active:bg-secondary-container/60
                    text-left transition-colors duration-150",
            onclick: move |_| {
                on_click.call((name.clone(), is_dir));
            },
            span { class: "material-symbols-outlined text-xl {icon_color}", "{icon}" }
            span { class: "flex-1 text-body-md text-on-surface", "{entry.name}" }
            if !entry.is_dir {
                span { class: "text-label-sm text-on-surface-variant",
                    "{entry.size} B"
                }
            }
        }
    }
}
