use dioxus::prelude::*;

use crate::state;
use crate::state::SiteRole;
use delta_core::PageId;

#[component]
pub fn PageView() -> Element {
    let Some((page_id, page)) = state::current_page() else {
        // Check if we're waiting for a site to load vs no site selected
        let is_loading = state::CURRENT_SITE.read().is_some();
        return rsx! {
            div { class: "flex items-center justify-center h-full",
                div { class: "text-center",
                    span { class: "delta-mark w-10 h-10 text-[22px] opacity-20 mb-4 inline-flex items-center justify-center rounded-xl loading-pulse",
                        "\u{0394}"
                    }
                    p { class: "text-text-muted-light text-sm mt-4",
                        if is_loading { "Loading..." } else { "Select a page to start reading" }
                    }
                }
            }
        };
    };

    let is_owner = state::current_site()
        .map(|s| s.role == SiteRole::Owner)
        .unwrap_or(false);

    let rendered_html = render_markdown(&page.content);
    let raw_markdown = page.content.clone();
    let mut show_source = use_signal(|| false);

    let site_prefix = state::current_site()
        .map(|s| s.prefix.clone())
        .unwrap_or_default();
    let page_title_for_share = page.title.clone();
    let mut link_copied = use_signal(|| false);
    let mut confirming_delete = use_signal(|| false);
    let mut renaming = use_signal(|| false);
    let mut rename_input = use_signal(|| page.title.clone());

    rsx! {
        div { class: "max-w-4xl mx-auto px-10 py-12",
            // Page header
            div { class: "flex items-start justify-between mb-2",
                div { class: "flex-1 min-w-0" }
                div { class: "flex items-center gap-1 ml-4 flex-shrink-0",
                    // All actions as uniform quiet text buttons — content is the star
                    button {
                        class: "px-3 py-1.5 text-xs text-text-muted hover:text-accent transition-colors rounded",
                        title: "Copy link to this page",
                        onclick: move |_| {
                            copy_page_url(&site_prefix, page_id, &page_title_for_share);
                            link_copied.set(true);
                            // Reset after 2 seconds
                            #[cfg(target_arch = "wasm32")]
                            {
                                let mut signal = link_copied;
                                wasm_bindgen_futures::spawn_local(async move {
                                    gloo_timers::future::sleep(std::time::Duration::from_secs(2)).await;
                                    signal.set(false);
                                });
                            }
                        },
                        if *link_copied.read() { "Copied!" } else { "Share" }
                    }
                    button {
                        class: if *show_source.read() { "px-3 py-1.5 text-xs text-accent transition-colors rounded" } else { "px-3 py-1.5 text-xs text-text-muted hover:text-accent transition-colors rounded" },
                        onclick: move |_| {
                            let current = *show_source.read();
                            show_source.set(!current);
                        },
                        if *show_source.read() { "\u{2715} Source" } else { "Source" }
                    }
                    if is_owner {
                        button {
                            class: "px-3 py-1.5 text-xs text-text-muted hover:text-accent transition-colors rounded",
                            onclick: move |_| {
                                rename_input.set(page.title.clone());
                                renaming.set(true);
                            },
                            "Rename"
                        }
                        button {
                            class: "px-3 py-1.5 text-xs text-text-muted hover:text-accent transition-colors rounded",
                            onclick: move |_| state::start_editing(),
                            "Edit"
                        }
                        if *confirming_delete.read() {
                            button {
                                class: "px-3 py-1.5 text-xs text-red-400 hover:text-red-300 transition-colors rounded",
                                onclick: move |_| {
                                    confirming_delete.set(false);
                                    state::delete_page(page_id);
                                },
                                "Yes, delete"
                            }
                            button {
                                class: "px-3 py-1.5 text-xs text-text-muted hover:text-text transition-colors rounded",
                                onclick: move |_| confirming_delete.set(false),
                                "Cancel"
                            }
                        } else {
                            button {
                                class: "px-3 py-1.5 text-xs text-text-muted hover:text-red-400 transition-colors rounded",
                                onclick: move |_| confirming_delete.set(true),
                                "Delete"
                            }
                        }
                    }
                }
            }

            // Content - rendered or source
            if *show_source.read() {
                pre {
                    class: "editor-textarea p-5 text-sm rounded-lg bg-panel-warm border border-border-light",
                    style: "white-space: pre-wrap; word-break: break-word; overflow-wrap: break-word;",
                    "{raw_markdown}"
                }
            } else {
                div {
                    class: "prose",
                    dangerous_inner_html: "{rendered_html}",
                }
            }

            // Footer
            div { class: "mt-16 pt-4 border-t border-border-light",
                p { class: "text-[11px] text-text-muted-light tracking-wide",
                    "Page {page_id} · Updated {format_timestamp(page.updated_at)}"
                }
            }
        }

        // Rename modal - uses absolute positioning (fixed doesn't work in sandboxed iframes)
        if *renaming.read() {
            div {
                style: "position: absolute; inset: 0; background: rgba(0,0,0,0.5); display: flex; align-items: center; justify-content: center; z-index: 50;",
                onclick: move |_| renaming.set(false),
                div {
                    class: "bg-panel rounded-xl shadow-lg w-80 p-5",
                    onclick: move |evt| evt.stop_propagation(),
                    h3 { class: "text-sm font-semibold text-text mb-3", "Rename Page" }
                    input {
                        class: "w-full px-3 py-2 bg-panel-warm border border-border-light rounded-lg text-text text-sm outline-none focus:border-accent",
                        r#type: "text",
                        value: "{rename_input}",
                        oninput: move |evt| rename_input.set(evt.value().to_string()),
                        onkeypress: move |evt| {
                            if evt.key() == Key::Enter {
                                let new_name = rename_input.read().clone();
                                if !new_name.trim().is_empty() {
                                    state::rename_page(page_id, new_name);
                                }
                                renaming.set(false);
                            } else if evt.key() == Key::Escape {
                                renaming.set(false);
                            }
                        },
                    }
                    div { class: "flex gap-3 mt-4 justify-end",
                        button {
                            class: "px-4 py-2 text-sm text-accent border border-accent hover:bg-accent hover:text-text-inverse rounded-lg transition-colors font-medium",
                            onclick: move |_| {
                                let new_name = rename_input.read().clone();
                                if !new_name.trim().is_empty() {
                                    state::rename_page(page_id, new_name);
                                }
                                renaming.set(false);
                            },
                            "OK"
                        }
                        button {
                            class: "px-4 py-2 text-sm text-text-muted hover:text-text transition-colors rounded",
                            onclick: move |_| renaming.set(false),
                            "Cancel"
                        }
                    }
                }
            }
        }
    }
}

