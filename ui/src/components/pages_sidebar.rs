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
    sorted_pages.sort_by(|(id_a, a), (id_b, b)| a.order.cmp(&b.order).then(id_a.cmp(id_b)));

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
                                div { class: "group/page flex items-center mb-0.5 transition-all-fast {item_class}",
                                    button {
                                        class: "flex-1 text-left px-3 py-2 text-sm",
                                        onclick: move |_| state::select_page(id),
                                        span { class: "{text_class}", "{page.title}" }
                                    }
                                    // Move up/down arrows (owner only, on hover)
                                    if is_owner && is_selected {
                                        {
                                            // Get sorted page IDs to find neighbors
                                            let sorted_ids: Vec<delta_core::PageId> = state::current_site()
                                                .map(|s| {
                                                    let mut pages: Vec<_> = s.state.pages.iter().collect();
                                                    pages.sort_by(|(ia, a), (ib, b)| a.order.cmp(&b.order).then(ia.cmp(ib)));
                                                    pages.into_iter().map(|(&pid, _)| pid).collect()
                                                })
                                                .unwrap_or_default();
                                            let pos = sorted_ids.iter().position(|&pid| pid == id);
                                            let prev_id = pos.and_then(|p| if p > 0 { sorted_ids.get(p - 1).copied() } else { None });
                                            let next_id = pos.and_then(|p| sorted_ids.get(p + 1).copied());
                                            rsx! {
                                                div { class: "flex flex-col opacity-0 group-hover/page:opacity-100 transition-opacity pr-1",
                                                    if let Some(prev) = prev_id {
                                                        button {
                                                            class: "text-[10px] text-text-muted hover:text-accent px-1 leading-none",
                                                            onclick: move |evt| {
                                                                evt.stop_propagation();
                                                                state::swap_page_order(id, prev);
                                                            },
                                                            "\u{25B2}" // up triangle
                                                        }
                                                    }
                                                    if let Some(next) = next_id {
                                                        button {
                                                            class: "text-[10px] text-text-muted hover:text-accent px-1 leading-none",
                                                            onclick: move |evt| {
                                                                evt.stop_propagation();
                                                                state::swap_page_order(id, next);
                                                            },
                                                            "\u{25BC}" // down triangle
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

            // New page + export key (owner only)
            if is_owner {
                div { class: "px-3 py-3 border-t border-border space-y-2",
                    button {
                        class: "w-full px-3 py-2 text-xs text-text-muted hover:text-accent border border-border hover:border-accent rounded-lg transition-colors",
                        onclick: move |_| {
                            state::create_page("New Page".into());
                        },
                        "+ New Page"
                    }
                    button {
                        class: "w-full px-3 py-1.5 text-[10px] text-text-muted hover:text-accent transition-colors",
                        onclick: move |_| {
                            *crate::components::export_key::SHOW_EXPORT.write() = true;
                        },
                        "Export Site Key"
                    }
                }
            }
        }
    }
}
