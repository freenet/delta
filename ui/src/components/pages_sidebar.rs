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
            // Site name header with share button
            div { class: "px-4 py-4 border-b border-border",
                h2 { class: "text-sm font-semibold text-text-light truncate",
                    "{site.name}"
                }
                if !site.state.config.config.description.is_empty() {
                    p { class: "text-[11px] text-text-muted mt-0.5 truncate",
                        "{site.state.config.config.description}"
                    }
                }
                // Share / copy code button
                {
                    let prefix = site.prefix.clone();
                    let mut copied = use_signal(|| false);
                    rsx! {
                        button {
                            class: "mt-2 w-full flex items-center justify-center gap-1.5 px-2 py-1.5 rounded-lg bg-accent-glow hover:bg-accent-soft text-accent text-xs font-mono tracking-wide transition-colors",
                            title: "Copy site code to share",
                            onclick: move |_| {
                                copy_to_clipboard(&prefix);
                                copied.set(true);
                            },
                            if *copied.read() {
                                "Copied!"
                            } else {
                                "Share: {site.prefix}"
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
                            "page-item page-item-selected"
                        } else {
                            "page-item hover:bg-surface-hover"
                        };
                        let text_class = if is_selected {
                            "text-text font-medium"
                        } else {
                            "text-text-light"
                        };
                        rsx! {
                            button {
                                class: "w-full text-left px-3 py-2 rounded-lg text-sm mb-0.5 transition-all-fast {item_class}",
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
                        class: "btn-primary w-full px-3 py-2 text-sm",
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

fn copy_to_clipboard(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let clipboard = window.navigator().clipboard();
            let _ = clipboard.write_text(text);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}
