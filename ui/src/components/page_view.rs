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

/// Render markdown to HTML, resolving `[[id|text]]` page links as hash links,
/// injecting `id="..."` attributes on headings so in-page anchor links
/// (`[Link to Heading](#heading)`) work natively, and beautifying bare
/// Freenet web-contract URLs (`http://gateway/v1/contract/web/<id>/...`)
/// into `freenet:<id-prefix>[/path]` labels with same-origin hrefs.
fn render_markdown(content: &str) -> String {
    let resolved = resolve_page_links(content);
    let html = markdown::to_html_with_options(&resolved, &markdown::Options::gfm())
        .unwrap_or_else(|_| markdown::to_html(&resolved));
    let html = inject_heading_ids(&html);
    finalize_anchors(&html, behind_gateway(), own_contract_id().as_deref())
}

/// True when Delta is currently being served from a path under
/// `/v1/contract/web/`, which is what gateway hosting looks like to the
/// browser. Returning false suppresses the host-stripping href rewrite
/// for `dx serve` and other dev flows where the rewritten same-origin
/// path would have no gateway behind it.
///
/// Exposed at module level so the editor live-preview honors the same
/// flag — without that the preview shows a same-origin path for
/// Freenet URLs in dev mode while the rendered page view doesn't,
/// which surprises authors during `dx serve` iteration.
#[cfg(target_arch = "wasm32")]
pub(super) fn behind_gateway() -> bool {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .map(|p| p.starts_with("/v1/contract/web/"))
        .unwrap_or(false)
}

/// Native test builds: default to true so existing tests verify the
/// production (gateway-hosted) behavior. Tests covering the dev-mode
/// path call `finalize_anchors` with an explicit `false` flag.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn behind_gateway() -> bool {
    true
}