/// Render markdown to HTML, resolving `[[id|text]]` page links as hash links
/// and injecting `id="..."` attributes on headings so in-page anchor links
/// (`[Link to Heading](#heading)`) work natively.
fn render_markdown(content: &str) -> String {
    let resolved = resolve_page_links(content);
    let html = markdown::to_html_with_options(&resolved, &markdown::Options::gfm())
        .unwrap_or_else(|_| markdown::to_html(&resolved));
    inject_heading_ids(&html)
}

/// Walk the rendered HTML and inject GFM-style `id="slug"` attributes on
/// every `<h1>`..`<h6>` opening tag. The `markdown` crate does not emit
/// these by default, so anchor links like `[Link](#test-heading)` have
/// nowhere to land. The id is the slugified plain-text of the heading
/// (HTML tags stripped); duplicate slugs are disambiguated with `-1`,
/// `-2`, ... matching the convention GitHub uses.
pub(super) fn inject_heading_ids(html: &str) -> String {
    let mut result = String::with_capacity(html.len() + 64);
    let mut id_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let bytes = html.as_bytes();
    // Byte position up to which non-heading content has yet to be flushed
    // into `result`. We accumulate ranges and slice via `html[..]` so
    // multibyte UTF-8 characters survive the walk intact (byte-by-byte
    // pushing-as-char would corrupt them). The byte-level pattern checks
    // below all use ASCII bytes, which by UTF-8 invariant never collide
    // with continuation bytes inside multibyte characters.
    let mut copy_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        // Look for `<hN>` where N is 1..=6 and the next char is `>`.
        if i + 4 <= bytes.len()
            && bytes[i] == b'<'
            && bytes[i + 1] == b'h'
            && (b'1'..=b'6').contains(&bytes[i + 2])
            && bytes[i + 3] == b'>'
        {
            let level = bytes[i + 2];
            let close_tag = format!("</h{}>", level as char);
            let inner_start = i + 4;
            if let Some(rel) = html[inner_start..].find(&close_tag) {
                let inner = &html[inner_start..inner_start + rel];
                let plain = strip_html_tags(inner);
                let base_slug = slugify_heading(&plain);
                if base_slug.is_empty() {
                    // Heading with no sluggable text — leave the tag
                    // alone rather than emit `id=""`.
                    i = inner_start;
                    continue;
                }
                // Flush everything up to the start of this heading
                // (`i` points at the `<`, which is ASCII, so the slice
                // ends on a char boundary).
                result.push_str(&html[copy_start..i]);
                let count = id_counts.entry(base_slug.clone()).or_insert(0);
                let slug = if *count == 0 {
                    base_slug.clone()
                } else {
                    format!("{}-{}", base_slug, *count)
                };
                *count += 1;
                result.push_str(&format!("<h{} id=\"{}\">", level as char, slug));
                result.push_str(inner);
                result.push_str(&close_tag);
                let after = inner_start + rel + close_tag.len();
                i = after;
                copy_start = after;
                continue;
            }
        }
        i += 1;
    }
    result.push_str(&html[copy_start..]);
    result
}

