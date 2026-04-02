use dioxus::prelude::*;

use crate::state;

#[component]
pub fn Editor() -> Element {
    let Some((page_id, _page)) = state::current_page() else {
        return rsx! {
            div { class: "flex items-center justify-center h-full text-text-muted",
                p { "No page selected" }
            }
        };
    };

    // Initialize editor content when entering edit mode
    use_effect(move || {
        let page = state::SITE.read().pages.get(&page_id).cloned();
        if let Some(page) = page {
            *state::EDITOR_TITLE.write() = page.title.clone();
            *state::EDITOR_CONTENT.write() = page.content.clone();
        }
    });

    let title = state::EDITOR_TITLE.read().clone();
    let content = state::EDITOR_CONTENT.read().clone();
    let preview_html = markdown::to_html(&content);

    rsx! {
        div { class: "flex flex-col h-full",
            // Toolbar
            div { class: "flex items-center gap-3 px-6 py-3 border-b border-border bg-surface",
                input {
                    class: "text-xl font-bold bg-transparent border-none outline-none flex-1 text-text placeholder-text-muted",
                    r#type: "text",
                    value: "{title}",
                    placeholder: "Page title",
                    oninput: move |evt| {
                        *state::EDITOR_TITLE.write() = evt.value().to_string();
                    },
                }
                button {
                    class: "px-4 py-2 text-sm bg-accent text-text-inverse rounded-lg hover:bg-accent-hover font-medium transition-colors",
                    onclick: move |_| state::save_current_page(),
                    "Save"
                }
                button {
                    class: "px-4 py-2 text-sm bg-surface-hover text-text rounded-lg hover:bg-border transition-colors",
                    onclick: move |_| {
                        *state::EDITING.write() = false;
                    },
                    "Cancel"
                }
            }

            // Editor + Preview split
            div { class: "flex flex-1 overflow-hidden",
                // Editor pane
                div { class: "flex-1 flex flex-col border-r border-border",
                    div { class: "px-4 py-2 text-xs font-medium text-text-muted bg-surface border-b border-border uppercase tracking-wide",
                        "Markdown"
                    }
                    textarea {
                        class: "editor-textarea flex-1 w-full p-4 resize-none outline-none text-sm",
                        value: "{content}",
                        placeholder: "Write your page content in Markdown...",
                        oninput: move |evt| {
                            *state::EDITOR_CONTENT.write() = evt.value().to_string();
                        },
                    }
                }

                // Preview pane
                div { class: "flex-1 flex flex-col bg-panel",
                    div { class: "px-4 py-2 text-xs font-medium text-text-muted bg-surface border-b border-border uppercase tracking-wide",
                        "Preview"
                    }
                    div { class: "flex-1 overflow-y-auto p-6",
                        div {
                            class: "prose max-w-none",
                            dangerous_inner_html: "{preview_html}"
                        }
                    }
                }
            }
        }
    }
}
