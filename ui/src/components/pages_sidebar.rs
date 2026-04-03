use dioxus::prelude::*;

use crate::state;
use crate::state::SiteRole;

#[component]
pub fn PagesSidebar() -> Element {
    let Some(site) = state::current_site() else {
        return rsx! {
            aside { class: "w-52 flex flex-col h-full bg-bg-warm border-r border-border",
                div { class: "flex items-center justify-center h-full",
                    p { class: "text-sm text-text-muted italic", "Select a site" }
                }
            }
        };
    };

    let current_page = *state::CURRENT_PAGE.read();
    let is_owner = site.role == SiteRole::Owner;

    let mut sorted_pages: Vec<_> = site.state.pages.iter().collect();
    sorted_pages.sort_by_key(|(id, _)| *id);

    rsx! {
        aside { class: "w-52 flex flex-col h-full bg-bg-warm border-r border-border",
            // Site name header - click to edit (owner only)
            {
                let mut editing_name = use_signal(|| false);
                let mut name_input = use_signal(|| site.name.clone());
                let site_name = site.name.clone();
                let prefix_for_rename = site.prefix.clone();
                rsx! {
                    div { class: "px-4 py-4 border-b border-border",
                        if *editing_name.read() && is_owner {
                            input {
                                class: "text-sm font-semibold bg-transparent border-b border-accent text-text-light outline-none w-full",
                                r#type: "text",
                                value: "{name_input}",
                                autofocus: true,
                                oninput: move |evt| name_input.set(evt.value().to_string()),
                                onkeypress: move |evt| {
                                    if evt.key() == Key::Enter {
                                        let new_name = name_input.read().clone();
                                        if !new_name.trim().is_empty() {
                                            state::rename_site(&prefix_for_rename, new_name);
                                        }
                                        editing_name.set(false);
                                    } else if evt.key() == Key::Escape {
                                        editing_name.set(false);
                                    }
                                },
                            }
                        } else {
                            h2 {
                                class: if is_owner { "text-sm font-semibold text-text-light truncate cursor-pointer hover:text-accent transition-colors" } else { "text-sm font-semibold text-text-light truncate" },
                                onclick: move |_| {
                                    if is_owner {
                                        name_input.set(site_name.clone());
                                        editing_name.set(true);
                                    }
                                },
                                "{site.name}"
                                if is_owner {
                                    span { class: "text-text-muted text-[10px] ml-1 opacity-0 group-hover:opacity-100", "(edit)" }
                                }
                            }
                        }
                        if !site.state.config.config.description.is_empty() {
                            p { class: "text-[11px] text-text-muted mt-0.5 truncate",
                                "{site.state.config.config.description}"
                            }
                        }
                    }
                }
            }

            // Pages list
            nav { class: "flex-1 overflow-y-auto py-2 px-2",
                p { class: "section-label mb-2 px-0", "Pages" }
                for (&id, page) in sorted_pages.iter() {
                    {
                        let is_selected = current_page == Some(id);
                        let item_class = if is_selected {
                            "bg-accent-soft border-l-2 border-accent rounded-r-lg"
                        } else {
                            "hover:bg-surface-hover rounded-lg"
                        };
                        let text_class = if is_selected {
                            "text-accent font-medium"
                        } else {
                            "text-text-light"
                        };
                        rsx! {
                            button {
                                class: "w-full text-left px-3 py-2 text-sm mb-0.5 transition-all-fast {item_class}",
                                onclick: move |_| state::select_page(id),
                                span { class: "{text_class}", "{page.title}" }
                            }
                        }
                    }
                }
            }

            // New page (owner only)
            if is_owner {
                div { class: "px-3 py-3 border-t border-border",
                    button {
                        class: "w-full px-3 py-2 text-xs text-text-muted hover:text-accent border border-border hover:border-accent rounded-lg transition-colors",
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
