use dioxus::prelude::*;

use crate::state;
use crate::state::SiteRole;

#[component]
pub fn PagesSidebar() -> Element {
    let Some(site) = state::current_site() else {
        return rsx! {
            aside { class: "w-56 border-r border-border flex flex-col h-full bg-surface",
                div { class: "flex items-center justify-center h-full text-text-muted text-sm",
                    "Select a site"
                }
            }
        };
    };

    let current_page = *state::CURRENT_PAGE.read();
    let is_owner = site.role == SiteRole::Owner;

    let mut sorted_pages: Vec<_> = site.state.pages.iter().collect();
    sorted_pages.sort_by_key(|(id, _)| *id);

    rsx! {
        aside { class: "w-56 border-r border-border flex flex-col h-full bg-surface",
            // Site header
            div { class: "p-4 border-b border-border",
                h2 { class: "text-sm font-bold text-text truncate",
                    "{site.name}"
                }
                if !site.state.config.config.description.is_empty() {
                    p { class: "text-xs text-text-muted mt-0.5 truncate",
                        "{site.state.config.config.description}"
                    }
                }
            }

            // Pages list
            nav { class: "flex-1 overflow-y-auto p-2",
                for (&id, page) in sorted_pages.iter() {
                    {
                        let is_selected = current_page == Some(id);
                        let bg = if is_selected {
                            "bg-panel text-text font-medium shadow-sm"
                        } else {
                            "hover:bg-surface-hover text-text"
                        };
                        rsx! {
                            button {
                                class: "w-full text-left px-3 py-2 rounded-lg text-sm mb-0.5 transition-colors {bg}",
                                onclick: move |_| state::select_page(id),
                                "{page.title}"
                            }
                        }
                    }
                }
            }

            // New page button (only for owners)
            if is_owner {
                div { class: "p-3 border-t border-border",
                    button {
                        class: "w-full px-3 py-2 text-sm bg-accent text-text-inverse rounded-lg hover:bg-accent-hover font-medium transition-colors",
                        onclick: move |_| {
                            state::create_page("New Page".into());
                        },
                        "+ New Page"
                    }
                }
            }
        }
    }
}
