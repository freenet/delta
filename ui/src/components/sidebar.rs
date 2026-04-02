use dioxus::prelude::*;

use crate::state;

#[component]
pub fn Sidebar() -> Element {
    let site = state::SITE.read();
    let current = *state::CURRENT_PAGE.read();

    let mut sorted_pages: Vec<_> = site.pages.iter().collect();
    sorted_pages.sort_by_key(|(id, _)| *id);

    rsx! {
        aside { class: "w-64 border-r border-border flex flex-col h-full bg-surface",
            // Site header
            div { class: "p-4 border-b border-border",
                h1 { class: "text-lg font-bold text-text truncate",
                    "{site.config.config.name}"
                }
                if !site.config.config.description.is_empty() {
                    p { class: "text-xs text-text-muted mt-1 truncate",
                        "{site.config.config.description}"
                    }
                }
            }

            // Page list
            nav { class: "flex-1 overflow-y-auto p-2",
                for (&id, page) in sorted_pages.iter() {
                    {
                        let is_selected = current == Some(id);
                        let bg = if is_selected { "bg-accent-soft text-accent font-medium" } else { "hover:bg-surface-hover text-text" };
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

            // New page button
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