/// The contract ID Delta is currently being served under, extracted
/// from `window.location.pathname`. Returns `None` outside of the
/// gateway-hosted path layout (e.g. `dx serve`, native tests).
///
/// Used by `finalize_anchors` so a Freenet URL pointing at *our own*
/// contract — e.g. one the user copied from the Share button — is
/// treated as an internal link rather than an external one. Without
/// that, clicking it would open in a new tab instead of doing the
/// in-iframe hashchange navigation the URL was intended to trigger.
#[cfg(target_arch = "wasm32")]
pub(super) fn own_contract_id() -> Option<String> {
    let pathname = web_sys::window().and_then(|w| w.location().pathname().ok())?;
    let after_marker = pathname.strip_prefix("/v1/contract/web/")?;
    let id_end = after_marker
        .find(|c: char| !is_freenet_base58_char(c))
        .unwrap_or(after_marker.len());
    if matches!(id_end, 43 | 44) {
        Some(after_marker[..id_end].to_string())
    } else {
        None
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn own_contract_id() -> Option<String> {
    None
}

/// Walk anchor tags in the rendered HTML and:
///
/// - Add `target="_blank" rel="noopener noreferrer"` to every truly
///   external anchor. An anchor is considered internal — and
///   therefore left to navigate in-iframe — if any of:
///     1. The href starts with `#` (hash-only: heading anchors,
///        Delta's `[[id|title]]` page links).
///     2. The href is a Freenet web-contract URL whose contract ID
///        equals our own iframe's contract ID, e.g. a URL the user
///        copied from the Share button. Without this, clicking such
///        a link would open a new tab instead of triggering the
///        in-iframe hashchange navigation that the URL was designed
///        to drive. (Ivvor, 2026-05-03 11:55)
/// - When `rewrite_freenet_hrefs` is true, rewrite Freenet web-contract
///   URLs to a same-origin absolute path so the link works for any
///   reader regardless of which gateway they're connected to.
/// - For bare Freenet web URLs (where the visible text equals the
///   original href), shorten the label to `freenet:<id-prefix>[/<path>]`
///   regardless of hosting. User-supplied `[label](url)` text is kept.
///
/// Ported from River (`river/main/ui/src/components/conversation.rs`),
/// adapted for Delta's in-iframe page link / heading anchor model.
pub(super) fn finalize_anchors(
    html: &str,
    rewrite_freenet_hrefs: bool,
    own_contract_id: Option<&str>,
) -> String {
    let mut out = String::with_capacity(html.len() + 32);
    let mut rest = html;
    while let Some(pos) = rest.find("<a ") {
        out.push_str(&rest[..pos]);
        let tag = &rest[pos..];
        let Some(open_end) = tag.find('>') else {
            out.push_str(tag);
            return out;
        };
        let opening = &tag[..=open_end];
        let after_open = &tag[open_end + 1..];
        let Some(close_pos) = after_open.find("</a>") else {
            out.push_str(tag);
            return out;
        };
        let inner = &after_open[..close_pos];
        let tail = &after_open[close_pos + 4..];

        let original_href = extract_href(opening);
        let parsed_freenet = original_href.as_deref().and_then(parse_freenet_web_url);
        let starts_with_hash = original_href
            .as_deref()
            .map(|h| h.starts_with('#'))
            .unwrap_or(true);
        let is_same_contract = match (own_contract_id, &parsed_freenet) {
            (Some(own), Some(parsed)) => own == parsed.contract_id,
            _ => false,
        };
        let is_internal = starts_with_hash || is_same_contract;

        // Inject `target="_blank" rel="noopener noreferrer"` on
        // truly external links only. Internal links stay in-iframe.
        let opening = if is_internal {
            opening.to_string()
        } else {
            opening.replacen(
                "<a href=\"",
                "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"",
                1,
            )
        };

        // Rewrite Freenet hrefs to same-origin paths regardless of
        // whether the link is internal or external — both benefit
        // from being host/port-agnostic. (Internal same-contract
        // links also need this so the hashchange they trigger lands
        // on the current iframe, not a freshly-loaded one at the
        // sender's hard-coded origin.)
        let opening = if rewrite_freenet_hrefs {
            match original_href.as_deref().and_then(rewrite_freenet_href) {
                Some(new_href) => {
                    let orig = original_href.as_deref().unwrap();
                    opening.replacen(
                        &format!("href=\"{orig}\""),
                        &format!("href=\"{new_href}\""),
                        1,
                    )
                }
                None => opening,
            }
        } else {
            opening
        };

        let new_inner = match original_href.as_deref() {
            Some(h) if !starts_with_hash && h == inner => {
                beautify_freenet_label(h).unwrap_or_else(|| inner.to_string())
            }
            _ => inner.to_string(),
        };

        out.push_str(&opening);
        out.push_str(&new_inner);
        out.push_str("</a>");
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn extract_href(opening_tag: &str) -> Option<String> {
    let start = opening_tag.find("href=\"")? + "href=\"".len();
    let end = opening_tag[start..].find('"')?;
    Some(opening_tag[start..start + end].to_string())
}

/// Parsed shape of a Freenet web-contract URL.
struct FreenetWebUrl<'a> {
    /// The contract ID — base58-encoded 32-byte BLAKE3 hash (43 or 44 chars).
    contract_id: &'a str,
    /// Anything after the contract ID: leading slash + path, query, fragment.
    /// `""` for `/v1/contract/web/<id>` with nothing after.
    suffix: &'a str,
    /// Same-origin absolute path including the marker:
    /// `/v1/contract/web/<id><suffix>`. Used as a host/port-agnostic href.
    absolute_path: &'a str,
}

/// Parse a Freenet web-contract URL, validating the contract ID looks
/// like a real base58-encoded 32-byte BLAKE3 hash. Rejects same-prefix
/// paths whose ID segment is too short or contains visual-confusion
/// chars (`0`, `O`, `I`, `l`) that a real contract ID can never have.
///
/// The URL must use `http` or `https` (defense in depth — `[label](url)`
/// markdown can in theory carry other schemes; we don't want to rewrite
/// `javascript:`-flavored input even though the rewrite would defang it).
///
/// The suffix must not contain `..` path segments. Without this guard a
/// pasted `http://attacker/v1/contract/web/<valid-shape-id>/../../foo`
/// would be rewritten to a same-origin path that the browser normalizes
/// into `/foo` on the reader's local gateway, redirecting the click to
/// the victim's gateway instead of the attacker's host.
fn parse_freenet_web_url(url: &str) -> Option<FreenetWebUrl<'_>> {
    // Either an absolute URL (http://host/v1/contract/web/...) or a
    // path-relative URL (/v1/contract/web/...) — both are valid in
    // user-authored markdown. Ivvor 2026-05-03 12:10 reported using
    // the relative form; without this the relative href slipped past
    // same-contract detection and got `target="_blank"` despite
    // pointing at our own contract.
    let path = if let Some(scheme_end) = url.find("://") {
        let scheme = &url[..scheme_end];
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return None;
        }
        let after_scheme = &url[scheme_end + 3..];
        let path_offset = after_scheme.find('/')?;
        &after_scheme[path_offset..]
    } else if url.starts_with('/') {
        url
    } else {
        return None;
    };
    let after_marker = path.strip_prefix("/v1/contract/web/")?;

    let id_end = after_marker
        .find(|c: char| !is_freenet_base58_char(c))
        .unwrap_or(after_marker.len());
    if !matches!(id_end, 43 | 44) {
        return None;
    }
    let suffix = &after_marker[id_end..];
    if suffix_has_dotdot_segment(suffix) {
        return None;
    }
    Some(FreenetWebUrl {
        contract_id: &after_marker[..id_end],
        suffix,
        absolute_path: path,
    })
}

/// True if any path segment in `suffix` is exactly `..`. Path segments
/// are the `/`-separated components before any `?` query or `#` fragment.
fn suffix_has_dotdot_segment(suffix: &str) -> bool {
    let path_only = suffix
        .split_once(['?', '#'])
        .map(|(p, _)| p)
        .unwrap_or(suffix);
    path_only.split('/').any(|seg| seg == "..")
}

/// Bitcoin-style base58 alphabet: digits and letters minus the visually
/// ambiguous `0`, `O`, `I`, `l`. Identical to the alphabet used in
/// `state::is_base58_char` but kept local because the contract-ID parser
/// only cares about chars-in-this-set, not "exactly 10 of them".
fn is_freenet_base58_char(c: char) -> bool {
    matches!(
        c,
        '1'..='9' | 'A'..='H' | 'J'..='N' | 'P'..='Z' | 'a'..='k' | 'm'..='z'
    )
}

/// Rewrite a Freenet web-contract URL's href to a same-origin absolute
/// path, stripping the scheme + host + port. Returns None for non-Freenet
/// URLs so the caller leaves the original href untouched.
///
/// `http://127.0.0.1:7509/v1/contract/web/<id>/foo` -> `/v1/contract/web/<id>/foo`
/// `https://gw.example/v1/contract/web/<id>/#hash`  -> `/v1/contract/web/<id>/#hash`
///
/// The browser resolves the absolute path against the current page's
/// origin, so the rewritten link points at whichever gateway Delta is
/// loaded from — fixing pasted links that hard-code the sender's local
/// gateway address.
fn rewrite_freenet_href(url: &str) -> Option<String> {
    Some(parse_freenet_web_url(url)?.absolute_path.to_string())
}

/// If `url` is a Freenet web-contract URL, return a beautified label
/// like `freenet:UDzGbcWr` or `freenet:UDzGbcWr/index.html`. Returns
/// None for any other URL so the caller falls back to the original
/// link text.
fn beautify_freenet_label(url: &str) -> Option<String> {
    let parsed = parse_freenet_web_url(url)?;
    // Defense in depth: refuse to beautify if the suffix carries raw
    // HTML metacharacters. The markdown crate URL-encodes these today,
    // but the label is rendered via dangerous_inner_html with no
    // further escaping, so we'd rather skip the rewrite than risk
    // smuggling markup.
    if parsed.suffix.contains(['<', '>', '"']) {
        return None;
    }
    // A bare trailing slash adds no information; drop it.
    let suffix = if parsed.suffix == "/" {
        ""
    } else {
        parsed.suffix
    };
    let id_prefix = &parsed.contract_id[..8];
    Some(format!("freenet:{id_prefix}{suffix}"))
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
    use super::{
        beautify_freenet_label, finalize_anchors, inject_heading_ids, parse_freenet_web_url,
        rewrite_freenet_href, slugify_heading, strip_html_tags, suffix_has_dotdot_segment,
    };

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

    // -- Freenet URL formatter (ported from River) --

    // 43-char Bitcoin-base58 contract IDs. Real River contract ID, used
    // verbatim in the river test suite.
    const RIVER_ID: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
    // Real Delta contract ID — 43 chars too.
    const DELTA_ID: &str = "EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr";

    #[test]
    fn parse_freenet_web_url_accepts_real_id() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{DELTA_ID}/");
        let parsed = parse_freenet_web_url(&url).expect("should parse");
        assert_eq!(parsed.contract_id, DELTA_ID);
        assert_eq!(parsed.suffix, "/");
        assert_eq!(
            parsed.absolute_path,
            format!("/v1/contract/web/{DELTA_ID}/")
        );
    }

    #[test]
    fn parse_freenet_web_url_accepts_https_and_subpath() {
        let url = format!("https://gw.example.com/v1/contract/web/{RIVER_ID}/index.html");
        let parsed = parse_freenet_web_url(&url).expect("should parse");
        assert_eq!(parsed.contract_id, RIVER_ID);
        assert_eq!(parsed.suffix, "/index.html");
    }

    #[test]
    fn parse_freenet_web_url_accepts_query_and_fragment() {
        let url = format!("http://gw/v1/contract/web/{RIVER_ID}/?invite=abc#hash");
        let parsed = parse_freenet_web_url(&url).expect("should parse");
        assert_eq!(parsed.suffix, "/?invite=abc#hash");
    }

    #[test]
    fn parse_freenet_web_url_rejects_non_http_scheme() {
        let url = format!("javascript:/v1/contract/web/{RIVER_ID}/");
        assert!(parse_freenet_web_url(&url).is_none());
        let url = format!("ftp://gw/v1/contract/web/{RIVER_ID}/");
        assert!(parse_freenet_web_url(&url).is_none());
    }

    #[test]
    fn parse_freenet_web_url_rejects_short_or_long_id() {
        // 9-char prefix is too short.
        assert!(parse_freenet_web_url("http://gw/v1/contract/web/short/").is_none());
        // 50-char prefix is too long.
        assert!(parse_freenet_web_url(
            "http://gw/v1/contract/web/12345678901234567890123456789012345678901234567890/"
        )
        .is_none());
    }

    #[test]
    fn parse_freenet_web_url_rejects_visual_confusion_chars_in_id() {
        // `0`, `O`, `I`, `l` aren't in the bitcoin base58 alphabet —
        // a real contract ID can never contain them.
        let url = format!("http://gw/v1/contract/web/0{}/", &RIVER_ID[1..]);
        assert!(parse_freenet_web_url(&url).is_none());
    }

    #[test]
    fn parse_freenet_web_url_rejects_dotdot_in_path() {
        // Path-traversal protection: even with a valid-shape ID, refuse
        // to rewrite if the suffix has `..` segments — the rewritten
        // same-origin path would be normalized by the browser into a
        // path the attacker chose on the *reader's* gateway.
        let url = format!("http://attacker/v1/contract/web/{RIVER_ID}/../../foo");
        assert!(parse_freenet_web_url(&url).is_none());
    }

    #[test]
    fn suffix_dotdot_helper() {
        assert!(suffix_has_dotdot_segment("/foo/../bar"));
        assert!(suffix_has_dotdot_segment("/.."));
        // `..` only as a path segment, not as part of a name.
        assert!(!suffix_has_dotdot_segment("/foo..bar"));
        // `..` after `?` is in the query string, not a path segment.
        assert!(!suffix_has_dotdot_segment("/foo?x=..&y=..&"));
    }

    #[test]
    fn rewrite_freenet_href_strips_host_and_port() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{DELTA_ID}/foo");
        assert_eq!(
            rewrite_freenet_href(&url).as_deref(),
            Some(format!("/v1/contract/web/{DELTA_ID}/foo").as_str())
        );
    }

    #[test]
    fn beautify_freenet_label_shortens_to_id_prefix() {
        let url = format!("http://gw/v1/contract/web/{RIVER_ID}");
        assert_eq!(
            beautify_freenet_label(&url).as_deref(),
            Some("freenet:raAqMhMG")
        );
    }

    #[test]
    fn beautify_freenet_label_drops_bare_trailing_slash() {
        let url = format!("http://gw/v1/contract/web/{RIVER_ID}/");
        assert_eq!(
            beautify_freenet_label(&url).as_deref(),
            Some("freenet:raAqMhMG")
        );
    }

    #[test]
    fn beautify_freenet_label_keeps_subpath() {
        let url = format!("http://gw/v1/contract/web/{RIVER_ID}/index.html");
        assert_eq!(
            beautify_freenet_label(&url).as_deref(),
            Some("freenet:raAqMhMG/index.html")
        );
    }

    #[test]
    fn beautify_freenet_label_refuses_html_metachars_in_suffix() {
        // Defense in depth: the label is rendered via dangerous_inner_html
        // with no further escaping, so refuse smuggled markup.
        let url = format!("http://gw/v1/contract/web/{RIVER_ID}/path?x=<script>alert(1)</script>");
        assert_eq!(beautify_freenet_label(&url), None);
    }

    #[test]
    fn finalize_anchors_adds_target_blank_to_external_links() {
        let html = "<a href=\"https://example.com\">Example</a>";
        let result = finalize_anchors(html, true, None);
        assert!(result.contains("target=\"_blank\""));
        assert!(result.contains("rel=\"noopener noreferrer\""));
    }

    #[test]
    fn finalize_anchors_leaves_internal_hash_links_alone() {
        // [[id|title]] page links and heading anchors all start with `#`
        // and must NOT get target="_blank" — that would open in a new
        // tab on every page click.
        let html = "<a href=\"#AmcVD92D3U/2/page-2\">Page 2</a>";
        let result = finalize_anchors(html, true, None);
        assert!(!result.contains("target=\"_blank\""));
        assert!(result.contains("href=\"#AmcVD92D3U/2/page-2\""));
    }

    #[test]
    fn finalize_anchors_rewrites_freenet_href_under_gateway() {
        let html =
            format!("<a href=\"http://127.0.0.1:7509/v1/contract/web/{DELTA_ID}/\">visit</a>");
        let result = finalize_anchors(&html, true, None);
        assert!(result.contains(&format!("href=\"/v1/contract/web/{DELTA_ID}/\"")));
    }

    #[test]
    fn finalize_anchors_skips_href_rewrite_when_not_behind_gateway() {
        // dx serve / dev mode: there's no gateway behind us, so don't
        // rewrite to a same-origin path that won't resolve.
        let html =
            format!("<a href=\"http://127.0.0.1:7509/v1/contract/web/{DELTA_ID}/\">visit</a>");
        let result = finalize_anchors(&html, false, None);
        assert!(result.contains(&format!(
            "href=\"http://127.0.0.1:7509/v1/contract/web/{DELTA_ID}/\""
        )));
    }

    #[test]
    fn finalize_anchors_beautifies_bare_url_label() {
        // Markdown's `<URL>` autolink syntax produces an anchor where
        // the visible text equals the href. Beautify these to
        // `freenet:<id-prefix>` regardless of hosting.
        let html = format!(
            "<a href=\"http://gw/v1/contract/web/{RIVER_ID}/\">http://gw/v1/contract/web/{RIVER_ID}/</a>"
        );
        let result = finalize_anchors(&html, false, None);
        assert!(result.contains(">freenet:raAqMhMG</a>"));
    }

    #[test]
    fn finalize_anchors_keeps_user_label_for_freenet_link() {
        // `[my link](url)` markdown gives anchor inner != href; keep
        // the user's label.
        let html = format!("<a href=\"http://gw/v1/contract/web/{RIVER_ID}/\">My Custom Label</a>");
        let result = finalize_anchors(&html, false, None);
        assert!(result.contains(">My Custom Label</a>"));
        assert!(!result.contains("freenet:"));
    }

    #[test]
    fn finalize_anchors_beautifies_autolink_with_ampersand_in_query() {
        // Regression test for the `&amp;`-encoding concern raised in the
        // PR #22 skeptical review: GFM autolinks emit the SAME
        // entity-encoded form in both `href="..."` and the visible
        // text, so the byte-exact comparison `href == inner` still
        // holds and beautification fires. Without this test, a future
        // markdown crate bump that changes the encoding asymmetry
        // would silently skip beautification on URLs with `&` in the
        // query string.
        let html = format!(
            "<a href=\"http://gw/v1/contract/web/{RIVER_ID}/?a=1&amp;b=2\">http://gw/v1/contract/web/{RIVER_ID}/?a=1&amp;b=2</a>"
        );
        let result = finalize_anchors(&html, false, None);
        assert!(
            result.contains(">freenet:raAqMhMG/?a=1&amp;b=2</a>"),
            "expected beautified label with `&amp;amp;` preserved; got: {result}"
        );
    }

    #[test]
    fn parse_freenet_web_url_accepts_relative_path() {
        // Ivvor 2026-05-03 12:10: `[David's Place](/v1/contract/web/<id>/#prefix/page/about)`
        // — relative-path markdown link (no scheme/host). Without
        // this, the relative href fell through `parse_freenet_web_url`,
        // missed the same-contract check, and got `target="_blank"`.
        let url = format!("/v1/contract/web/{DELTA_ID}/#Fe5jaFmRnp/1/about");
        let parsed = parse_freenet_web_url(&url).expect("relative URL should parse");
        assert_eq!(parsed.contract_id, DELTA_ID);
        assert_eq!(parsed.suffix, "/#Fe5jaFmRnp/1/about");
        assert_eq!(parsed.absolute_path, url);
    }

    #[test]
    fn parse_freenet_web_url_rejects_path_relative_non_contract() {
        // Path-relative URL that doesn't hit the marker is not a
        // Freenet URL.
        assert!(parse_freenet_web_url("/some/other/path").is_none());
        assert!(parse_freenet_web_url("/v1/contract/web/").is_none());
    }

    #[test]
    fn parse_freenet_web_url_rejects_unrooted_path_without_scheme() {
        // No scheme AND no leading `/` — could be a fragment, a
        // relative path, anything. Don't try to parse.
        assert!(parse_freenet_web_url("v1/contract/web/abc").is_none());
        assert!(parse_freenet_web_url("foo").is_none());
    }

    #[test]
    fn finalize_anchors_treats_relative_same_contract_url_as_internal() {
        // The exact reproduction of Ivvor's case: relative-path
        // markdown link to the same contract. Must not get
        // `target="_blank"` and must keep the relative href intact.
        let html = format!(
            "<a href=\"/v1/contract/web/{DELTA_ID}/#Fe5jaFmRnp/1/about\">David's Place</a>"
        );
        let result = finalize_anchors(&html, true, Some(DELTA_ID));
        assert!(
            !result.contains("target=\"_blank\""),
            "relative same-contract URL must not get target=_blank: {result}"
        );
        assert!(result.contains(&format!(
            "href=\"/v1/contract/web/{DELTA_ID}/#Fe5jaFmRnp/1/about\""
        )));
    }

    #[test]
    fn finalize_anchors_treats_same_contract_url_as_internal() {
        // Regression test for Ivvor's 2026-05-03 11:55 report:
        // clicking a Delta-to-Delta link copied from the Share button
        // (which produces a full Freenet URL ending in `#prefix/id/slug`)
        // was opening a new tab instead of doing the in-iframe
        // hashchange navigation. When the URL points at our own
        // contract, treat it as internal so `target="_blank"` is NOT
        // added.
        let html = format!(
            "<a href=\"http://gw/v1/contract/web/{DELTA_ID}/#AmcVD92D3U/2/page-2\">jump</a>"
        );
        let result = finalize_anchors(&html, true, Some(DELTA_ID));
        assert!(
            !result.contains("target=\"_blank\""),
            "same-contract URL must not get target=_blank: {result}"
        );
        // The href is still rewritten to a same-origin path so
        // hashchange lands on the current iframe.
        assert!(result.contains(&format!(
            "href=\"/v1/contract/web/{DELTA_ID}/#AmcVD92D3U/2/page-2\""
        )));
    }

    #[test]
    fn finalize_anchors_treats_different_contract_url_as_external() {
        // The other half of the rule: a URL pointing at a *different*
        // contract IS external — it would navigate the iframe to a
        // different contract's WASM, which is fundamentally a tab-
        // level transition.
        let html = format!("<a href=\"http://gw/v1/contract/web/{RIVER_ID}/foo\">elsewhere</a>");
        let result = finalize_anchors(&html, true, Some(DELTA_ID));
        assert!(
            result.contains("target=\"_blank\""),
            "cross-contract URL should get target=_blank: {result}"
        );
    }

    #[test]
    fn finalize_anchors_handles_multiple_anchors_in_one_pass() {
        let html = format!(
            "<p>See <a href=\"http://gw/v1/contract/web/{RIVER_ID}/\">http://gw/v1/contract/web/{RIVER_ID}/</a> \
             or <a href=\"https://example.com\">Example</a> or <a href=\"#section\">jump</a>.</p>"
        );
        let result = finalize_anchors(&html, true, None);
        // Freenet beautification.
        assert!(result.contains(">freenet:raAqMhMG</a>"));
        // External link gets target=_blank.
        assert!(result.matches("target=\"_blank\"").count() == 2); // freenet + example
                                                                   // Internal hash anchor untouched.
        assert!(result.contains("href=\"#section\">jump</a>"));
    }
}
