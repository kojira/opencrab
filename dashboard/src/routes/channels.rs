use dioxus::prelude::*;
use crate::api::{list_channel_configs, set_channel_whitelisted, set_channel_heartbeat_config, ChannelConfigDto};

#[component]
pub fn Channels() -> Element {
    let mut guild_id_input = use_signal(|| String::new());
    let mut guild_id = use_signal(|| Option::<String>::None);

    let channels = use_resource(move || {
        let gid = guild_id.read().clone();
        async move {
            match gid {
                Some(id) => list_channel_configs(id).await,
                None => Ok(vec![]),
            }
        }
    });

    rsx! {
        div { class: "max-w-7xl mx-auto",
            h1 { class: "page-title mb-6", "チャンネル設定" }

            // Guild ID input
            div { class: "card-elevated mb-6",
                form {
                    class: "flex gap-3",
                    onsubmit: move |e| {
                        e.prevent_default();
                        let val = guild_id_input.read().clone();
                        if !val.is_empty() {
                            guild_id.set(Some(val));
                        }
                    },
                    input {
                        r#type: "text",
                        class: "input-outlined flex-1",
                        placeholder: "Guild ID を入力...",
                        value: "{guild_id_input}",
                        oninput: move |e| guild_id_input.set(e.value())
                    }
                    button {
                        r#type: "submit",
                        class: "btn-filled",
                        span { class: "material-symbols-outlined text-xl", "search" }
                        "読み込む"
                    }
                }
            }

            // Channel list
            match &*channels.read() {
                Some(Ok(config_list)) => {
                    if guild_id.read().is_none() {
                        rsx! {
                            div { class: "empty-state",
                                span { class: "material-symbols-outlined empty-state-icon", "tune" }
                                p { class: "empty-state-text", "Guild ID を入力してチャンネル設定を読み込んでください。" }
                            }
                        }
                    } else if config_list.is_empty() {
                        rsx! {
                            div { class: "empty-state",
                                span { class: "material-symbols-outlined empty-state-icon", "tune" }
                                p { class: "empty-state-text", "チャンネル設定が見つかりません。" }
                            }
                        }
                    } else {
                        rsx! {
                            div { class: "card-elevated overflow-hidden",
                                table { class: "w-full",
                                    thead {
                                        tr { class: "border-b border-outline-variant",
                                            th { class: "text-left text-label-lg text-on-surface-variant px-4 py-3", "チャンネル名" }
                                            th { class: "text-left text-label-lg text-on-surface-variant px-4 py-3", "チャンネルID" }
                                            th { class: "text-center text-label-lg text-on-surface-variant px-4 py-3", "ホワイトリスト" }
                                            th { class: "text-center text-label-lg text-on-surface-variant px-4 py-3", "HB有効" }
                                            th { class: "text-left text-label-lg text-on-surface-variant px-4 py-3", "HB間隔(秒)" }
                                        }
                                    }
                                    tbody {
                                        for cfg in config_list.iter() {
                                            ChannelRow {
                                                config: cfg.clone(),
                                                channels: channels,
                                            }
                                        }
                                    }
                                }
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
fn ChannelRow(config: ChannelConfigDto, channels: Resource<Result<Vec<ChannelConfigDto>, ServerFnError>>) -> Element {
    let badge = if config.whitelisted { "badge-success" } else { "badge-neutral" };
    let label = if config.whitelisted { "有効" } else { "無効" };

    let channel_id = config.channel_id.clone();
    let guild_id = config.guild_id.clone();
    let channel_name = config.channel_name.clone();
    let whitelisted = config.whitelisted;

    let hb_channel_id = config.channel_id.clone();
    let hb_guild_id = config.guild_id.clone();
    let hb_channel_name = config.channel_name.clone();
    let hb_interval_secs = config.heartbeat_interval_secs;

    let hb_channel_id2 = config.channel_id.clone();
    let hb_guild_id2 = config.guild_id.clone();
    let hb_channel_name2 = config.channel_name.clone();
    let hb_enabled2 = config.heartbeat_enabled;
    let mut hb_interval_input = use_signal(|| config.heartbeat_interval_secs.map(|v| v.to_string()).unwrap_or_default());

    rsx! {
        tr { class: "border-b border-outline-variant last:border-b-0 hover:bg-surface-container-high/50 transition-colors",
            td { class: "px-4 py-3",
                div { class: "flex items-center gap-2",
                    span { class: "material-symbols-outlined text-lg text-on-surface-variant", "tag" }
                    span { class: "text-body-lg text-on-surface", "{config.channel_name}" }
                }
            }
            td { class: "px-4 py-3",
                span { class: "text-body-md text-on-surface-variant font-mono", "{config.channel_id}" }
            }
            td { class: "px-4 py-3 text-center",
                button {
                    class: "{badge} cursor-pointer hover:opacity-80 transition-opacity",
                    onclick: move |_| {
                        let cid = channel_id.clone();
                        let gid = guild_id.clone();
                        let cname = channel_name.clone();
                        let new_val = !whitelisted;
                        let mut channels = channels.clone();
                        spawn(async move {
                            let _ = set_channel_whitelisted(cid, gid, cname, new_val).await;
                            channels.restart();
                        });
                    },
                    "{label}"
                }
            }
            td { class: "px-4 py-3 text-center",
                input {
                    r#type: "checkbox",
                    class: "w-4 h-4 cursor-pointer",
                    checked: config.heartbeat_enabled,
                    onchange: move |e| {
                        let cid = hb_channel_id.clone();
                        let gid = hb_guild_id.clone();
                        let cname = hb_channel_name.clone();
                        let enabled = e.checked();
                        let interval = hb_interval_secs;
                        let mut channels = channels.clone();
                        spawn(async move {
                            let _ = set_channel_heartbeat_config(cid, gid, cname, enabled, interval).await;
                            channels.restart();
                        });
                    }
                }
            }
            td { class: "px-4 py-3",
                input {
                    r#type: "number",
                    class: "input-outlined w-28 text-sm",
                    placeholder: "デフォルト",
                    value: "{hb_interval_input}",
                    min: "60",
                    oninput: move |e| {
                        hb_interval_input.set(e.value());
                    },
                    onblur: move |_| {
                        let cid = hb_channel_id2.clone();
                        let gid = hb_guild_id2.clone();
                        let cname = hb_channel_name2.clone();
                        let enabled = hb_enabled2;
                        let val = hb_interval_input.read().clone();
                        let interval = val.parse::<u64>().ok();
                        let mut channels = channels.clone();
                        spawn(async move {
                            let _ = set_channel_heartbeat_config(cid, gid, cname, enabled, interval).await;
                            channels.restart();
                        });
                    }
                }
            }
        }
    }
}
