use delta_core::PageId;
use dioxus::prelude::*;

use crate::state;

#[component]
pub fn Editor() -> Element {
    let Some((_page_id, _page)) = state::current_page() else {
        return rsx! {
            div { class: "flex items-center justify-center h-full text-text-muted-light",
                p { "No page selected" }
            }
        };
    };

    use_effect(move || {
        if let Some((_, page)) = state::current_page() {
            *state::EDITOR_TITLE.write() = page.title.clone();
            *state::EDITOR_CONTENT.write() = page.content.clone();
        }
    });

    let title = state::EDITOR_TITLE.read().clone();
    let content = state::EDITOR_CONTENT.read().clone();
    let preview_html = markdown::to_html(&content);

    // Autocomplete state
    let mut autocomplete_query = use_signal(|| None::<String>);
    let mut autocomplete_visible = use_signal(|| false);

    // Check if cursor is inside [[ ]] and extract the query
    let mut check_autocomplete = move |text: &str| {
        // Find the last [[ that isn't closed by ]]
        if let Some(open) = text.rfind("[[") {
            let after = &text[open + 2..];
            if !after.contains("]]") {
                // We're inside an unclosed [[ - the query is everything after [[
                let query = after.to_string();
                autocomplete_query.set(Some(query));
                autocomplete_visible.set(true);
                return;
            }
        }
        autocomplete_visible.set(false);
        autocomplete_query.set(None);
    };

    // Get matching pages for autocomplete
    let matches: Vec<(PageId, String)> = if let Some(query) = &*autocomplete_query.read() {
        let lower = query.to_lowercase();
        state::current_site()
            .map(|site| {
                site.state
                    .pages
                    .iter()
                    .filter(|(_, p)| lower.is_empty() || p.title.to_lowercase().contains(&lower))
                    .map(|(&id, p)| (id, p.title.clone()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    rsx! {
        div { class: "flex flex-col h-full bg-panel",
            // Toolbar
            div { class: "flex items-center gap-3 px-6 py-3 border-b border-border-light",
                input {
                    class: "text-xl bg-transparent border-none outline-none flex-1 text-text placeholder-text-muted-light font-semibold",
                    r#type: "text",
                    value: "{title}",
                    placeholder: "Page title",
                    oninput: move |evt| {
                        *state::EDITOR_TITLE.write() = evt.value().to_string();
                    },
                }
                button {
                    class: "px-4 py-1.5 text-sm text-accent border border-accent hover:bg-accent hover:text-text-inverse rounded-lg transition-colors font-medium",
                    onclick: move |_| state::save_current_page(),
                    "Save"
                }
                button {
                    class: "px-4 py-1.5 text-sm text-text-muted hover:text-text transition-colors rounded",
                    onclick: move |_| {
                        *state::EDITING.write() = false;
                    },
                    "Cancel"
                }
            }

            // Editor + Preview split
            div { class: "flex flex-1 overflow-hidden",
                // Editor pane
                div {
                    class: "relative flex flex-col border-r border-border-light",
                    style: "width: 60%; min-width: 400px;",
                    div { class: "flex items-center justify-between px-4 py-2 border-b border-border-light bg-panel-warm",
                        span { class: "text-[10px] font-semibold text-text-muted-light uppercase tracking-[0.1em]",
                            "Markdown"
                        }
                        // Compact cheat sheet
                        span { class: "text-[9px] text-text-muted font-mono",
                            "# H1  ## H2  **bold**  *italic*  `code`  - list  > quote  [[Page Title]]"
                        }
                    }
                    textarea {
                        class: "editor-textarea flex-1 w-full p-5 resize-none outline-none",
                        value: "{content}",
                        placeholder: "Write your page content in Markdown...",
                        oninput: move |evt| {
                            let text = evt.value().to_string();
                            check_autocomplete(&text);
                            *state::EDITOR_CONTENT.write() = text;
                        },
                    }

                    // Autocomplete dropdown
                    if *autocomplete_visible.read() && !matches.is_empty() {
                        div {
                            class: "absolute left-4 right-4 bottom-4 bg-panel border border-border-light rounded-lg shadow-lg z-10 max-h-48 overflow-y-auto",
                            div { class: "px-3 py-1.5 text-[10px] text-text-muted-light uppercase tracking-wide border-b border-border-light",
                                "Link to page"
                            }
                            for (id, page_title) in matches.iter() {
                                {
                                    let id = *id;
                                    let page_title = page_title.clone();
                                    let page_title_for_insert = page_title.clone();
                                    rsx! {
                                        button {
                                            class: "w-full text-left px-3 py-2 text-sm text-text hover:bg-accent-glow hover:text-accent transition-colors",
                                            onclick: move |_| {
                                                // Replace the [[ ... with [[id|title]]
                                                let mut content = state::EDITOR_CONTENT.read().clone();
                                                if let Some(pos) = content.rfind("[[") {
                                                    content.truncate(pos);
                                                    content.push_str(&format!("[[{id}|{page_title_for_insert}]]"));
                                                    *state::EDITOR_CONTENT.write() = content;
                                                }
                                                autocomplete_visible.set(false);
                                                autocomplete_query.set(None);
                                            },
                                            "{page_title}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Preview pane
                div {
                    class: "flex flex-col bg-panel min-w-0 overflow-hidden",
                    style: "flex: 1;",
                    div { class: "px-4 py-2 text-[10px] font-semibold text-text-muted-light border-b border-border-light uppercase tracking-[0.1em] bg-panel-warm",
                        "Preview"
                    }
                    div { class: "flex-1 overflow-y-auto p-8",
                        div {
                            class: "prose max-w-none",
                            dangerous_inner_html: "{preview_html}"
                        }
                    }
                }
            }
        }
    }
}
