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
        div { class: "flex h-screen bg-white text-gray-900",
            sidebar::Sidebar {}
            main { class: "flex-1 overflow-y-auto",
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
