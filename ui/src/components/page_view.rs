use dioxus::prelude::*;

use crate::state;
use crate::state::SiteRole;
use delta_core::PageId;

#[component]
pub fn PageView() -> Element {
    let Some((page_id, page)) = state::current_page() else {
        return rsx! {
            div { class: "flex items-center justify-center h-full text-text-muted",
                div { class: "text-center",
                    p { class: "text-lg", "No page selected" }
                    p { class: "text-sm mt-1", "Choose a page from the sidebar" }
                }
            }
        };
    };

    let is_owner = state::current_site()
        .map(|s| s.role == SiteRole::Owner)
        .unwrap_or(false);

    let rendered_html = render_markdown(&page.content);

    rsx! {
        div { class: "max-w-3xl mx-auto px-8 py-8",
            // Page header
            div { class: "flex items-center justify-between mb-8",
                h1 { class: "text-3xl font-bold text-text",
                    "{page.title}"
                }
                if is_owner {
                    div { class: "flex gap-2",
                        button {
                            class: "px-4 py-2 text-sm bg-accent text-text-inverse rounded-lg hover:bg-accent-hover font-medium transition-colors",
                            onclick: move |_| state::start_editing(),
                            "Edit"
                        }
                        button {
                            class: "px-4 py-2 text-sm bg-surface text-text-muted rounded-lg hover:bg-surface-hover transition-colors",
                            onclick: move |_| state::delete_page(page_id),
                            "Delete"
                        }
                    }
                }
            }

            // Rendered markdown content
            // Page links are rendered as onclick handlers that call navigate_to_page
            div {
                class: "prose",
                dangerous_inner_html: "{rendered_html}",
                onclick: move |evt| {
                    // Intercept clicks on internal page links
                    handle_link_click(evt);
                },
            }

            // Page metadata
            div { class: "mt-12 pt-4 border-t border-border text-xs text-text-muted",
                "Page #{page_id} · Updated {format_timestamp(page.updated_at)}"
            }
        }
    }
}

/// Handle clicks on links within rendered markdown.
/// Internal page links have href="/page/{id}" and are intercepted.
fn handle_link_click(evt: Event<MouseData>) {
    // We need to check if the clicked element (or parent) is an anchor
    // with an internal page link. In WASM, we read the event target.
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        if let Some(target) = evt.data().downcast::<web_sys::MouseEvent>() {
            // Walk up to find the <a> element
            if let Some(element) = target
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            {
                let anchor = if element.tag_name() == "A" {
                    Some(element)
                } else {
                    element.closest("a").ok().flatten()
                };

                if let Some(a) = anchor {
                    if let Some(href) = a.get_attribute("href") {
                        // Check if it's an internal page link: /page_id/slug
                        let path = href.trim_start_matches('/');
                        if let Some(id_str) = path.split('/').next() {
                            if let Ok(page_id) = id_str.parse::<PageId>() {
                                evt.prevent_default();
                                state::navigate_to_page(page_id);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = evt;
}

/// Render markdown to HTML, resolving `[[id|text]]` page links.
fn render_markdown(content: &str) -> String {
    let resolved = resolve_page_links(content);
    markdown::to_html(&resolved)
}

/// Replace `[[id|Display Text]]` with internal links.
fn resolve_page_links(content: &str) -> String {
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
                        .get(&state::CURRENT_SITE.read().clone().unwrap_or_default())
                        .and_then(|s| s.state.pages.get(&id))
                        .map(|p| p.title.clone())
                        .unwrap_or_else(|| display.to_string());
                    let slug = slugify(&title);
                    // Use /id/slug format — intercepted by onclick handler
                    result.push_str(&format!("[{title}](/{id}/{slug})"));
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

fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn format_timestamp(ts: u64) -> String {
    use chrono::{DateTime, Utc};
    let dt = DateTime::<Utc>::from_timestamp(ts as i64, 0);
    dt.map(|d| d.format("%b %d, %Y at %H:%M UTC").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
