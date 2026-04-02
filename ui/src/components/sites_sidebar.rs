use dioxus::prelude::*;

use crate::state;
use crate::state::SiteRole;

#[component]
pub fn SitesSidebar() -> Element {
    let sites = state::SITES.read();
    let current_prefix = (*state::CURRENT_SITE.read()).clone();

    // Separate owned and visited sites
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
        aside { class: "w-52 border-r border-border flex flex-col h-full bg-bg",
            // Header
            div { class: "p-4 border-b border-border",
                h1 { class: "text-base font-bold text-text tracking-tight", "Delta" }
                p { class: "text-xs text-text-muted mt-0.5", "Decentralized sites" }
            }

            // Sites list
            nav { class: "flex-1 overflow-y-auto px-2 py-3",
                // My Sites section
                if !owned.is_empty() {
                    div { class: "mb-4",
                        p { class: "px-2 mb-1 text-[10px] font-semibold text-text-muted uppercase tracking-widest",
                            "My Sites"
                        }
                        for (prefix, site) in owned.iter() {
                            { site_button(prefix, site, &current_prefix) }
                        }
                    }
                }

                // Visited section
                if !visited.is_empty() {
                    div {
                        p { class: "px-2 mb-1 text-[10px] font-semibold text-text-muted uppercase tracking-widest",
                            "Visited"
                        }
                        for (prefix, site) in visited.iter() {
                            { site_button(prefix, site, &current_prefix) }
                        }
                    }
                }
            }

            // Add site
            div { class: "p-3 border-t border-border",
                button {
                    class: "w-full px-3 py-2 text-xs bg-surface text-text-muted rounded-lg hover:bg-surface-hover transition-colors",
                    "+ Add Site"
                }
            }
        }
    }
}

fn site_button(prefix: &str, site: &state::KnownSite, current_prefix: &Option<String>) -> Element {
    let is_selected = current_prefix.as_deref() == Some(prefix);
    let bg = if is_selected {
        "bg-accent-soft text-accent font-medium"
    } else {
        "hover:bg-surface-hover text-text"
    };
    let prefix_owned = prefix.to_string();

    rsx! {
        button {
            class: "w-full text-left px-2.5 py-1.5 rounded-lg text-sm mb-0.5 transition-colors {bg}",
            onclick: move |_| state::select_site(&prefix_owned),
            div { class: "truncate", "{site.name}" }
            div { class: "text-[10px] text-text-muted font-mono truncate", "{site.prefix}" }
        }
    }
}