/// Strip HTML tags from a string, returning just the textual content
/// with the common HTML entities decoded back to their characters.
/// Used for heading-id slugification so a heading containing a link or
/// `<em>` produces a slug from the visible text only, and headings
/// like `# Foo & Bar` (rendered as `Foo &amp; Bar` by the markdown
/// crate) slugify the same way GitHub does.
fn strip_html_tags(s: &str) -> String {
    let stripped = {
        let mut out = String::with_capacity(s.len());
        let mut in_tag = false;
        for c in s.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out
    };
    decode_html_entities(&stripped)
}

/// Minimal HTML entity decoder covering the entities the `markdown`
/// crate emits: `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`, plus
/// numeric character references `&#N;` and `&#xN;`. Anything unknown
/// is passed through verbatim.
fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after.find(';') {
            let entity = &after[..=semi];
            let decoded: Option<char> = match entity {
                "&amp;" => Some('&'),
                "&lt;" => Some('<'),
                "&gt;" => Some('>'),
                "&quot;" => Some('"'),
                "&apos;" => Some('\''),
                "&#39;" => Some('\''),
                _ => {
                    // Numeric character references: &#N; (decimal) or &#xN; (hex)
                    let body = &entity[1..entity.len() - 1];
                    if let Some(rest_body) = body.strip_prefix('#') {
                        let cp = if let Some(hex) = rest_body
                            .strip_prefix('x')
                            .or_else(|| rest_body.strip_prefix('X'))
                        {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            rest_body.parse::<u32>().ok()
                        };
                        cp.and_then(char::from_u32)
                    } else {
                        None
                    }
                }
            };
            if let Some(c) = decoded {
                out.push(c);
                rest = &after[semi + 1..];
                continue;
            }
            // Unknown entity — pass the `&` through verbatim and keep
            // scanning from the next byte so we don't infinite-loop.
            out.push('&');
            rest = &after[1..];
        } else {
            // No closing `;` — pass the rest through.
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Slugify heading text into an HTML `id`. Approximates the GitHub
/// algorithm: lowercase, drop punctuation, collapse whitespace runs to
/// a single hyphen, keep alphanumerics, hyphens and underscores.
fn slugify_heading(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_hyphen = false;
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
            last_was_hyphen = false;
        } else if (c.is_whitespace() || c == '-') && !last_was_hyphen && !out.is_empty() {
            out.push('-');
            last_was_hyphen = true;
        }
        // else: drop punctuation
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Replace page links with hash-routed markdown links.
///
/// Supported syntax:
///   [[id|Display Text]]  - link by page ID (canonical, stored format)
///   [[Page Title]]       - link by page title (user-friendly)
///   [[Page Title|Label]] - link by title with custom display text
fn resolve_page_links(content: &str) -> String {
    let prefix = state::CURRENT_SITE.read().clone().unwrap_or_default();
    let sites = state::SITES.read();
    let pages = sites.get(&prefix).map(|s| &s.state.pages);

    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find("[[") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];

        if let Some(end) = after_open.find("]]") {
            let link_content = &after_open[..end];
            let resolved = if let Some((first, display)) = link_content.split_once('|') {
                // [[first|display]] - first could be ID or title
                // The display text is always used as the rendered link text
                if let Ok(id) = first.trim().parse::<PageId>() {
                    // [[id|Display Text]] - canonical format
                    let slug = pages
                        .and_then(|p| p.get(&id))
                        .map(|p| p.title.clone())
                        .unwrap_or_else(|| display.to_string());
                    let hash = state::build_hash_route(&prefix, Some(id), Some(&slug));
                    Some(format!("[{display}]({hash})"))
                } else {
                    // [[Title|Label]] - look up by title, show label
                    find_page_by_title(pages, first.trim()).map(|(id, _)| {
                        let hash = state::build_hash_route(&prefix, Some(id), Some(first.trim()));
                        format!("[{display}]({hash})")
                    })
                }
            } else {
                // [[id]] or [[Page Title]] - no custom display text
                let trimmed = link_content.trim();
                if let Ok(id) = trimmed.parse::<PageId>() {
                    // [[id]] - render as current page title (auto-updates on rename)
                    pages.and_then(|p| p.get(&id)).map(|p| {
                        let hash = state::build_hash_route(&prefix, Some(id), Some(&p.title));
                        format!("[{}]({hash})", p.title)
                    })
                } else {
                    // [[Page Title]] - look up by title
                    find_page_by_title(pages, trimmed).map(|(id, title)| {
                        let hash = state::build_hash_route(&prefix, Some(id), Some(&title));
                        format!("[{title}]({hash})")
                    })
                }
            };

            result.push_str(&resolved.unwrap_or_else(|| {
                // Broken link - render as styled warning text
                format!("<span style=\"color: var(--color-text-muted); text-decoration: line-through;\" title=\"Page not found\">[[{link_content}]]</span>")
            }));
            rest = &after_open[end + 2..];
        } else {
            result.push_str("[[");
            rest = after_open;
        }
    }
    result.push_str(rest);
    result
}

