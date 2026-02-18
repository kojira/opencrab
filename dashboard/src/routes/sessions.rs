use dioxus::prelude::*;
use crate::api::{get_sessions, get_session, get_session_logs, send_mentor_instruction};
use crate::app::Route;

#[component]
pub fn Sessions() -> Element {
    let sessions = use_resource(move || get_sessions());

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-6",
                "Sessions"
            }

            match &*sessions.read() {
                Some(Ok(session_list)) => rsx! {
                    if session_list.is_empty() {
                        div { class: "text-center py-12",
                            p { class: "text-gray-500 dark:text-gray-400",
                                "No sessions found."
                            }
                        }
                    } else {
                        div { class: "space-y-4",
                            for session in session_list.iter() {
                                SessionCard { session: session.clone() }
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

#[component]
fn SessionCard(session: crate::api::SessionDto) -> Element {
    let status_class = match session.status.as_str() {
        "active" => "bg-green-100 text-green-800",
        "completed" => "bg-blue-100 text-blue-800",
        "paused" => "bg-yellow-100 text-yellow-800",
        _ => "bg-gray-100 text-gray-800",
    };

    rsx! {
        Link {
            to: Route::SessionDetail { id: session.id.clone() },
            class: "block bg-white dark:bg-gray-800 rounded-lg shadow p-6 hover:shadow-lg transition-shadow",
            div { class: "flex items-center justify-between",
                div { class: "flex-1",
                    h3 { class: "text-lg font-semibold text-gray-900 dark:text-white",
                        "{session.theme}"
                    }
                    p { class: "text-sm text-gray-500 dark:text-gray-400 mt-1",
                        "Mode: {session.mode} | Phase: {session.phase} | Turn: {session.turn_number}"
                    }
                }
                div { class: "flex items-center space-x-3",
                    span { class: "text-sm text-gray-500",
                        "{session.participant_count} participants"
                    }
                    span { class: "px-2 py-1 text-xs rounded-full {status_class}",
                        "{session.status}"
                    }
                }
            }
        }
    }
}

#[component]
pub fn SessionDetail(id: String) -> Element {
    let id_for_session = id.clone();
    let id_for_logs = id.clone();
    let id_for_send = id.clone();

    let session = use_resource(move || {
        let id = id_for_session.clone();
        async move { get_session(id).await }
    });

    let logs = use_resource(move || {
        let id = id_for_logs.clone();
        async move { get_session_logs(id).await }
    });

    let mut mentor_input = use_signal(|| String::new());

    rsx! {
        div { class: "max-w-4xl mx-auto h-full flex flex-col",
            // Session header
            match &*session.read() {
                Some(Ok(s)) => rsx! {
                    div { class: "bg-white dark:bg-gray-800 shadow rounded-lg p-4 mb-4",
                        div { class: "flex items-center justify-between",
                            div {
                                h1 { class: "text-xl font-bold text-gray-900 dark:text-white",
                                    "{s.theme}"
                                }
                                p { class: "text-sm text-gray-500",
                                    "Mode: {s.mode} | Phase: {s.phase} | Turn: {s.turn_number} | Status: {s.status}"
                                }
                            }
                        }
                    }
                },
                _ => rsx! {
                    div { class: "bg-white dark:bg-gray-800 shadow rounded-lg p-4 mb-4",
                        p { class: "text-gray-500", "Loading session..." }
                    }
                },
            }

            // Log entries
            div { class: "flex-1 overflow-y-auto space-y-2 mb-4",
                match &*logs.read() {
                    Some(Ok(log_list)) => rsx! {
                        if log_list.is_empty() {
                            div { class: "text-center py-12",
                                p { class: "text-gray-500", "No logs yet." }
                            }
                        } else {
                            for log in log_list.iter() {
                                SessionLogItem {
                                    log_type: log.log_type.clone(),
                                    content: log.content.clone(),
                                    speaker_id: log.speaker_id.clone(),
                                    created_at: log.created_at.clone(),
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
                            p { class: "text-gray-500", "Loading logs..." }
                        }
                    },
                }
            }

            // Mentor input
            div { class: "bg-white dark:bg-gray-800 shadow rounded-lg p-4",
                form {
                    class: "flex space-x-2",
                    onsubmit: move |e| {
                        e.prevent_default();
                        let session_id = id_for_send.clone();
                        let content = mentor_input.read().clone();
                        if !content.is_empty() {
                            mentor_input.set(String::new());
                            let mut logs = logs.clone();
                            spawn(async move {
                                let _ = send_mentor_instruction(session_id, content).await;
                                logs.restart();
                            });
                        }
                    },
                    input {
                        r#type: "text",
                        class: "flex-1 px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                        placeholder: "Type mentor instruction...",
                        value: "{mentor_input}",
                        oninput: move |e| mentor_input.set(e.value())
                    }
                    button {
                        r#type: "submit",
                        class: "px-6 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 font-medium",
                        "Send"
                    }
                }
            }
        }
    }
}

#[component]
fn SessionLogItem(
    log_type: String,
    content: String,
    speaker_id: Option<String>,
    created_at: String,
) -> Element {
    let bg_class = match log_type.as_str() {
        "speech" => "bg-blue-50 border-blue-200 dark:bg-blue-900/20 dark:border-blue-800",
        "inner_voice" => "bg-purple-50 border-purple-200 dark:bg-purple-900/20 dark:border-purple-800",
        "action" => "bg-green-50 border-green-200 dark:bg-green-900/20 dark:border-green-800",
        "system" => "bg-gray-50 border-gray-200 dark:bg-gray-900/20 dark:border-gray-700",
        _ => "bg-white border-gray-200 dark:bg-gray-800 dark:border-gray-700",
    };

    let speaker = speaker_id.unwrap_or_default();

    rsx! {
        div { class: "p-3 rounded-lg border {bg_class}",
            div { class: "flex justify-between text-sm text-gray-500 dark:text-gray-400 mb-1",
                span { class: "font-medium", "{speaker}" }
                div { class: "flex items-center space-x-2",
                    span { class: "px-1.5 py-0.5 text-xs rounded bg-gray-200 dark:bg-gray-600",
                        "{log_type}"
                    }
                    span { "{created_at}" }
                }
            }
            p { class: "text-gray-900 dark:text-white whitespace-pre-wrap",
                "{content}"
            }
        }
    }
}
