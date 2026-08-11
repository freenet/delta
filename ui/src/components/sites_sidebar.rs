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
        aside { class: "w-40 md:w-56 flex flex-col h-full bg-bg border-r border-border",

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
                            SiteRow {
                                key: "{prefix}",
                                prefix: prefix.to_string(),
                                name: site.name.clone(),
                                site_prefix: site.prefix.clone(),
                                is_selected: current_prefix.as_deref() == Some(prefix.as_str()),
                            }
                        }
                    }
                }

                if !visited.is_empty() {
                    div {
                        p { class: "section-label mb-2", "Visited" }
                        for (prefix, site) in visited.iter() {
                            SiteRow {
                                key: "{prefix}",
                                prefix: prefix.to_string(),
                                name: site.name.clone(),
                                site_prefix: site.prefix.clone(),
                                is_selected: current_prefix.as_deref() == Some(prefix.as_str()),
                            }
                        }
                    }
                }
            }

            // Add site button + build info
            div { class: "px-3 py-3 border-t border-border",
                button {
                    class: "w-full px-3 py-2 text-xs text-text-muted hover:text-accent border border-border hover:border-accent rounded-lg transition-colors mb-2",
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

#[component]
fn SiteRow(prefix: String, name: String, site_prefix: String, is_selected: bool) -> Element {
    let prefix_owned = prefix.clone();
    let prefix_for_remove = prefix.clone();
    let mut confirming_remove = use_signal(|| false);

    let row_class = if is_selected {
        "site-selected bg-surface"
    } else {
        "hover:bg-surface-hover"
    };

    rsx! {
        div { class: "group relative flex items-center {row_class} transition-all-fast",
            button {
                class: "w-full px-3 py-2 text-left",
                onclick: move |_| {
                    confirming_remove.set(false);
                    state::select_site(&prefix_owned);
                },
                div { class: "text-sm text-text-light truncate font-medium", "{name}" }
                div { class: "text-[10px] text-text-muted font-mono truncate", "{site_prefix}" }
            }
            // Remove: two-click confirm
            if *confirming_remove.read() {
                button {
                    class: "absolute right-1 top-1/2 -translate-y-1/2 px-2 py-1 rounded text-[10px] font-medium bg-red-500/20 text-red-400 hover:bg-red-500/30 transition-colors",
                    onclick: move |evt| {
                        evt.stop_propagation();
                        state::remove_site(&prefix_for_remove);
                    },
                    "Remove?"
                }
            } else {
                button {
                    class: "absolute right-1 top-1/2 -translate-y-1/2 w-6 h-6 flex items-center justify-center rounded text-text-muted hover:text-text hover:bg-surface-hover opacity-0 group-hover:opacity-100 transition-opacity text-sm",
                    title: "Remove site from list",
                    onclick: move |evt| {
                        evt.stop_propagation();
                        confirming_remove.set(true);
                    },
                    "\u{00d7}"
                }
            }
        }
    }
}

const BUILD_TIMESTAMP_ISO: &str = env!("BUILD_TIMESTAMP_ISO", "unknown");

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

#[cfg(test)]
mod tests {
    /// `ui/build.rs` must not hardcode paths inside `.git/`.
    ///
    /// In a git worktree `.git` is a FILE, so `../.git/HEAD` does not resolve,
    /// and Cargo treats an unresolvable `rerun-if-changed` target as
    /// permanently dirty — the build script then re-runs on every build in a
    /// worktree and caches only in the main checkout. That asymmetry hid a
    /// stale-migration-table bug from every investigator following the repo's
    /// own "always use a worktree" convention, because the one layout where
    /// caching was live is the main checkout, where releases are cut.
    ///
    /// Scraping the source is the only instrument available: a build script's
    /// directives are consumed by Cargo, not observable from a unit test. No
    /// self-match risk here — the scraped file is `build.rs`, a different file
    /// from the one this test lives in, so this test's own literals can never
    /// appear in the text being searched.
    #[test]
    fn build_script_resolves_git_paths_instead_of_hardcoding_them() {
        let src = include_str!("../../build.rs");

        assert!(
            !src.contains("rerun-if-changed=../.git/"),
            "build.rs hardcodes a path inside .git/, which does not resolve in \
             a git worktree and makes the build script permanently dirty there. \
             Use `git rev-parse --git-path` so it works in both layouts."
        );
        assert!(
            src.contains("--git-path"),
            "build.rs must resolve git metadata paths via \
             `git rev-parse --git-path` so they are correct in both a normal \
             checkout and a worktree"
        );
    }
}
