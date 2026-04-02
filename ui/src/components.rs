mod add_site_dialog;
mod editor;
mod page_view;
mod pages_sidebar;
mod sites_sidebar;

use dioxus::prelude::*;

use crate::freenet_api;
use crate::state;

#[component]
pub fn App() -> Element {
    // Initialize example data and connect to Freenet on first render
    use_effect(|| {
        state::init_example_data();
        setup_hash_listener();
        freenet_api::connect_to_freenet();
        // Delegate registration happens after connection is established.
        // For now, register immediately — the WebSocket will queue the message.
        freenet_api::register_delegate();
    });

    let show_add_site = *state::SHOW_ADD_SITE.read();

    rsx! {
        div { class: "flex h-screen bg-bg text-text",
            sites_sidebar::SitesSidebar {}
            if show_add_site {
                main { class: "flex-1 overflow-y-auto bg-panel",
                    add_site_dialog::AddSiteDialog {}
                }
            } else {
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
}

/// Set up a listener for hash changes so page links work.
fn setup_hash_listener() {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::prelude::*;
        use wasm_bindgen::JsCast;

        let closure = Closure::<dyn Fn()>::new(|| {
            if let Some(window) = web_sys::window() {
                let hash = window.location().hash().unwrap_or_default();
                if let Some((prefix, page_id)) = state::parse_hash_route(&hash) {
                    let sites = state::SITES.read();
                    if sites.contains_key(&prefix) {
                        let current = state::CURRENT_SITE.read().clone();
                        drop(sites);

                        if current.as_deref() != Some(&prefix) {
                            state::select_site(&prefix);
                        }
                        if let Some(pid) = page_id {
                            if *state::CURRENT_PAGE.read() != Some(pid) {
                                state::select_page(pid);
                            }
                        }
                    }
                }
            }
        });

        if let Some(window) = web_sys::window() {
            let _ = window
                .add_event_listener_with_callback("hashchange", closure.as_ref().unchecked_ref());
        }
        closure.forget();
    }
}
