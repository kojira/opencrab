use dioxus::prelude::*;
use crate::api::WorkspaceEntryDto;

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
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-2",
                "Workspace"
            }
            p { class: "text-gray-500 dark:text-gray-400 mb-6",
                "Agent: {agent_id}"
            }

            div { class: "grid grid-cols-1 lg:grid-cols-2 gap-6",
                // File tree
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow",
                    div { class: "p-4 border-b border-gray-200 dark:border-gray-700",
                        div { class: "flex items-center space-x-2",
                            if !current_path.read().is_empty() {
                                button {
                                    class: "text-blue-600 hover:text-blue-800 text-sm",
                                    onclick: move |_| {
                                        let path = current_path.read().clone();
                                        let parent = path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
                                        current_path.set(parent);
                                    },
                                    ".. (up)"
                                }
                            }
                            span { class: "text-sm text-gray-500",
                                "/{current_path}"
                            }
                        }
                    }

                    div { class: "p-2",
                        match &*entries.read() {
                            Some(Ok(entry_list)) => rsx! {
                                if entry_list.is_empty() {
                                    p { class: "p-4 text-gray-500 text-sm", "Empty directory" }
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
                                p { class: "p-4 text-red-500 text-sm", "Error: {e}" }
                            },
                            None => rsx! {
                                p { class: "p-4 text-gray-500 text-sm", "Loading..." }
                            },
                        }
                    }
                }

                // File viewer/editor
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow",
                    div { class: "p-4 border-b border-gray-200 dark:border-gray-700 flex items-center justify-between",
                        span { class: "text-sm font-medium text-gray-700 dark:text-gray-300",
                            if let Some(ref name) = *selected_file.read() {
                                "{name}"
                            } else {
                                "No file selected"
                            }
                        }
                        if selected_file.read().is_some() {
                            button {
                                class: "text-sm text-blue-600 hover:text-blue-800",
                                onclick: move |_| {
                                    let current = *editing.read();
                                    editing.set(!current);
                                },
                                if *editing.read() { "Cancel" } else { "Edit" }
                            }
                        }
                    }

                    div { class: "p-4",
                        if let Some(ref content) = *file_content.read() {
                            if *editing.read() {
                                textarea {
                                    class: "w-full h-96 px-3 py-2 font-mono text-sm border border-gray-300 dark:border-gray-600 rounded-lg bg-gray-50 dark:bg-gray-900 text-gray-900 dark:text-white",
                                    value: "{edit_content}",
                                    oninput: move |e| edit_content.set(e.value())
                                }
                                button {
                                    class: "mt-2 px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700",
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
                                    "Save"
                                }
                            } else {
                                pre { class: "h-96 overflow-auto font-mono text-sm text-gray-900 dark:text-white whitespace-pre-wrap",
                                    "{content}"
                                }
                            }
                        } else {
                            p { class: "text-gray-400 text-center py-12",
                                "Select a file to view"
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
    let icon = if entry.is_dir { "D" } else { "F" };
    let icon_bg = if entry.is_dir {
        "bg-yellow-100 text-yellow-800"
    } else {
        "bg-gray-100 text-gray-800"
    };
    let name = entry.name.clone();
    let is_dir = entry.is_dir;

    rsx! {
        button {
            class: "w-full flex items-center space-x-3 px-3 py-2 rounded hover:bg-gray-100 dark:hover:bg-gray-700 text-left",
            onclick: move |_| {
                on_click.call((name.clone(), is_dir));
            },
            span { class: "w-6 h-6 rounded text-xs font-bold flex items-center justify-center {icon_bg}",
                "{icon}"
            }
            span { class: "flex-1 text-sm text-gray-900 dark:text-white",
                "{entry.name}"
            }
            if !entry.is_dir {
                span { class: "text-xs text-gray-400",
                    "{entry.size} B"
                }
            }
        }
    }
}
