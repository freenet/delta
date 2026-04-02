use dioxus::prelude::*;

use crate::freenet_api::ConnectionStatus;
use crate::state;
use crate::state::SiteRole;

#[component]
pub fn SitesSidebar() -> Element {
    let sites = state::SITES.read();
    let current_prefix = (*state::CURRENT_SITE.read()).clone();

    let mut owned: Vec<_> = sites
        .iter()
        .filter(|(_, s)| s.role == SiteRole::Owner)
        .collect();
    let mut visited: Vec<_> = sites
        .iter()
        .filter(|(_, s)| s.role == SiteRole::Visitor)
        .collect();
    owned.sort_by_key(|(_, s)| s.name.to_lowercase());
    visited.sort_by_key(|(_, s)| s.name.to_lowercase());

    rsx! {
        aside { class: "w-56 flex flex-col h-full bg-bg border-r border-border",

            // Logo header
            div { class: "px-4 py-4 border-b border-border",
                div { class: "flex items-center gap-2.5",
                    span { class: "delta-mark", "\u{0394}" }
                    div {
                        span { class: "delta-logo text-text-light text-[17px]", "Delta" }
                        p { class: "text-[10px] text-text-muted leading-tight mt-0.5", "Decentralized publishing" }
                    }
                }
            }

            // Sites list
            nav { class: "flex-1 overflow-y-auto py-3",
                if !owned.is_empty() {
                    div { class: "mb-5",
                        p { class: "section-label mb-2", "My Sites" }
                        for (prefix, site) in owned.iter() {
                            { site_row(prefix, site, &current_prefix) }
                        }
                    }
                }

                if !visited.is_empty() {
                    div {
                        p { class: "section-label mb-2", "Visited" }
                        for (prefix, site) in visited.iter() {
                            { site_row(prefix, site, &current_prefix) }
                        }
                    }
                }
            }

            // Add site button + build info
            div { class: "px-3 py-3 border-t border-border",
                button {
                    class: "btn-secondary w-full px-3 py-2 text-xs mb-2",
                    onclick: move |_| state::show_add_site_prompt(),
                    "+ Add Site"
                }
                {
                    let status = crate::freenet_api::CONNECTION_STATUS.read();
                    let (dot_color, status_text) = match &*status {
                        ConnectionStatus::Connected => ("bg-green-500", "Connected"),
                        ConnectionStatus::Connecting => ("bg-yellow-500", "Connecting..."),
                        ConnectionStatus::Disconnected => ("bg-gray-400", "Offline"),
                        ConnectionStatus::Error(_) => ("bg-red-500", "Error"),
                    };
                    rsx! {
                        div { class: "flex items-center justify-center gap-1.5 mb-1",
                            span { class: "w-1.5 h-1.5 rounded-full {dot_color} inline-block" }
                            span { class: "text-[9px] text-text-muted", "{status_text}" }
                        }
                    }
                }
                p { class: "text-[9px] text-text-muted text-center leading-tight",
                    "Built: {format_build_time_local()}"
                }
            }
        }
    }
}

fn site_row(prefix: &str, site: &state::KnownSite, current_prefix: &Option<String>) -> Element {
    let is_selected = current_prefix.as_deref() == Some(prefix);
    let is_owner = site.role == SiteRole::Owner;
    let prefix_owned = prefix.to_string();

    let row_class = if is_selected {
        "site-selected bg-surface"
    } else {
        "hover:bg-surface-hover"
    };

    let avatar_class = if is_owner {
        "site-avatar site-avatar-owner"
    } else {
        "site-avatar site-avatar-visitor"
    };

    // First letter of site name for avatar
    let initial = site.name.chars().next().unwrap_or('?');

    rsx! {
        button {
            class: "w-full flex items-center gap-2.5 px-3 py-2 text-left transition-all-fast {row_class}",
            onclick: move |_| state::select_site(&prefix_owned),
            span { class: "{avatar_class}", "{initial}" }
            div { class: "min-w-0 flex-1",
                div { class: "text-sm text-text-light truncate font-medium", "{site.name}" }
                div { class: "text-[10px] text-text-muted font-mono truncate", "{site.prefix}" }
            }
        }
    }
}

const BUILD_TIMESTAMP_ISO: &str = env!("BUILD_TIMESTAMP_ISO", "unknown");

/// Convert UTC ISO timestamp to local time using browser's Date API.
fn format_build_time_local() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        use js_sys::Date;
        let date = Date::new(&wasm_bindgen::JsValue::from_str(BUILD_TIMESTAMP_ISO));
        if date.to_string().as_string().is_some() {
            let year = date.get_full_year();
            let month = date.get_month() + 1;
            let day = date.get_date();
            let hours = date.get_hours();
            let minutes = date.get_minutes();
            let offset_min = date.get_timezone_offset() as i32;
            let tz_str = if offset_min == 0 {
                "UTC".to_string()
            } else {
                let sign = if offset_min <= 0 { '+' } else { '-' };
                let abs = offset_min.unsigned_abs();
                format!("UTC{sign}{}", abs / 60)
            };
            format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02} {tz_str}")
        } else {
            BUILD_TIMESTAMP_ISO.to_string()
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        BUILD_TIMESTAMP_ISO.to_string()
    }
}
