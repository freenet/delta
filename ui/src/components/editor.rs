use delta_core::PageId;
use dioxus::prelude::*;

use crate::state;

/// What the edit buffers were last seeded with: which page, and the exact
/// text written into them.
///
/// The values matter as much as the identity. Comparing the live buffers
/// against them is how the effect tells "the user has typed" from "the
/// buffers are still exactly what we put there", which is what lets it keep
/// following the persisted page until the moment the user actually edits.
#[derive(PartialEq)]
struct SeededFrom {
    identity: (String, PageId),
    title: String,
    content: String,
}

#[component]
pub fn Editor() -> Element {
    // What the edit buffers were last seeded with. `None` until the first
    // seeding run.
    //
    // Declared before the "no page selected" early return purely so the
    // seeding effect keeps running when there is briefly no current page
    // (e.g. the page was deleted out from under an open editor) — with the
    // return first, the effect would not exist on those renders. This is NOT
    // a hook-ordering fix: seven `use_signal` calls still sit below that
    // return, so the component genuinely calls a different number of hooks
    // depending on `current_page()`. That is safe here because dioxus 0.7.4
    // indexes hooks per render and resets the index each pass, so an early
    // return simply truncates the list to a prefix and the indices realign
    // next render. Do not read this comment as a licence to assume hook
    // order is unconditional in this component — it is not.
    let mut seeded = use_signal(|| None::<SeededFrom>);

    // Seed the edit buffers from the persisted page — but never over text the
    // user has typed.
    //
    // This effect reads CURRENT_SITE, CURRENT_PAGE and SITES (the last via
    // `state::current_page`), which subscribes it to all three. Dioxus's
    // `Signal::write()` drop guard calls `update_subscribers()`
    // unconditionally — it never compares the old value against the new — so
    // ANY write to SITES re-runs this effect, including a write that changes
    // nothing at all. Several handlers take the SITES write guard before
    // checking whether they have anything to change (`freenet_api::delegate`,
    // `freenet_api::operations`), and after load the legacy delegate sweep
    // and the freenet-migrate walk land a multi-second trickle of them.
    //
    // Seeding on every run therefore overwrote whatever the user had typed
    // with the persisted page text a few seconds into an edit. There is no
    // draft buffer, so the typing was simply gone: freenet/delta#62, reported
    // by Ivvor on 2026-08-12 ("the page would be reset to the current
    // contract state version, overwriting all my edits").
    //
    // Guarding on `!EDITING` instead would be both wrong and useless:
    // `Editor` is only rendered while `EDITING` is true (see
    // `components::App`), so the guard would be false on every single run and
    // the buffers would never be seeded at all.
    //
    // Keying only on the page IDENTITY is not enough either, and gets the
    // same class of bug back with a different victim. If the editor opened on
    // the delegate's backed-up (older) text and the network GET landed a
    // moment later, an identity-only guard would leave the buffers on the
    // stale text. The user then types one character and saves — and
    // `state::save_current_page` routes through `next_page_updated_at`
    // (`state.rs`), which reads the CURRENT `updated_at` out of SITES and
    // returns `max(now, existing + 1)`. The save therefore strictly dominates
    // the newer generation that had already arrived and silently reverts it,
    // with no conflict UI. Same thing long-lived when another device's edit
    // arrives via `handle_site_delta`.
    //
    // So the rule is: re-seed when the page identity is stale, OR when the
    // buffers still hold exactly what was last seeded into them — the latter
    // meaning the user has not typed, so following the persisted text is both
    // safe and necessary to keep the save base current. Once they type, the
    // buffers stop matching and nothing overwrites them until they save or
    // cancel.
    //
    // `seeded` is component-local, so closing the editor and reopening it
    // re-seeds from the persisted text; a cancelled draft does not resurrect.
    use_effect(move || {
        let Some(prefix) = (*state::CURRENT_SITE.read()).clone() else {
            return;
        };
        let Some((page_id, page)) = state::current_page() else {
            return;
        };
        let identity = (prefix, page_id);

        // `peek` throughout, deliberately. A normal read would subscribe this
        // effect to signals it also writes: `seeded` would re-run it every
        // time it seeds, and the editor buffers would re-run it on every
        // keystroke.
        let reseed = match &*seeded.peek() {
            None => true,
            Some(prev) => {
                prev.identity != identity
                    || (*state::EDITOR_TITLE.peek() == prev.title
                        && *state::EDITOR_CONTENT.peek() == prev.content)
            }
        };
        if !reseed {
            return;
        }

        // Write only on a real difference. Nothing downstream depends on the
        // notification, and skipping the no-op write avoids re-rendering the
        // editor (and re-running the markdown preview pipeline) on every one
        // of the no-op SITES writes that caused this bug.
        if *state::EDITOR_TITLE.peek() != page.title {
            *state::EDITOR_TITLE.write() = page.title.clone();
        }
        if *state::EDITOR_CONTENT.peek() != page.content {
            *state::EDITOR_CONTENT.write() = page.content.clone();
        }
        seeded.set(Some(SeededFrom {
            identity,
            title: page.title.clone(),
            content: page.content.clone(),
        }));
    });

    let Some((_page_id, _page)) = state::current_page() else {
        return rsx! {
            div { class: "flex items-center justify-center h-full text-text-muted-light",
                p { "No page selected" }
            }
        };
    };

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
    let preview_html = super::page_view::finalize_anchors(
        &preview_html,
        super::page_view::behind_gateway(),
        super::page_view::own_contract_id().as_deref(),
    );

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
            div { class: "flex items-center gap-2 md:gap-3 px-4 md:px-6 py-3 border-b border-border-light",
                input {
                    // `min-w-0` lets the title shrink so the Save/Cancel
                    // buttons stay visible on narrow (mobile) screens.
                    class: "text-xl bg-transparent border-none outline-none flex-1 min-w-0 text-text placeholder-text-muted-light font-semibold",
                    r#type: "text",
                    value: "{title}",
                    placeholder: "Page title",
                    oninput: move |evt| {
                        *state::EDITOR_TITLE.write() = evt.value().to_string();
                    },
                }
                button {
                    class: "px-3 md:px-4 py-1.5 text-sm text-accent border border-accent hover:bg-accent hover:text-text-inverse rounded-lg transition-colors font-medium flex-shrink-0",
                    onclick: move |_| state::save_current_page(),
                    "Save"
                }
                button {
                    class: "px-3 md:px-4 py-1.5 text-sm text-text-muted hover:text-text transition-colors rounded flex-shrink-0",
                    onclick: move |_| {
                        *state::EDITING.write() = false;
                    },
                    "Cancel"
                }
            }

            // Editor + Preview split — side by side on desktop, stacked on mobile
            div { class: "flex flex-col md:flex-row flex-1 overflow-hidden",
                // Editor pane
                div {
                    class: "relative flex flex-col min-h-0 flex-1 md:flex-none w-full md:w-[60%] md:min-w-[400px] border-b md:border-b-0 md:border-r border-border-light",
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
                    class: "flex flex-col flex-1 min-h-0 min-w-0 bg-panel overflow-hidden",
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

/// Behavioural coverage for the edit-buffer seeding effect (freenet/delta#62).
///
/// These mount the REAL `Editor` component in a headless `VirtualDom`, which
/// works on the native target: `dioxus`'s core is platform-independent, and
/// the only two browser-dependent helpers `Editor` reaches
/// (`page_view::behind_gateway` / `own_contract_id`) already have
/// `cfg(not(target_arch = "wasm32"))` fallbacks. So the reseed behaviour does
/// NOT need a browser to test, and these are not source-scrape pins — they
/// drive the effect and assert on the buffer contents.
///
/// Effect-flush semantics, verified empirically before these were written:
/// `rebuild_in_place()` alone does NOT run effects; they are flushed by the
/// following `render_immediate_to_vec()`. A write that changes nothing still
/// schedules a re-run, which is the bug's trigger and is what
/// `a_background_no_op_sites_write_does_not_clobber_an_in_progress_edit`
/// exercises.
#[cfg(test)]
mod reseed_tests {
    use super::*;
    use crate::state;
    use delta_core::{Page, SiteConfig, SiteState};
    use ed25519_dalek::SigningKey;

    const PREFIX: &str = "AmcVD92D3U";

    fn owner_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn site(pages: &[(PageId, &str, &str)]) -> state::KnownSite {
        let owner = owner_key();
        let mut site_state = SiteState::new(SiteConfig::default(), &owner);
        for (id, title, content) in pages {
            let page = Page::new(*id, title.to_string(), content.to_string(), 100, &owner);
            site_state
                .upsert_page(*id, page, &owner.verifying_key())
                .unwrap();
        }
        state::KnownSite {
            name: "Test site".to_string(),
            prefix: PREFIX.to_string(),
            role: state::SiteRole::Owner,
            state: site_state,
            owner_pubkey: owner.verifying_key().to_bytes(),
            contract_key: None,
        }
    }

    /// Mirrors the branch in `components::App` that decides whether the
    /// editor exists at all. Rendering `Editor` through it (rather than as
    /// the root) is what makes `EDITING` mount and unmount the component, so
    /// the "reopening re-seeds" test is not silently testing a fresh runtime.
    #[component]
    fn Harness() -> Element {
        if *state::EDITING.read() {
            rsx! { Editor {} }
        } else {
            rsx! { div { "viewing" } }
        }
    }

    /// Run pending work until it stops producing more.
    ///
    /// `rebuild_in_place()` does not itself run effects, and an effect queued
    /// while a render is in flight (e.g. the mount effect of a component that
    /// appeared during that render) only runs on the NEXT pass — so a single
    /// pass is not enough to reach a settled state. Iterating is also what
    /// keeps these tests honest in the failing direction: the bug shows up as
    /// an EXTRA seeding run, and extra passes can only make that more likely
    /// to be observed, never less.
    fn flush(dom: &mut VirtualDom) {
        for _ in 0..4 {
            let _ = dom.render_immediate_to_vec();
        }
    }

    /// Mount the harness with `page_id` of `site` selected and open the
    /// editor, mirroring `state::start_editing` / `state::create_page` (both
    /// of which set `EDITING = true`, which is what renders `Editor`).
    fn open_editor_on(site: state::KnownSite, page_id: PageId) -> VirtualDom {
        let mut dom = VirtualDom::new(Harness);
        dom.rebuild_in_place();
        dom.in_runtime(|| {
            state::SITES.write().insert(PREFIX.to_string(), site);
            *state::CURRENT_SITE.write() = Some(PREFIX.to_string());
            *state::CURRENT_PAGE.write() = Some(page_id);
            *state::EDITING.write() = true;
        });
        flush(&mut dom);
        dom
    }

    #[test]
    fn a_background_no_op_sites_write_does_not_clobber_an_in_progress_edit() {
        let mut dom = open_editor_on(
            site(&[(1, "Home", "persisted text"), (2, "Second", "second text")]),
            1,
        );

        // Opening the editor seeds the buffers from the persisted page.
        dom.in_runtime(|| {
            assert_eq!(state::EDITOR_CONTENT.cloned(), "persisted text");
            assert_eq!(state::EDITOR_TITLE.cloned(), "Home");
        });

        // The user types. Nothing is saved yet, so this text lives ONLY in
        // the editor buffers — never in SITES.
        dom.in_runtime(|| {
            *state::EDITOR_CONTENT.write() = "the user's unsaved draft".to_string();
            *state::EDITOR_TITLE.write() = "Home, retitled".to_string();
        });

        // A background handler takes the SITES write guard and changes
        // nothing. Several do exactly this (the legacy delegate sweep, the
        // freenet-migrate walk, the state-response handlers), and Dioxus
        // notifies subscribers on drop of the guard regardless, so the
        // seeding effect is re-run.
        dom.in_runtime(|| {
            let _guard = state::SITES.write();
        });
        flush(&mut dom);

        dom.in_runtime(|| {
            assert_eq!(
                state::EDITOR_CONTENT.cloned(),
                "the user's unsaved draft",
                "a no-op SITES write must not overwrite the edit buffer (#62)"
            );
            assert_eq!(state::EDITOR_TITLE.cloned(), "Home, retitled");
        });

        // Positive control, same DOM. The assertions above are all "this did
        // NOT change", which would also hold if the effect had simply stopped
        // running — and `flush`'s pass count is a constant, so a change in
        // dioxus's scheduling could silently make that true. Moving to
        // another page must still re-seed, so a dead effect fails here
        // instead of passing everything.
        dom.in_runtime(|| *state::CURRENT_PAGE.write() = Some(2));
        flush(&mut dom);
        dom.in_runtime(|| {
            assert_eq!(
                state::EDITOR_CONTENT.cloned(),
                "second text",
                "positive control: the seeding effect must still be live"
            );
        });
    }

    #[test]
    fn a_remote_update_to_the_page_being_edited_does_not_clobber_the_draft() {
        let mut dom = open_editor_on(
            site(&[(1, "Home", "persisted text"), (2, "Second", "second text")]),
            1,
        );

        dom.in_runtime(|| {
            *state::EDITOR_CONTENT.write() = "the user's unsaved draft".to_string();
        });

        // A network UPDATE for the very page being edited lands and rewrites
        // SITES. The persisted page legitimately changes; the user's unsaved
        // buffer must still not be replaced under them.
        dom.in_runtime(|| {
            let mut sites = state::SITES.write();
            let known = sites.get_mut(PREFIX).expect("site present");
            known.state.pages.get_mut(&1).expect("page present").content =
                "text that arrived from the network".to_string();
        });
        flush(&mut dom);

        dom.in_runtime(|| {
            assert_eq!(
                state::EDITOR_CONTENT.cloned(),
                "the user's unsaved draft",
                "an incoming UPDATE must not overwrite the edit buffer (#62)"
            );
        });

        // Positive control (see the sibling test): prove the effect is still
        // live, so "did not change" cannot pass by the effect being dead.
        dom.in_runtime(|| *state::CURRENT_PAGE.write() = Some(2));
        flush(&mut dom);
        dom.in_runtime(|| {
            assert_eq!(
                state::EDITOR_CONTENT.cloned(),
                "second text",
                "positive control: the seeding effect must still be live"
            );
        });
    }

    #[test]
    fn a_remote_update_reaches_the_buffers_when_the_user_has_not_typed() {
        // The other half of the rule, and a regression guard on the first
        // version of this fix, which keyed only on page identity.
        //
        // The realistic sequence: the editor opens on the delegate's backed-up
        // (older) text, and the network GET lands a moment later. An
        // identity-only guard leaves the buffers on the stale text. The user
        // then types one character and saves, and `save_current_page` ->
        // `next_page_updated_at` returns `max(now, existing + 1)` against the
        // NEWER `updated_at` now in SITES — so the save strictly dominates and
        // silently reverts the update that had already arrived. That is the
        // same lost-update bug as #62 with a different victim, so the buffers
        // must keep following the persisted page until the user actually types.
        let mut dom = open_editor_on(site(&[(1, "Home", "older backed-up text")]), 1);

        dom.in_runtime(|| {
            assert_eq!(state::EDITOR_CONTENT.cloned(), "older backed-up text");
        });

        // The network GET lands. The user has NOT typed.
        dom.in_runtime(|| {
            let mut sites = state::SITES.write();
            let known = sites.get_mut(PREFIX).expect("site present");
            known.state.pages.get_mut(&1).expect("page present").content =
                "newer text from the network".to_string();
        });
        flush(&mut dom);

        dom.in_runtime(|| {
            assert_eq!(
                state::EDITOR_CONTENT.cloned(),
                "newer text from the network",
                "with nothing typed, the buffer must track the persisted page \
                 so the save base does not go stale"
            );
        });
    }

    #[test]
    fn tracking_stops_at_the_first_keystroke_and_does_not_resume() {
        // The boundary between the two rules above. Once the user types, no
        // later update may move the buffer — not even the one that arrives
        // after several more no-op writes.
        let mut dom = open_editor_on(site(&[(1, "Home", "v1"), (2, "Second", "second text")]), 1);

        dom.in_runtime(|| *state::EDITOR_CONTENT.write() = "v1 plus the user's edit".to_string());

        for text in ["v2", "v3"] {
            dom.in_runtime(|| {
                let mut sites = state::SITES.write();
                sites
                    .get_mut(PREFIX)
                    .expect("site present")
                    .state
                    .pages
                    .get_mut(&1)
                    .expect("page present")
                    .content = text.to_string();
            });
            flush(&mut dom);
        }

        dom.in_runtime(|| {
            assert_eq!(state::EDITOR_CONTENT.cloned(), "v1 plus the user's edit");
        });

        // Positive control (see the sibling tests).
        dom.in_runtime(|| *state::CURRENT_PAGE.write() = Some(2));
        flush(&mut dom);
        dom.in_runtime(|| {
            assert_eq!(
                state::EDITOR_CONTENT.cloned(),
                "second text",
                "positive control: the seeding effect must still be live"
            );
        });
    }

    #[test]
    fn switching_to_a_different_page_reseeds_the_buffers() {
        // Pins the identity branch, so "fix it by seeding only once at mount"
        // does not pass: `state::create_page` moves CURRENT_PAGE while
        // EDITING stays true, and the buffers must then follow the new page.
        let mut dom = open_editor_on(
            site(&[(1, "Home", "home text"), (2, "Second", "second text")]),
            1,
        );

        dom.in_runtime(|| {
            *state::EDITOR_CONTENT.write() = "draft on page 1".to_string();
            *state::CURRENT_PAGE.write() = Some(2);
        });
        flush(&mut dom);

        dom.in_runtime(|| {
            assert_eq!(state::EDITOR_CONTENT.cloned(), "second text");
            assert_eq!(state::EDITOR_TITLE.cloned(), "Second");
        });
    }

    #[test]
    fn reopening_the_editor_on_the_same_page_seeds_from_the_persisted_text() {
        // The buffers are only left alone WITHIN one edit session. Closing
        // the editor unmounts the component and drops `seeded_from` with it,
        // so a later edit of the same page starts from what is actually
        // stored — otherwise a cancelled edit would silently resurrect
        // itself the next time the user pressed Edit.
        let mut dom = open_editor_on(site(&[(1, "Home", "persisted text")]), 1);

        // Type, then Cancel (`EDITING = false`, exactly what the Cancel
        // button does) — the draft is deliberately discarded.
        dom.in_runtime(|| {
            *state::EDITOR_CONTENT.write() = "abandoned draft".to_string();
            *state::EDITING.write() = false;
        });
        flush(&mut dom);

        // Press Edit again on the same page.
        dom.in_runtime(|| *state::EDITING.write() = true);
        flush(&mut dom);

        dom.in_runtime(|| {
            assert_eq!(
                state::EDITOR_CONTENT.cloned(),
                "persisted text",
                "reopening the editor must re-seed from the stored page"
            );
        });
    }
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
