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

    rsx! {
        div { class: "max-w-2xl mx-auto px-10 py-12",
            // Page header
            div { class: "flex items-start justify-between mb-2",
                div { class: "flex-1 min-w-0" }
                if is_owner {
                    div { class: "flex gap-2 ml-4 flex-shrink-0",
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

fn format_timestamp(ts: u64) -> String {
    use chrono::{DateTime, Utc};
    let dt = DateTime::<Utc>::from_timestamp(ts as i64, 0);
    dt.map(|d| d.format("%b %d, %Y").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
