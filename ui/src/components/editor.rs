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
    let mut ac_query = use_signal(|| None::<String>);
    let mut ac_visible = use_signal(|| false);
    let mut ac_selected = use_signal(|| 0usize);
    let mut cursor_pos = use_signal(|| 0usize);

    // Get matching pages
    let matches: Vec<(PageId, String)> = if let Some(query) = &*ac_query.read() {
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
    let match_count = matches.len();

    // Insert a page link at the current cursor position
    let mut insert_link = move |id: PageId, title: &str| {
        let content = state::EDITOR_CONTENT.read().clone();
        let pos = (*cursor_pos.read()).min(content.len());
        let before = &content[..pos];
        if let Some(open) = before.rfind("[[") {
            let after_cursor = &content[pos..];
            let mut new_content = content[..open].to_string();
            new_content.push_str(&format!("[[{id}|{title}]]"));
            new_content.push_str(after_cursor);
            *state::EDITOR_CONTENT.write() = new_content;
        }
        ac_visible.set(false);
        ac_query.set(None);
        ac_selected.set(0);
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
                        span { class: "text-[9px] text-text-muted font-mono",
                            "**bold**  *italic*  `code`  [[Page Title]]"
                        }
                    }
                    div { class: "relative flex-1",
                        textarea {
                            id: "delta-editor",
                            class: "editor-textarea w-full h-full p-5 resize-none outline-none",
                            value: "{content}",
                            placeholder: "Write your page content in Markdown...",
                            oninput: move |evt| {
                                let text = evt.value().to_string();
                                update_autocomplete(&text, &mut ac_query, &mut ac_visible, &mut ac_selected, &mut cursor_pos);
                                *state::EDITOR_CONTENT.write() = text;
                            },
                            onkeydown: move |evt| {
                                if !*ac_visible.read() || match_count == 0 {
                                    return;
                                }
                                let sel = *ac_selected.read();
                                match evt.key() {
                                    Key::ArrowDown => {
                                        evt.prevent_default();
                                        ac_selected.set((sel + 1) % match_count);
                                    }
                                    Key::ArrowUp => {
                                        evt.prevent_default();
                                        if sel == 0 {
                                            ac_selected.set(match_count - 1);
                                        } else {
                                            ac_selected.set(sel - 1);
                                        }
                                    }
                                    Key::Tab | Key::Enter => {
                                        evt.prevent_default();
                                        // Insert the selected match
                                        let matches: Vec<(PageId, String)> = if let Some(query) = &*ac_query.read() {
                                            let lower = query.to_lowercase();
                                            state::current_site()
                                                .map(|site| {
                                                    site.state.pages.iter()
                                                        .filter(|(_, p)| lower.is_empty() || p.title.to_lowercase().contains(&lower))
                                                        .map(|(&id, p)| (id, p.title.clone()))
                                                        .collect()
                                                })
                                                .unwrap_or_default()
                                        } else {
                                            Vec::new()
                                        };
                                        if let Some((id, title)) = matches.get(sel) {
                                            insert_link(*id, title);
                                        }
                                    }
                                    Key::Escape => {
                                        ac_visible.set(false);
                                        ac_query.set(None);
                                        ac_selected.set(0);
                                    }
                                    _ => {}
                                }
                            },
                        }

                        // Autocomplete dropdown
                        if *ac_visible.read() && !matches.is_empty() {
                            div {
                                class: "absolute left-4 right-4 bg-panel border border-border-light rounded-lg shadow-lg z-10 max-h-48 overflow-y-auto",
                                style: "bottom: 12px;",
                                div { class: "px-3 py-1.5 text-[10px] text-text-muted-light border-b border-border-light",
                                    "Link to page - \u{2191}\u{2193} navigate, Enter/Tab select, Esc cancel"
                                }
                                for (idx, (id, page_title)) in matches.iter().enumerate() {
                                    {
                                        let id = *id;
                                        let page_title_display = page_title.clone();
                                        let page_title_insert = page_title.clone();
                                        let is_highlighted = idx == *ac_selected.read();
                                        let item_class = if is_highlighted {
                                            "w-full text-left px-3 py-2 text-sm bg-accent-soft text-accent"
                                        } else {
                                            "w-full text-left px-3 py-2 text-sm text-text hover:bg-accent-glow hover:text-accent transition-colors"
                                        };
                                        rsx! {
                                            button {
                                                class: "{item_class}",
                                                onmousedown: move |evt| {
                                                    evt.prevent_default();
                                                    insert_link(id, &page_title_insert);
                                                },
                                                "{page_title_display}"
                                            }
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

/// Check if cursor is inside [[ and update autocomplete state.
#[allow(clippy::ptr_arg)]
fn update_autocomplete(
    text: &str,
    ac_query: &mut Signal<Option<String>>,
    ac_visible: &mut Signal<bool>,
    ac_selected: &mut Signal<usize>,
    cursor_pos: &mut Signal<usize>,
) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("delta-editor"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
        {
            let pos = el.selection_start().ok().flatten().unwrap_or(0) as usize;
            *cursor_pos.write() = pos;

            let before_cursor = &text[..pos.min(text.len())];
            if let Some(open) = before_cursor.rfind("[[") {
                let between = &before_cursor[open + 2..];
                if !between.contains("]]") && !between.contains('\n') {
                    ac_query.set(Some(between.to_string()));
                    ac_visible.set(true);
                    ac_selected.set(0);
                    return;
                }
            }
        }
    }
    ac_visible.set(false);
    ac_query.set(None);
    let _ = (text, cursor_pos);
}
