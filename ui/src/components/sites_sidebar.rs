use dioxus::prelude::*;

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
                p { class: "text-[9px] text-text-muted text-center leading-tight",
                    "Built: {env!(\"BUILD_TIMESTAMP\")}"
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
