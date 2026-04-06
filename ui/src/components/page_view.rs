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
                        class: "px-3 py-1.5 text-xs text-text-muted hover:text-accent transition-colors rounded",
                        onclick: move |_| {
                            let current = *show_source.read();
                            show_source.set(!current);
                        },
                        if *show_source.read() { "Rendered" } else { "Source" }
                    }
                    if is_owner {
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
                    class: "editor-textarea p-5 text-sm rounded-lg bg-panel-warm border border-border-light overflow-x-auto whitespace-pre-wrap",
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
    }
}

/// Render markdown to HTML, resolving `[[id|text]]` page links as hash links.
fn render_markdown(content: &str) -> String {
    let resolved = resolve_page_links(content);
    markdown::to_html(&resolved)
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

            result.push_str(&resolved.unwrap_or_else(|| format!("[[{link_content}]]")));
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
