mod editor;
mod page_view;
mod sidebar;

use dioxus::prelude::*;

use crate::state;

#[component]
pub fn App() -> Element {
    // Initialize example data on first render
    use_effect(|| {
        state::init_example_data();
    });

    rsx! {
        div { class: "flex h-screen bg-bg text-text",
            sidebar::Sidebar {}
            main { class: "flex-1 overflow-y-auto bg-panel",
                {
                    if *state::EDITING.read() {
                        rsx! { editor::Editor {} }
                    } else {
                        rsx! { page_view::PageView {} }
                    }
                }
            }
        }
    }
}