/// Find a page by title (case-insensitive).
fn find_page_by_title(
    pages: Option<&std::collections::BTreeMap<PageId, delta_core::Page>>,
    title: &str,
) -> Option<(PageId, String)> {
    let pages = pages?;
    let lower = title.to_lowercase();
    pages
        .iter()
        .find(|(_, p)| p.title.to_lowercase() == lower)
        .map(|(&id, p)| (id, p.title.clone()))
}

/// Copy the full URL for a specific page to clipboard.
/// Uses execCommand('copy') fallback since the Clipboard API is blocked
/// in sandboxed iframes without clipboard-write permission.
fn copy_page_url(prefix: &str, page_id: PageId, title: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        if let Some(window) = web_sys::window() {
            let location = window.location();
            let origin = location.origin().unwrap_or_default();
            let pathname = location.pathname().unwrap_or_default();
            let hash = state::build_hash_route(prefix, Some(page_id), Some(title));
            let url = format!("{origin}{pathname}{hash}");

            // Use textarea + execCommand fallback (works in sandboxed iframes)
            if let Some(doc) = window.document() {
                if let Ok(el) = doc.create_element("textarea") {
                    if let Some(textarea) = el.dyn_ref::<web_sys::HtmlTextAreaElement>() {
                        textarea.set_value(&url);
                        if let Some(style) = textarea
                            .dyn_ref::<web_sys::HtmlElement>()
                            .map(|e| e.style())
                        {
                            let _ = style.set_property("position", "fixed");
                            let _ = style.set_property("opacity", "0");
                        }
                        if let Some(body) = doc.body() {
                            let _ = body.append_child(textarea);
                            textarea.select();
                            if let Some(html_doc) = doc.dyn_ref::<web_sys::HtmlDocument>() {
                                let _ = html_doc.exec_command("copy");
                            }
                            let _ = body.remove_child(textarea);
                        }
                    }
                }
            }

            // Send hash to parent shell for URL bar update (#3747)
            let msg = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &msg,
                &wasm_bindgen::JsValue::from_str("__freenet_shell__"),
                &wasm_bindgen::JsValue::TRUE,
            );
            let _ = js_sys::Reflect::set(
                &msg,
                &wasm_bindgen::JsValue::from_str("type"),
                &wasm_bindgen::JsValue::from_str("hash"),
            );
            let _ = js_sys::Reflect::set(
                &msg,
                &wasm_bindgen::JsValue::from_str("hash"),
                &wasm_bindgen::JsValue::from_str(&hash),
            );
            let target = window.parent().ok().flatten().unwrap_or(window);
            let _ = target.post_message(&msg, "*");
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (prefix, page_id, title);
    }
}

