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
                onclick: move |evt| {
                    handle_link_click(evt);
                },
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

fn handle_link_click(evt: Event<MouseData>) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        if let Some(target) = evt.data().downcast::<web_sys::MouseEvent>() {
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

fn render_markdown(content: &str) -> String {
    let resolved = resolve_page_links(content);
    markdown::to_html(&resolved)
}

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
    dt.map(|d| d.format("%b %d, %Y").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
