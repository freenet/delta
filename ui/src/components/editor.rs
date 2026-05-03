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
    let preview_html = markdown::to_html_with_options(&content, &markdown::Options::gfm())
        .unwrap_or_else(|_| markdown::to_html(&content));
    // Inject heading ids so in-page anchor links (`[Link](#heading)`) work
    // in the live preview as well as in the rendered page view, and
    // beautify any Freenet URLs the user has typed. Honor the same
    // gateway-detection flag the rendered page view uses — passing
    // `true` unconditionally would show a same-origin `/v1/contract/
    // web/...` path in `dx serve` previews where there's no gateway
    // behind Delta to resolve it.
    let preview_html = super::page_view::inject_heading_ids(&preview_html);
    let preview_html =
        super::page_view::finalize_anchors(&preview_html, super::page_view::behind_gateway());

    // Autocomplete state
    let mut ac_query = use_signal(|| None::<String>);
    let mut ac_visible = use_signal(|| false);
    let mut ac_selected = use_signal(|| 0usize);
    let mut cursor_pos = use_signal(|| 0usize);
    let mut ac_top = use_signal(|| 0i32);
    let mut ac_left = use_signal(|| 0i32);
    let mut ac_open_upward = use_signal(|| false);

    // Get matching pages
    let current_page_id = *state::CURRENT_PAGE.read();
    let matches: Vec<(PageId, String)> = if let Some(query) = &*ac_query.read() {
        let lower = query.to_lowercase();
        state::current_site()
            .map(|site| {
                site.state
                    .pages
                    .iter()
                    .filter(|(&id, p)| {
                        // Exclude the current page
                        Some(id) != current_page_id
                            && (lower.is_empty() || p.title.to_lowercase().contains(&lower))
                    })
                    .map(|(&id, p)| (id, p.title.clone()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Add "Create new page" option if query is non-empty and doesn't exactly match
    let ac_query_val = ac_query.read().clone().unwrap_or_default();
    let show_create = !ac_query_val.is_empty()
        && !matches
            .iter()
            .any(|(_, t)| t.to_lowercase() == ac_query_val.to_lowercase());
    // Total items: matches + optional create
    let match_count = matches.len() + if show_create { 1 } else { 0 };

    // Insert a page link (or create a new page)
    let mut insert_link = move |id: PageId, title: &str| {
        let content = state::EDITOR_CONTENT.read().clone();
        // `cursor_pos` is a UTF-8 byte offset stored by
        // `update_autocomplete`. Clamp to a valid char boundary in
        // case the content was edited between the input event and
        // this callback firing; slicing inside a multi-byte char
        // would panic with "unreachable" in WASM.
        let mut pos = (*cursor_pos.read()).min(content.len());
        while pos > 0 && !content.is_char_boundary(pos) {
            pos -= 1;
        }
        let before = &content[..pos];

        if id == u64::MAX {
            // Create a new page with this title, then insert link
            let new_title = title.to_string();
            state::create_page(new_title);
            // The new page ID is the one just created
            if let Some(site) = state::current_site() {
                let new_id = site.state.next_page_id - 1;
                if let Some(open) = before.rfind("[[") {
                    let after_cursor = &content[pos..];
                    let mut new_content = content[..open].to_string();
                    new_content.push_str(&format!("[[{new_id}]]"));
                    new_content.push_str(after_cursor);
                    *state::EDITOR_CONTENT.write() = new_content;
                }
            }
            // Switch back to the page we were editing
            // (create_page switches to the new page and opens editor)
            *state::EDITING.write() = false;
        } else if let Some(open) = before.rfind("[[") {
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

    let _ = (ac_top, ac_left, ac_open_upward); // no longer used for positioning

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
                                        if sel < matches.len() {
                                            if let Some((id, title)) = matches.get(sel) {
                                                insert_link(*id, title);
                                            }
                                        } else {
                                            // "Create" option selected
                                            let q = ac_query.read().clone().unwrap_or_default();
                                            if !q.is_empty() {
                                                insert_link(u64::MAX, &q);
                                            }
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

                        // Autocomplete dropdown - centered in editor
                        if *ac_visible.read() && match_count > 0 {
                            div {
                                class: "bg-panel border border-border-light rounded-lg shadow-lg overflow-y-auto z-50",
                                style: "position: absolute; top: 50%; left: 50%; transform: translate(-50%, -50%); max-height: 200px; min-width: 220px; max-width: 320px;",
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
                                // "Create new page" option
                                if show_create {
                                    {
                                        let create_title = ac_query_val.clone();
                                        let is_highlighted = matches.len() == *ac_selected.read();
                                        let item_class = if is_highlighted {
                                            "w-full text-left px-3 py-1.5 text-sm bg-accent-soft text-accent border-t border-border-light"
                                        } else {
                                            "w-full text-left px-3 py-1.5 text-sm text-text-muted hover:bg-accent-glow hover:text-accent transition-colors border-t border-border-light"
                                        };
                                        rsx! {
                                            button {
                                                class: "{item_class}",
                                                onmousedown: move |evt| {
                                                    evt.prevent_default();
                                                    insert_link(u64::MAX, &create_title);
                                                },
                                                "+ Create \"{ac_query_val}\""
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

/// Convert a JavaScript UTF-16 code-unit offset (as returned by
/// `HtmlTextAreaElement::selection_start`) to a UTF-8 byte offset
/// suitable for slicing a Rust `&str`.
///
/// JS strings count UTF-16 code units; Rust strings are UTF-8 byte
/// sequences. Slicing `text[..utf16_pos]` directly panics with
/// "unreachable" in WASM whenever the cursor lands inside or after
/// any non-ASCII character (e.g. `·`, `é`, emoji, CJK). The result
/// is always on a UTF-8 char boundary.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn utf16_to_byte_index(s: &str, utf16_pos: usize) -> usize {
    let mut utf16 = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        if utf16_pos <= utf16 {
            return byte_idx;
        }
        let next = utf16 + ch.len_utf16();
        if utf16_pos < next {
            // The position lies inside a surrogate pair (rare;
            // browsers don't normally put cursors between halves of
            // a pair). Snap to the start of the char so the
            // resulting byte index is on a valid UTF-8 boundary.
            return byte_idx;
        }
        utf16 = next;
    }
    s.len()
}

/// Check if cursor is inside [[ and update autocomplete state.
#[allow(clippy::ptr_arg, clippy::too_many_arguments, unused_variables)]
fn update_autocomplete(
    text: &str,
    ac_query: &mut Signal<Option<String>>,
    ac_visible: &mut Signal<bool>,
    ac_selected: &mut Signal<usize>,
    cursor_pos: &mut Signal<usize>,
    _ac_top: &mut Signal<i32>,
    _ac_left: &mut Signal<i32>,
    _ac_open_upward: &mut Signal<bool>,
) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("delta-editor"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
        {
            let utf16_pos = el.selection_start().ok().flatten().unwrap_or(0) as usize;
            let byte_pos = utf16_to_byte_index(text, utf16_pos);
            *cursor_pos.write() = byte_pos;

            let before_cursor = &text[..byte_pos];
            if let Some(open) = before_cursor.rfind("[[") {
                let between = &before_cursor[open + 2..];
                if !between.contains("]]") && !between.contains('\n') {
                    web_sys::console::log_1(
                        &format!(
                            "Delta: autocomplete triggered, query='{}' pos={}",
                            between, byte_pos
                        )
                        .into(),
                    );
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
}

#[cfg(test)]
mod tests {
    use super::utf16_to_byte_index;

    #[test]
    fn ascii_offsets_match_byte_offsets() {
        assert_eq!(utf16_to_byte_index("abc", 0), 0);
        assert_eq!(utf16_to_byte_index("abc", 1), 1);
        assert_eq!(utf16_to_byte_index("abc", 3), 3);
    }

    #[test]
    fn two_byte_utf8_char_advances_by_two_bytes_per_unit() {
        // "·" is U+00B7: 1 UTF-16 unit, 2 UTF-8 bytes.
        let s = "a·b";
        assert_eq!(utf16_to_byte_index(s, 0), 0);
        assert_eq!(utf16_to_byte_index(s, 1), 1); // after 'a'
        assert_eq!(utf16_to_byte_index(s, 2), 3); // after '·' (skip its 2 bytes)
        assert_eq!(utf16_to_byte_index(s, 3), 4); // after 'b'
    }

    #[test]
    fn three_byte_utf8_char_advances_by_three_bytes_per_unit() {
        // "汉" is U+6C49: 1 UTF-16 unit, 3 UTF-8 bytes.
        let s = "a汉b";
        assert_eq!(utf16_to_byte_index(s, 1), 1);
        assert_eq!(utf16_to_byte_index(s, 2), 4);
        assert_eq!(utf16_to_byte_index(s, 3), 5);
    }

    #[test]
    fn surrogate_pair_consumes_two_utf16_units() {
        // "🎉" is U+1F389: surrogate pair (2 UTF-16 units), 4 UTF-8 bytes.
        let s = "a🎉b";
        assert_eq!(utf16_to_byte_index(s, 1), 1); // after 'a'
        assert_eq!(utf16_to_byte_index(s, 2), 1); // mid-surrogate, snap to char start
        assert_eq!(utf16_to_byte_index(s, 3), 5); // after '🎉'
        assert_eq!(utf16_to_byte_index(s, 4), 6); // after 'b'
    }

    #[test]
    fn out_of_range_clamps_to_string_len() {
        let s = "a·b";
        assert_eq!(utf16_to_byte_index(s, 100), s.len());
    }

    #[test]
    fn zero_pos_returns_zero() {
        assert_eq!(utf16_to_byte_index("", 0), 0);
        assert_eq!(utf16_to_byte_index("·", 0), 0);
    }

    #[test]
    fn slicing_with_returned_index_never_panics() {
        // Regression: the previous code did `&text[..selection_start]`,
        // which panics inside any non-ASCII char. With the converter
        // every reachable cursor position must yield a valid char
        // boundary regardless of what is to the left.
        let cases = ["·abc", "a·b·c", "汉a汉", "🎉x🎉", "café"];
        for s in cases {
            // Use the largest UTF-16 length the string can produce so
            // we stress every code-unit position 0..=utf16_len.
            let utf16_len: usize = s.chars().map(|c| c.len_utf16()).sum();
            for pos in 0..=utf16_len + 2 {
                let idx = utf16_to_byte_index(s, pos);
                let _ = &s[..idx]; // must not panic
            }
        }
    }
}
