use dioxus::prelude::*;

use crate::state;

#[component]
pub fn Sidebar() -> Element {
    let site = state::SITE.read();
    let current = *state::CURRENT_PAGE.read();

    let mut sorted_pages: Vec<_> = site.pages.iter().collect();
    sorted_pages.sort_by_key(|(id, _)| *id);

    rsx! {
        aside { class: "w-64 border-r border-gray-200 flex flex-col h-full bg-gray-50",
            // Site header
            div { class: "p-4 border-b border-gray-200",
                h1 { class: "text-lg font-bold text-gray-900 truncate",
                    "{site.config.config.name}"
                }
                if !site.config.config.description.is_empty() {
                    p { class: "text-xs text-gray-500 mt-1 truncate",
                        "{site.config.config.description}"
                    }
                }
            }

            // Page list
            nav { class: "flex-1 overflow-y-auto p-2",
                for (&id, page) in sorted_pages.iter() {
                    {
                        let is_selected = current == Some(id);
                        let bg = if is_selected { "bg-blue-100 text-blue-900" } else { "hover:bg-gray-100 text-gray-700" };
                        rsx! {
                            button {
                                class: "w-full text-left px-3 py-2 rounded-md text-sm mb-0.5 {bg}",
                                onclick: move |_| state::select_page(id),
                                "{page.title}"
                            }
                        }
                    }
                }
            }

            // New page button
            div { class: "p-3 border-t border-gray-200",
                button {
                    class: "w-full px-3 py-2 text-sm bg-blue-600 text-white rounded-md hover:bg-blue-700",
                    onclick: move |_| {
                        state::create_page("New Page".into());
                    },
                    "+ New Page"
                }
            }
        }
    }
}
