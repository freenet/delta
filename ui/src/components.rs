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
    // Initialize once — use_hook runs only on first mount, not on re-renders
    use_hook(|| {
        #[cfg(target_arch = "wasm32")]
        web_sys::console::log_1(
            &format!(
                "Delta: built {} ({})",
                env!("BUILD_TIMESTAMP_ISO"),
                env!("GIT_COMMIT")
            )
            .into(),
        );
        set_document_title("Delta");
        setup_hash_listener();
        freenet_api::connect_to_freenet();
        state::init_from_hash();
    });

    let show_add_site = *state::SHOW_ADD_SITE.read();
    let has_sites = !state::SITES.read().is_empty();
    let has_current = state::CURRENT_SITE.read().is_some();

    rsx! {
        document::Link { rel: "icon", href: asset!("/assets/favicon.svg") }
        div { class: "flex h-screen bg-bg text-text",
            sites_sidebar::SitesSidebar {}
            if show_add_site {
                main { class: "flex-1 overflow-y-auto bg-panel",
                    add_site_dialog::AddSiteDialog {}
                }
            } else if !has_sites || !has_current {
                // Welcome screen — no sites yet
                main { class: "flex-1 overflow-y-auto bg-panel",
                    div { class: "flex items-center justify-center h-full",
                        div { class: "text-center max-w-md mx-8",
                            span { class: "delta-mark inline-flex mb-6 w-16 h-16 text-[32px] rounded-2xl", "\u{0394}" }
                            h1 { class: "text-2xl font-semibold text-text mb-2", "Welcome to Delta" }
                            p { class: "text-sm text-text-muted-light mb-8 leading-relaxed",
                                "Decentralized publishing on Freenet. Create your own site or visit one using a site code."
                            }
                            div { class: "flex gap-3 justify-center",
                                button {
                                    class: "btn-primary px-6 py-3 text-sm",
                                    onclick: move |_| state::show_add_site_prompt(),
                                    "Get Started"
                                }
                            }
                        }
                    }
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

/// Set the document title, notifying the gateway shell via postMessage.
fn set_document_title(title: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsValue;
        if let Some(window) = web_sys::window() {
            // Set title on our own document
            if let Some(doc) = window.document() {
                doc.set_title(title);
            }
            // Notify the gateway shell (we're inside an iframe)
            let msg = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &msg,
                &JsValue::from_str("__freenet_shell__"),
                &JsValue::TRUE,
            );
            let _ = js_sys::Reflect::set(
                &msg,
                &JsValue::from_str("type"),
                &JsValue::from_str("title"),
            );
            let _ =
                js_sys::Reflect::set(&msg, &JsValue::from_str("title"), &JsValue::from_str(title));
            let target = window.parent().ok().flatten().unwrap_or(window);
            let _ = target.post_message(&msg, "*");
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = title;
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
