use dioxus::prelude::*;
use crate::api::{get_agents, get_llm_metrics, get_llm_metrics_detail};

#[component]
pub fn Analytics() -> Element {
    let agents = use_resource(move || get_agents());
    let mut selected_agent = use_signal(|| Option::<String>::None);
    let mut selected_period = use_signal(|| "week".to_string());

    let agent_id = selected_agent.read().clone();
    let period = selected_period.read().clone();

    let summary = use_resource(move || {
        let agent_id = agent_id.clone();
        let period = period.clone();
        async move {
            if let Some(id) = agent_id {
                get_llm_metrics(id, period).await.ok()
            } else {
                None
            }
        }
    });

    let agent_id2 = selected_agent.read().clone();
    let period2 = selected_period.read().clone();

    let detail = use_resource(move || {
        let agent_id = agent_id2.clone();
        let period = period2.clone();
        async move {
            if let Some(id) = agent_id {
                get_llm_metrics_detail(id, period).await.ok()
            } else {
                None
            }
        }
    });

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "page-title mb-6", "Analytics & Metrics" }

            // Controls
            div { class: "card-elevated mb-6",
                div { class: "flex gap-4",
                    div { class: "flex-1",
                        label { class: "block text-label-lg text-on-surface mb-2",
                            span { class: "flex items-center gap-1.5",
                                span { class: "material-symbols-outlined text-lg", "smart_toy" }
                                "Agent"
                            }
                        }
                        match &*agents.read() {
                            Some(Ok(agent_list)) => rsx! {
                                select {
                                    class: "select-outlined",
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
                            _ => rsx! { p { class: "text-body-md text-on-surface-variant", "Loading..." } },
                        }
                    }
                    div {
                        label { class: "block text-label-lg text-on-surface mb-2",
                            span { class: "flex items-center gap-1.5",
                                span { class: "material-symbols-outlined text-lg", "calendar_today" }
                                "Period"
                            }
                        }
                        div { class: "segmented-group",
                            button {
                                class: if *selected_period.read() == "day" { "segmented-btn-active" } else { "segmented-btn" },
                                onclick: move |_| selected_period.set("day".to_string()),
                                "24h"
                            }
                            button {
                                class: if *selected_period.read() == "week" { "segmented-btn-active" } else { "segmented-btn" },
                                onclick: move |_| selected_period.set("week".to_string()),
                                "7 days"
                            }
                            button {
                                class: if *selected_period.read() == "month" { "segmented-btn-active" } else { "segmented-btn" },
                                onclick: move |_| selected_period.set("month".to_string()),
                                "30 days"
                            }
                        }
                    }
                }
            }

            if selected_agent.read().is_some() {
                // Summary metric cards
                if let Some(Some(s)) = summary.read().as_ref() {
                    div { class: "grid grid-cols-2 md:grid-cols-5 gap-4 mb-6",
                        MetricCard { icon: "api", label: "API Calls", value: format!("{}", s.count) }
                        MetricCard { icon: "token", label: "Total Tokens", value: format_number(s.total_tokens) }
                        MetricCard { icon: "payments", label: "Total Cost", value: format!("${:.4}", s.total_cost) }
                        MetricCard { icon: "speed", label: "Avg Latency", value: format!("{:.0}ms", s.avg_latency) }
                        MetricCard { icon: "grade", label: "Avg Quality", value: format!("{:.2}", s.avg_quality) }
                    }
                }

                // Detail table
                div { class: "card-outlined overflow-hidden",
                    div { class: "px-6 py-4 border-b border-outline-variant",
                        h2 { class: "section-title mb-0 flex items-center gap-2",
                            span { class: "material-symbols-outlined text-xl text-primary", "table_chart" }
                            "Usage by Model"
                        }
                    }

                    match detail.read().as_ref() {
                        Some(Some(models)) => rsx! {
                            if models.is_empty() {
                                div { class: "empty-state",
                                    span { class: "material-symbols-outlined empty-state-icon", "table_rows" }
                                    p { class: "empty-state-text", "No usage data for this period." }
                                }
                            } else {
                                div { class: "overflow-x-auto",
                                    table { class: "data-table",
                                        thead {
                                            tr {
                                                th { "Provider" }
                                                th { "Model" }
                                                th { class: "text-right", "Requests" }
                                                th { class: "text-right", "Tokens" }
                                                th { class: "text-right", "Cost" }
                                                th { class: "text-right", "Avg Latency" }
                                            }
                                        }
                                        tbody {
                                            for model in models.iter() {
                                                tr {
                                                    td { "{model.provider}" }
                                                    td { class: "font-mono", "{model.model}" }
                                                    td { class: "text-right", "{model.request_count}" }
                                                    td { class: "text-right", "{format_number(model.total_tokens)}" }
                                                    td { class: "text-right", "${model.total_cost:.4}" }
                                                    td { class: "text-right", "{model.avg_latency:.0}ms" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        _ => rsx! {
                            div { class: "empty-state",
                                p { class: "text-body-lg text-on-surface-variant", "Loading..." }
                            }
                        },
                    }
                }
            } else {
                div { class: "empty-state",
                    span { class: "material-symbols-outlined empty-state-icon", "analytics" }
                    p { class: "empty-state-text", "Select an agent to view metrics" }
                }
            }
        }
    }
}

#[component]
fn MetricCard(icon: &'static str, label: &'static str, value: String) -> Element {
    rsx! {
        div { class: "card-elevated",
            div { class: "flex items-center gap-2 mb-2",
                span { class: "material-symbols-outlined text-lg text-primary", "{icon}" }
                p { class: "text-label-lg text-on-surface-variant", "{label}" }
            }
            p { class: "text-headline-sm text-on-surface font-semibold", "{value}" }
        }
    }
}

fn format_number(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}
