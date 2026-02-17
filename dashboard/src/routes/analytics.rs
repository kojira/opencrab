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
            h1 { class: "text-2xl font-bold text-gray-900 dark:text-white mb-6",
                "Analytics & Metrics"
            }

            // Controls
            div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 mb-6",
                div { class: "flex space-x-4",
                    div { class: "flex-1",
                        label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                            "Agent"
                        }
                        match &*agents.read() {
                            Some(Ok(agent_list)) => rsx! {
                                select {
                                    class: "w-full px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
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
                            _ => rsx! { p { class: "text-gray-500", "Loading..." } },
                        }
                    }
                    div {
                        label { class: "block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1",
                            "Period"
                        }
                        select {
                            class: "px-4 py-2 rounded-lg border border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-700 text-gray-900 dark:text-white",
                            value: "{selected_period}",
                            onchange: move |e| selected_period.set(e.value()),
                            option { value: "day", "Last 24h" }
                            option { value: "week", "Last 7 days" }
                            option { value: "month", "Last 30 days" }
                        }
                    }
                }
            }

            if selected_agent.read().is_some() {
                // Summary cards
                if let Some(Some(s)) = summary.read().as_ref() {
                    div { class: "grid grid-cols-1 md:grid-cols-5 gap-4 mb-6",
                        MetricCard { label: "API Calls", value: format!("{}", s.count) }
                        MetricCard { label: "Total Tokens", value: format_number(s.total_tokens) }
                        MetricCard { label: "Total Cost", value: format!("${:.4}", s.total_cost) }
                        MetricCard { label: "Avg Latency", value: format!("{:.0}ms", s.avg_latency) }
                        MetricCard { label: "Avg Quality", value: format!("{:.2}", s.avg_quality) }
                    }
                }

                // Detail table
                div { class: "bg-white dark:bg-gray-800 rounded-lg shadow",
                    div { class: "p-4 border-b border-gray-200 dark:border-gray-700",
                        h2 { class: "text-lg font-semibold text-gray-900 dark:text-white",
                            "Usage by Model"
                        }
                    }

                    match detail.read().as_ref() {
                        Some(Some(models)) => rsx! {
                            if models.is_empty() {
                                div { class: "p-8 text-center",
                                    p { class: "text-gray-500", "No usage data for this period." }
                                }
                            } else {
                                table { class: "w-full",
                                    thead {
                                        tr { class: "border-b border-gray-200 dark:border-gray-700",
                                            th { class: "px-4 py-3 text-left text-sm font-medium text-gray-500", "Provider" }
                                            th { class: "px-4 py-3 text-left text-sm font-medium text-gray-500", "Model" }
                                            th { class: "px-4 py-3 text-right text-sm font-medium text-gray-500", "Requests" }
                                            th { class: "px-4 py-3 text-right text-sm font-medium text-gray-500", "Tokens" }
                                            th { class: "px-4 py-3 text-right text-sm font-medium text-gray-500", "Cost" }
                                            th { class: "px-4 py-3 text-right text-sm font-medium text-gray-500", "Avg Latency" }
                                        }
                                    }
                                    tbody {
                                        for model in models.iter() {
                                            tr { class: "border-b border-gray-100 dark:border-gray-700 hover:bg-gray-50 dark:hover:bg-gray-750",
                                                td { class: "px-4 py-3 text-sm text-gray-900 dark:text-white", "{model.provider}" }
                                                td { class: "px-4 py-3 text-sm text-gray-900 dark:text-white font-mono", "{model.model}" }
                                                td { class: "px-4 py-3 text-sm text-gray-900 dark:text-white text-right", "{model.request_count}" }
                                                td { class: "px-4 py-3 text-sm text-gray-900 dark:text-white text-right", "{format_number(model.total_tokens)}" }
                                                td { class: "px-4 py-3 text-sm text-gray-900 dark:text-white text-right", "${model.total_cost:.4}" }
                                                td { class: "px-4 py-3 text-sm text-gray-900 dark:text-white text-right", "{model.avg_latency:.0}ms" }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        _ => rsx! {
                            div { class: "p-8 text-center",
                                p { class: "text-gray-500", "Loading..." }
                            }
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn MetricCard(label: &'static str, value: String) -> Element {
    rsx! {
        div { class: "bg-white dark:bg-gray-800 rounded-lg shadow p-4 border border-gray-200 dark:border-gray-700",
            p { class: "text-sm text-gray-500 dark:text-gray-400 mb-1", "{label}" }
            p { class: "text-xl font-bold text-gray-900 dark:text-white", "{value}" }
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
