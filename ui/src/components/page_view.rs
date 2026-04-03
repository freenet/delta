use dioxus::prelude::*;

use crate::state;
use crate::state::SiteRole;
use delta_core::PageId;

#[component]
pub fn PageView() -> Element {
    let Some((page_id, page)) = state::current_page() else {
        return rsx! {
            div { class: "flex items-center justify-center h-full",
                div { class: "text-center",
                    span { class: "delta-mark text-3xl w-12 h-12 text-[28px] opacity-30 mb-4 inline-flex items-center justify-center rounded-xl",
                        "\u{0394}"
                    }
                    p { class: "text-text-muted-light text-sm mt-4", "Select a page to start reading" }
                }
            }
        };
    };

    let is_owner = state::current_site()
        .map(|s| s.role == SiteRole::Owner)
        .unwrap_or(false);

    let rendered_html = render_markdown(&page.content);

    let site_prefix = state::current_site()
        .map(|s| s.prefix.clone())
        .unwrap_or_default();
    let page_title_for_share = page.title.clone();
    let mut link_copied = use_signal(|| false);
    let mut confirming_delete = use_signal(|| false);

    rsx! {
        div { class: "max-w-2xl mx-auto px-10 py-12",
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
                        },
                        if *link_copied.read() { "Copied!" } else { "Share" }
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

            // Rendered markdown
            div {
                class: "prose",
                dangerous_inner_html: "{rendered_html}",
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

/// Replace `[[id|Display Text]]` with hash-routed links.
fn resolve_page_links(content: &str) -> String {
    let prefix = state::CURRENT_SITE.read().clone().unwrap_or_default();

    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find("[[") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];

        if let Some(end) = after_open.find("]]") {
            let link_content = &after_open[..end];
            if let Some((id_str, display)) = link_content.split_once('|') {
                if let Ok(id) = id_str.trim().parse::<PageId>() {
                    let title = state::SITES
                        .read()
                        .get(&prefix)
                        .and_then(|s| s.state.pages.get(&id))
                        .map(|p| p.title.clone())
                        .unwrap_or_else(|| display.to_string());
                    // Hash link so the hashchange listener picks it up
                    let hash = state::build_hash_route(&prefix, Some(id), Some(&title));
                    result.push_str(&format!("[{title}]({hash})"));
                } else {
                    result.push_str(&format!("[[{link_content}]]"));
                }
            } else {
                result.push_str(&format!("[[{link_content}]]"));
            }
            rest = &after_open[end + 2..];
        } else {
            result.push_str("[[");
            rest = after_open;
        }
    }
    result.push_str(rest);
    result
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
                        if let Some(style) = textarea.dyn_ref::<web_sys::HtmlElement>().map(|e| e.style()) {
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
