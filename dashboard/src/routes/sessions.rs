use dioxus::prelude::*;
use crate::api::{get_sessions, get_session, get_session_logs, send_mentor_instruction};
use crate::app::Route;

#[component]
pub fn Sessions() -> Element {
    let sessions = use_resource(move || get_sessions());

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "page-title mb-6", "Sessions" }

            match &*sessions.read() {
                Some(Ok(session_list)) => rsx! {
                    if session_list.is_empty() {
                        div { class: "empty-state",
                            span { class: "material-symbols-outlined empty-state-icon", "forum" }
                            p { class: "empty-state-text", "No sessions found." }
                        }
                    } else {
                        div { class: "space-y-3",
                            for session in session_list.iter() {
                                SessionCard { session: session.clone() }
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
fn SessionCard(session: crate::api::SessionDto) -> Element {
    let (badge_class, status_icon) = match session.status.as_str() {
        "active" => ("badge-success", "play_circle"),
        "completed" => ("badge-info", "check_circle"),
        "paused" => ("badge-warning", "pause_circle"),
        _ => ("badge-neutral", "help"),
    };

    rsx! {
        Link {
            to: Route::SessionDetail { id: session.id.clone() },
            class: "card-elevated block group",
            div { class: "flex items-center justify-between",
                div { class: "flex items-center gap-4 flex-1 min-w-0",
                    div { class: "w-10 h-10 rounded-lg bg-tertiary-container flex items-center justify-center shrink-0",
                        span { class: "material-symbols-outlined text-xl text-tertiary", "forum" }
                    }
                    div { class: "min-w-0",
                        h3 { class: "text-title-md text-on-surface group-hover:text-primary transition-colors truncate",
                            "{session.theme}"
                        }
                        div { class: "flex items-center gap-3 text-body-sm text-on-surface-variant mt-0.5",
                            span { class: "flex items-center gap-1",
                                span { class: "material-symbols-outlined text-sm", "settings" }
                                "{session.mode}"
                            }
                            span { class: "flex items-center gap-1",
                                span { class: "material-symbols-outlined text-sm", "flag" }
                                "{session.phase}"
                            }
                            span { class: "flex items-center gap-1",
                                span { class: "material-symbols-outlined text-sm", "replay" }
                                "Turn {session.turn_number}"
                            }
                        }
                    }
                }
                div { class: "flex items-center gap-3 shrink-0",
                    span { class: "chip text-body-sm",
                        span { class: "material-symbols-outlined text-sm", "group" }
                        "{session.participant_count}"
                    }
                    span { class: "{badge_class}",
                        span { class: "material-symbols-outlined text-sm mr-0.5", "{status_icon}" }
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
                Some(Ok(s)) => {
                    let (badge_class, status_icon) = match s.status.as_str() {
                        "active" => ("badge-success", "play_circle"),
                        "completed" => ("badge-info", "check_circle"),
                        "paused" => ("badge-warning", "pause_circle"),
                        _ => ("badge-neutral", "help"),
                    };
                    rsx! {
                        div { class: "card-elevated mb-4",
                            div { class: "flex items-center justify-between",
                                div { class: "flex items-center gap-4",
                                    Link {
                                        to: Route::Sessions {},
                                        class: "btn-text p-1",
                                        span { class: "material-symbols-outlined", "arrow_back" }
                                    }
                                    div {
                                        h1 { class: "text-title-lg text-on-surface", "{s.theme}" }
                                        div { class: "flex items-center gap-3 text-body-sm text-on-surface-variant mt-0.5",
                                            span { "Mode: {s.mode}" }
                                            span { "Phase: {s.phase}" }
                                            span { "Turn: {s.turn_number}" }
                                        }
                                    }
                                }
                                span { class: "{badge_class}",
                                    span { class: "material-symbols-outlined text-sm mr-0.5", "{status_icon}" }
                                    "{s.status}"
                                }
                            }
                        }
                    }
                },
                _ => rsx! {
                    div { class: "card-elevated mb-4",
                        p { class: "text-body-lg text-on-surface-variant", "Loading session..." }
                    }
                },
            }

            // Log entries
            div { class: "flex-1 overflow-y-auto space-y-2 mb-4",
                match &*logs.read() {
                    Some(Ok(log_list)) => rsx! {
                        if log_list.is_empty() {
                            div { class: "empty-state",
                                span { class: "material-symbols-outlined empty-state-icon", "chat" }
                                p { class: "empty-state-text", "No logs yet." }
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
                        div { class: "card-outlined border-error bg-error-container/30 p-4",
                            div { class: "flex items-center gap-2",
                                span { class: "material-symbols-outlined text-error", "error" }
                                p { class: "text-body-lg text-error-on-container", "Error: {e}" }
                            }
                        }
                    },
                    None => rsx! {
                        div { class: "empty-state",
                            p { class: "text-body-lg text-on-surface-variant", "Loading logs..." }
                        }
                    },
                }
            }

            // Mentor input
            div { class: "card-elevated",
                form {
                    class: "flex gap-3",
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
                        class: "input-outlined flex-1",
                        placeholder: "Type mentor instruction...",
                        value: "{mentor_input}",
                        oninput: move |e| mentor_input.set(e.value())
                    }
                    button {
                        r#type: "submit",
                        class: "btn-filled",
                        span { class: "material-symbols-outlined text-xl", "send" }
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
    let (border_color, icon, icon_color) = match log_type.as_str() {
        "speech" => ("border-l-primary", "chat_bubble", "text-primary"),
        "inner_voice" => ("border-l-purple-500", "psychology", "text-purple-500"),
        "action" => ("border-l-tertiary", "bolt", "text-tertiary"),
        "system" => ("border-l-secondary", "settings", "text-secondary"),
        _ => ("border-l-outline", "help", "text-on-surface-variant"),
    };

    let speaker = speaker_id.unwrap_or_default();

    rsx! {
        div { class: "bg-surface-container rounded-lg border-l-4 {border_color} p-4",
            div { class: "flex items-center justify-between mb-2",
                div { class: "flex items-center gap-2",
                    span { class: "material-symbols-outlined text-lg {icon_color}", "{icon}" }
                    span { class: "text-label-lg text-on-surface", "{speaker}" }
                }
                div { class: "flex items-center gap-2",
                    span { class: "badge-neutral text-label-sm", "{log_type}" }
                    span { class: "text-body-sm text-on-surface-variant", "{created_at}" }
                }
            }
            p { class: "text-body-lg text-on-surface whitespace-pre-wrap pl-8",
                "{content}"
            }
        }
    }
}
