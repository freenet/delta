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

    rsx! {
        div { class: "max-w-2xl mx-auto px-10 py-12",
            // Page header
            div { class: "flex items-start justify-between mb-2",
                div { class: "flex-1 min-w-0" }
                div { class: "flex gap-2 ml-4 flex-shrink-0",
                    // Share button — always visible
                    button {
                        class: "px-3 py-2 text-sm text-text-muted hover:text-accent hover:bg-accent-glow rounded-lg transition-colors",
                        title: "Copy link to this page",
                        onclick: move |_| {
                            copy_page_url(&site_prefix, page_id, &page_title_for_share);
                            link_copied.set(true);
                        },
                        if *link_copied.read() { "Copied!" } else { "Share" }
                    }
                    if is_owner {
                        button {
                            class: "btn-primary px-4 py-2 text-sm",
                            onclick: move |_| state::start_editing(),
                            "Edit"
                        }
                        button {
                            class: "btn-ghost px-4 py-2 text-sm",
                            onclick: move |_| state::delete_page(page_id),
                            "Delete"
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
fn copy_page_url(prefix: &str, page_id: PageId, title: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let location = window.location();
            let origin = location.origin().unwrap_or_default();
            let pathname = location.pathname().unwrap_or_default();
            let hash = state::build_hash_route(prefix, Some(page_id), Some(title));
            let url = format!("{origin}{pathname}{hash}");
            let clipboard = window.navigator().clipboard();
            let _ = clipboard.write_text(&url);

            // Also update the hash via postMessage for #3747
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