fn format_timestamp(ts: u64) -> String {
    use chrono::{DateTime, Utc};
    let dt = DateTime::<Utc>::from_timestamp(ts as i64, 0);
    dt.map(|d| d.format("%b %d, %Y").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::{inject_heading_ids, slugify_heading, strip_html_tags};

    #[test]
    fn slugify_simple_heading() {
        assert_eq!(slugify_heading("Hello World"), "hello-world");
    }

    #[test]
    fn slugify_drops_punctuation() {
        // GitHub-style: punctuation is stripped, not replaced with hyphens.
        assert_eq!(slugify_heading("Foo's Bar!"), "foos-bar");
        assert_eq!(slugify_heading("What? Why!"), "what-why");
    }

    #[test]
    fn slugify_collapses_runs_of_whitespace() {
        // Multiple spaces / tabs become a single hyphen.
        assert_eq!(slugify_heading("Foo   Bar"), "foo-bar");
        assert_eq!(slugify_heading("Foo \t Bar"), "foo-bar");
    }

    #[test]
    fn slugify_keeps_underscores() {
        // Underscores are valid in HTML ids and meaningful in code-style headings.
        assert_eq!(slugify_heading("snake_case_heading"), "snake_case_heading");
    }

    #[test]
    fn slugify_trims_leading_and_trailing_separators() {
        assert_eq!(slugify_heading("  Hello  "), "hello");
        assert_eq!(slugify_heading("--Hello--"), "hello");
    }

    #[test]
    fn slugify_empty_for_punctuation_only_heading() {
        assert_eq!(slugify_heading("???"), "");
        assert_eq!(slugify_heading(""), "");
    }

    #[test]
    fn slugify_lowercases_unicode() {
        assert_eq!(slugify_heading("Café"), "café");
    }

    #[test]
    fn strip_tags_keeps_text_only() {
        assert_eq!(
            strip_html_tags("Hello <em>World</em>"),
            "Hello World".to_string()
        );
        assert_eq!(
            strip_html_tags("<a href=\"x\">Link</a> Text"),
            "Link Text".to_string()
        );
    }

    #[test]
    fn inject_id_into_simple_h1() {
        let html = "<h1>Hello World</h1>";
        assert_eq!(
            inject_heading_ids(html),
            "<h1 id=\"hello-world\">Hello World</h1>"
        );
    }

    #[test]
    fn inject_id_for_all_levels_h1_through_h6() {
        for level in 1..=6 {
            let html = format!("<h{level}>Test</h{level}>");
            let expected = format!("<h{level} id=\"test\">Test</h{level}>");
            assert_eq!(inject_heading_ids(&html), expected);
        }
    }

    #[test]
    fn inject_id_uses_text_only_for_heading_with_inline_html() {
        // Heading with a `<em>` produces an id from the visible text,
        // not from the raw HTML.
        let html = "<h2>The <em>Way</em> Forward</h2>";
        assert_eq!(
            inject_heading_ids(html),
            "<h2 id=\"the-way-forward\">The <em>Way</em> Forward</h2>"
        );
    }

    #[test]
    fn duplicate_headings_get_disambiguating_suffixes() {
        // GitHub convention: the second `<h2>Notes</h2>` becomes id="notes-1".
        let html = "<h2>Notes</h2><h2>Notes</h2><h2>Notes</h2>";
        assert_eq!(
            inject_heading_ids(html),
            "<h2 id=\"notes\">Notes</h2><h2 id=\"notes-1\">Notes</h2><h2 id=\"notes-2\">Notes</h2>"
        );
    }

    #[test]
    fn empty_heading_is_left_untouched() {
        // `<h1></h1>` slugifies to "", and we'd rather leave the tag
        // alone than emit `id=""`.
        let html = "<h1></h1>";
        assert_eq!(inject_heading_ids(html), "<h1></h1>");
    }

    #[test]
    fn punctuation_only_heading_is_left_untouched() {
        let html = "<h1>!!!</h1>";
        assert_eq!(inject_heading_ids(html), "<h1>!!!</h1>");
    }

    #[test]
    fn non_heading_tags_are_left_alone() {
        let html = "<p>Not a heading</p><div><h1>Header</h1></div>";
        assert_eq!(
            inject_heading_ids(html),
            "<p>Not a heading</p><div><h1 id=\"header\">Header</h1></div>"
        );
    }

    #[test]
    fn h7_or_higher_is_not_a_real_heading() {
        // Defensive: there is no `<h7>` in HTML; don't try to inject.
        let html = "<h7>Fake</h7>";
        assert_eq!(inject_heading_ids(html), "<h7>Fake</h7>");
    }

    #[test]
    fn anchor_link_target_matches_heading_id() {
        // The reproduction Ivvor reported: `[Link to Heading](#heading)`
        // generates `<a href="#heading">` and the matching heading must
        // get `id="heading"` so the browser can scroll to it.
        let html = "<h2>Heading</h2><p><a href=\"#heading\">jump</a></p>";
        let with_ids = inject_heading_ids(html);
        assert!(with_ids.contains("<h2 id=\"heading\">"));
        assert!(with_ids.contains("href=\"#heading\""));
    }

    #[test]
    fn multibyte_utf8_outside_headings_is_preserved() {
        // Regression test: an earlier draft pushed bytes one at a time
        // via `push(bytes[i] as char)`, which corrupted multibyte UTF-8
        // sequences in non-heading content (e.g. `é` -> `Ã©`).
        let html = "<p>Café was open</p><h1>Title</h1><p>Naïve</p>";
        let result = inject_heading_ids(html);
        assert!(
            result.contains("Café"),
            "Café was corrupted in non-heading content: {result}"
        );
        assert!(result.contains("Naïve"), "Naïve was corrupted: {result}");
        assert!(result.contains("<h1 id=\"title\">"));
    }

    #[test]
    fn multibyte_utf8_inside_headings_survives_and_slugifies() {
        let html = "<h1>Café Résumé</h1>";
        let result = inject_heading_ids(html);
        assert_eq!(result, "<h1 id=\"café-résumé\">Café Résumé</h1>");
    }

    #[test]
    fn strip_tags_decodes_common_html_entities() {
        // The markdown crate emits `&amp;` for `&`, `&lt;` for `<`, etc.
        // The slugifier must see the *decoded* text so a heading
        // `# Foo & Bar` produces id="foo-bar", matching GitHub.
        assert_eq!(strip_html_tags("Foo &amp; Bar"), "Foo & Bar");
        assert_eq!(strip_html_tags("&lt;tag&gt;"), "<tag>");
        assert_eq!(strip_html_tags("It&#39;s"), "It's");
        assert_eq!(strip_html_tags("&quot;quoted&quot;"), "\"quoted\"");
    }

    #[test]
    fn slug_for_heading_with_ampersand_matches_github() {
        let html = "<h2>Foo &amp; Bar</h2>";
        let result = inject_heading_ids(html);
        assert_eq!(result, "<h2 id=\"foo-bar\">Foo &amp; Bar</h2>");
    }

    #[test]
    fn unknown_entity_passes_through() {
        // `&unknownEntity;` isn't decoded; the `&` is preserved verbatim
        // so we don't lose data and the slugifier strips it as
        // punctuation.
        assert_eq!(strip_html_tags("foo &unknown; bar"), "foo &unknown; bar");
    }

    #[test]
    fn stray_ampersand_without_semicolon_passes_through() {
        // No closing `;` -> not an entity; pass the rest of the string
        // through without trying to parse.
        assert_eq!(strip_html_tags("foo & bar"), "foo & bar");
    }
}
