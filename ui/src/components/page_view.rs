use dioxus::prelude::*;

use crate::state;
use delta_core::PageId;

#[component]
pub fn PageView() -> Element {
    let Some((page_id, page)) = state::current_page() else {
        return rsx! {
            div { class: "flex items-center justify-center h-full text-gray-400",
                p { "Select a page or create a new one" }
            }
        };
    };

    let rendered_html = render_markdown(&page.content);

    rsx! {
        div { class: "max-w-3xl mx-auto px-8 py-8",
            // Page header
            div { class: "flex items-center justify-between mb-6",
                h1 { class: "text-3xl font-bold text-gray-900",
                    "{page.title}"
                }
                div { class: "flex gap-2",
                    button {
                        class: "px-3 py-1.5 text-sm bg-gray-100 text-gray-700 rounded-md hover:bg-gray-200",
                        onclick: move |_| state::start_editing(),
                        "Edit"
                    }
                    button {
                        class: "px-3 py-1.5 text-sm bg-red-50 text-red-600 rounded-md hover:bg-red-100",
                        onclick: move |_| state::delete_page(page_id),
                        "Delete"
                    }
                }
            }

            // Rendered markdown content
            div {
                class: "prose",
                dangerous_inner_html: "{rendered_html}"
            }

            // Page metadata
            div { class: "mt-8 pt-4 border-t border-gray-100 text-xs text-gray-400",
                "Page #{page_id} · Last updated: {format_timestamp(page.updated_at)}"
            }
        }
    }
}

/// Render markdown to HTML, resolving `[[id|text]]` page links.
fn render_markdown(content: &str) -> String {
    let resolved = resolve_page_links(content);
    markdown::to_html(&resolved)
}

/// Replace `[[id|Display Text]]` with HTML links.
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
                    // Look up current title from state
                    let title = state::SITE
                        .read()
                        .pages
                        .get(&id)
                        .map(|p| p.title.clone())
                        .unwrap_or_else(|| display.to_string());
                    let slug = slugify(&title);
                    result.push_str(&format!("[{title}](/{id}/{slug})",));
                } else {
                    // Not a valid ID, keep as-is
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
    dt.map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
