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
    let mut ac_top = use_signal(|| 0i32);
    let mut ac_left = use_signal(|| 0i32);
    let mut ac_open_upward = use_signal(|| false);

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

    // Insert a page link
    let mut insert_link = move |id: PageId, _title: &str| {
        let content = state::EDITOR_CONTENT.read().clone();
        let pos = (*cursor_pos.read()).min(content.len());
        let before = &content[..pos];
        if let Some(open) = before.rfind("[[") {
            let after_cursor = &content[pos..];
            let mut new_content = content[..open].to_string();
            new_content.push_str(&format!("[[{id}]]"));
            new_content.push_str(after_cursor);
            *state::EDITOR_CONTENT.write() = new_content;
        }
        ac_visible.set(false);
        ac_query.set(None);
        ac_selected.set(0);
    };

    // Dropdown position style
    let dropdown_style = if *ac_open_upward.read() {
        format!(
            "position: absolute; left: {}px; bottom: calc(100% - {}px); max-height: 180px;",
            *ac_left.read(),
            *ac_top.read()
        )
    } else {
        format!(
            "position: absolute; left: {}px; top: {}px; max-height: 180px;",
            *ac_left.read(),
            *ac_top.read()
        )
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
                            "**bold**  *italic*  `code`  [[ page link  [[id|text]]  [label](url)"
                        }
                    }
                    div { class: "relative flex-1 overflow-hidden",
                        textarea {
                            id: "delta-editor",
                            class: "editor-textarea w-full h-full p-5 resize-none outline-none",
                            value: "{content}",
                            placeholder: "Write your page content in Markdown...",
                            oninput: move |evt| {
                                let text = evt.value().to_string();
                                update_autocomplete(
                                    &text,
                                    &mut ac_query, &mut ac_visible, &mut ac_selected,
                                    &mut cursor_pos, &mut ac_top, &mut ac_left, &mut ac_open_upward,
                                );
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

                        // Autocomplete dropdown positioned near cursor
                        if *ac_visible.read() && !matches.is_empty() {
                            div {
                                class: "bg-panel border border-border-light rounded-lg shadow-lg z-10 overflow-y-auto",
                                style: "{dropdown_style}",
                                div { class: "px-3 py-1 text-[9px] text-text-muted-light border-b border-border-light",
                                    "\u{2191}\u{2193} Enter/Tab to select, Esc cancel"
                                }
                                for (idx, (id, page_title)) in matches.iter().enumerate() {
                                    {
                                        let id = *id;
                                        let page_title_display = page_title.clone();
                                        let page_title_insert = page_title.clone();
                                        let is_highlighted = idx == *ac_selected.read();
                                        let item_class = if is_highlighted {
                                            "w-full text-left px-3 py-1.5 text-sm bg-accent-soft text-accent"
                                        } else {
                                            "w-full text-left px-3 py-1.5 text-sm text-text hover:bg-accent-glow hover:text-accent transition-colors"
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

/// Check if cursor is inside [[ and update autocomplete state + position.
#[allow(clippy::ptr_arg, clippy::too_many_arguments, unused_variables)]
fn update_autocomplete(
    text: &str,
    ac_query: &mut Signal<Option<String>>,
    ac_visible: &mut Signal<bool>,
    ac_selected: &mut Signal<usize>,
    cursor_pos: &mut Signal<usize>,
    ac_top: &mut Signal<i32>,
    ac_left: &mut Signal<i32>,
    ac_open_upward: &mut Signal<bool>,
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

                    // Calculate cursor position using mirror div technique
                    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                        // Create a hidden div that mirrors the textarea
                        if let Ok(mirror) = doc.create_element("div") {
                            let style = [
                                "position: absolute",
                                "visibility: hidden",
                                "white-space: pre-wrap",
                                "word-wrap: break-word",
                                &format!("width: {}px", el.client_width()),
                                &format!(
                                    "font: {}",
                                    el.style().get_property_value("font").unwrap_or_default()
                                ),
                                "font-family: var(--font-family-mono)",
                                "font-size: 0.875rem",
                                "line-height: 1.7",
                                "padding: 20px",
                                "tab-size: 4",
                            ]
                            .join("; ");
                            let _ = mirror.set_attribute("style", &style);

                            // Text up to cursor, with a marker span
                            let text_before = &text[..pos.min(text.len())];
                            mirror.set_inner_html(&format!(
                                "{}<span id='_ac_cursor'>|</span>",
                                text_before.replace('<', "&lt;").replace('>', "&gt;")
                            ));

                            if let Some(body) = doc.body() {
                                let _ = body.append_child(&mirror);

                                if let Some(cursor_span) = doc.get_element_by_id("_ac_cursor") {
                                    let cursor_top =
                                        cursor_span.get_bounding_client_rect().top() as i32;
                                    let cursor_left =
                                        cursor_span.get_bounding_client_rect().left() as i32;
                                    let textarea_rect = el.get_bounding_client_rect();
                                    let ta_top = textarea_rect.top() as i32;
                                    let ta_left = textarea_rect.left() as i32;
                                    let ta_height = textarea_rect.height() as i32;

                                    // Position relative to textarea
                                    let rel_top = cursor_top - ta_top + 24; // below cursor line
                                    let rel_left = (cursor_left - ta_left).max(8).min(200);

                                    // Flip upward if cursor is in bottom half
                                    let flip = rel_top > ta_height / 2;
                                    ac_open_upward.set(flip);

                                    if flip {
                                        ac_top.set(ta_height - (cursor_top - ta_top) + 4);
                                    } else {
                                        ac_top.set(rel_top);
                                    }
                                    ac_left.set(rel_left);
                                }

                                let _ = body.remove_child(&mirror);
                            }
                        }
                    }
                    return;
                }
            }
        }
    }
    ac_visible.set(false);
    ac_query.set(None);
}
