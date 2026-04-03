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
pub(crate) fn set_document_title(title: &str) {
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

/// Set up listeners for hash changes (internal navigation) and
/// __freenet_shell__ hash messages (deep-link from parent shell).
fn setup_hash_listener() {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::prelude::*;
        use wasm_bindgen::JsCast;

        // Handle internal hashchange events
        let hashchange = Closure::<dyn Fn()>::new(|| {
            handle_hash_navigation();
        });
        if let Some(window) = web_sys::window() {
            let _ = window.add_event_listener_with_callback(
                "hashchange",
                hashchange.as_ref().unchecked_ref(),
            );
        }
        hashchange.forget();

        // Handle __freenet_shell__ hash messages from parent shell (deep-linking)
        let message_handler =
            Closure::<dyn Fn(web_sys::MessageEvent)>::new(|event: web_sys::MessageEvent| {
                let data = event.data();
                if let Some(obj) = data.dyn_ref::<js_sys::Object>() {
                    // Check if it's a __freenet_shell__ message
                    let is_shell = js_sys::Reflect::get(obj, &"__freenet_shell__".into())
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if !is_shell {
                        return;
                    }
                    let msg_type = js_sys::Reflect::get(obj, &"type".into())
                        .ok()
                        .and_then(|v| v.as_string())
                        .unwrap_or_default();
                    if msg_type == "hash" {
                        if let Some(hash) = js_sys::Reflect::get(obj, &"hash".into())
                            .ok()
                            .and_then(|v| v.as_string())
                        {
                            web_sys::console::log_1(
                                &format!("Delta: received hash from shell: {hash}").into(),
                            );
                            // Check if WebSocket is connected
                            let connected = matches!(
                                &*freenet_api::CONNECTION_STATUS.read(),
                                freenet_api::ConnectionStatus::Connected
                            );
                            if connected {
                                navigate_from_hash(&hash);
                            } else {
                                // Queue for when connection is ready
                                web_sys::console::log_1(
                                    &"Delta: queuing hash for after connection".into(),
                                );
                                *state::PENDING_HASH.write() = Some(hash);
                            }
                        }
                    }
                }
            });
        if let Some(window) = web_sys::window() {
            let _ = window.add_event_listener_with_callback(
                "message",
                message_handler.as_ref().unchecked_ref(),
            );
        }
        message_handler.forget();
    }
}

/// Navigate to a hash route - called when connected.
#[allow(dead_code)]
fn navigate_from_hash(hash: &str) {
    if let Some((prefix, page_id)) = state::parse_hash_route(hash) {
        if let Some(pid) = page_id {
            *state::PENDING_PAGE_ID.write() = Some(pid);
        }
        if state::SITES.read().contains_key(&prefix) {
            state::select_site(&prefix);
        } else {
            state::visit_site(prefix);
        }
    }
}

/// Replay any pending hash navigation after connection is established.
#[allow(dead_code)]
pub fn replay_pending_hash() {
    if let Some(hash) = state::PENDING_HASH.write().take() {
        #[cfg(target_arch = "wasm32")]
        web_sys::console::log_1(&format!("Delta: replaying queued hash: {hash}").into());
        navigate_from_hash(&hash);
    }
}

#[cfg(target_arch = "wasm32")]
fn handle_hash_navigation() {
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
}
