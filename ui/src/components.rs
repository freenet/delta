mod editor;
mod page_view;
mod pages_sidebar;
mod sites_sidebar;

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
            sites_sidebar::SitesSidebar {}
            pages_sidebar::PagesSidebar {}
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
