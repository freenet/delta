use delta_core::SiteKeyExport;
use dioxus::prelude::*;

use crate::state;

/// Signal to control export modal visibility.
pub static SHOW_EXPORT: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// The armored export token (set by delegate response handler).
pub static EXPORT_TOKEN: GlobalSignal<Option<String>> = GlobalSignal::new(|| None);

/// Error message when export is not possible.
pub static EXPORT_ERROR: GlobalSignal<Option<String>> = GlobalSignal::new(|| None);

/// Request the signing key from the delegate for export.
pub fn request_export() {
    // Clear previous state
    *EXPORT_TOKEN.write() = None;
    *EXPORT_ERROR.write() = None;

    // The currently-selected site is the one being exported. Without a
    // prefix the delegate would return whatever happens to live in the
    // legacy single-key slot, which may not match this site's owner_pubkey
    // at all — so the exported token would contain a key that can't sign
    // valid updates on the importing node.
    let Some(site) = state::current_site() else {
        *EXPORT_ERROR.write() = Some("No site selected to export.".to_string());
        *SHOW_EXPORT.write() = true;
        return;
    };

    // GetSigningKey only works on the current delegate (V5+).
    // If the key is in a legacy delegate, we can't extract raw bytes.
    if !crate::freenet_api::delegate::has_current_key() {
        *EXPORT_ERROR.write() = Some(
            "Signing key is stored in a previous delegate version and cannot be exported. \
             Create a new site to get an exportable key."
                .to_string(),
        );
        *SHOW_EXPORT.write() = true;
        return;
    }

    *SHOW_EXPORT.write() = true;
    let request = delta_core::DelegateRequest::GetSigningKey {
        prefix: Some(site.prefix.clone()),
    };
    crate::freenet_api::delegate::send_delegate_request_pub(&request);
}

/// Called by the delegate response handler when SigningKey arrives.
pub fn handle_signing_key_response(key_bytes: Vec<u8>) {
    // Legacy-migration code path also fires SigningKey requests; only treat
    // this response as an export result when the export modal is open.
    if !*SHOW_EXPORT.read() {
        return;
    }
    let Some(site) = state::current_site() else {
        return;
    };

    // Defence in depth: verify the returned signing key actually matches
    // the current site's owner_pubkey. The pre-V7 delegate only had a
    // legacy single-key slot and would silently return the wrong key for
    // sites with per-prefix storage, producing an export token that could
    // not sign valid updates after import.
    let key_array: [u8; 32] = match key_bytes.as_slice().try_into() {
        Ok(a) => a,
        Err(_) => {
            *EXPORT_ERROR.write() = Some("Delegate returned malformed signing key.".to_string());
            return;
        }
    };
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&key_array);
    if signing_key.verifying_key().to_bytes() != site.owner_pubkey {
        *EXPORT_ERROR.write() = Some(
            "Delegate returned a signing key that does not match this site's owner. \
             Export aborted to avoid producing a broken token."
                .to_string(),
        );
        return;
    }

    let export = SiteKeyExport {
        signing_key: key_bytes,
        owner_pubkey: site.owner_pubkey.to_vec(),
        prefix: site.prefix.clone(),
        name: site.name.clone(),
    };
    *EXPORT_TOKEN.write() = Some(export.to_armored());
}

#[component]
pub fn ExportKeyModal() -> Element {
    if !*SHOW_EXPORT.read() {
        return rsx! {};
    }

    let token = EXPORT_TOKEN.read().clone();
    let error = EXPORT_ERROR.read().clone();
    let mut copied = use_signal(|| false);

    rsx! {
        // Modal overlay
        div {
            style: "position: absolute; inset: 0; background: rgba(0,0,0,0.5); display: flex; align-items: center; justify-content: center; z-index: 50;",
            onclick: move |_| *SHOW_EXPORT.write() = false,
            // Modal content
            div {
                class: "bg-panel rounded-xl shadow-lg w-96 p-6",
                onclick: move |evt| evt.stop_propagation(),
                h2 { class: "text-lg font-semibold text-text mb-1", "Export Site Key" }
                if let Some(err) = &error {
                    // Error state -- show message with only a Close button
                    p { class: "text-sm text-red-400 py-6 text-center", "{err}" }
                    div { class: "flex justify-end",
                        button {
                            class: "px-4 py-2 text-sm text-text-muted hover:text-text transition-colors rounded",
                            onclick: move |_| *SHOW_EXPORT.write() = false,
                            "Close"
                        }
                    }
                } else if let Some(armored) = &token {
                    p { class: "text-xs text-text-muted-light mb-4",
                        "This token contains your private signing key. Treat it like a password - do not share it publicly. Use it to import this site's ownership on another device."
                    }
                    textarea {
                        class: "w-full h-40 p-3 text-xs font-mono bg-panel-warm border border-border-light rounded-lg text-text resize-none outline-none",
                        readonly: true,
                        value: "{armored}",
                    }
                    div { class: "flex gap-3 mt-4",
                        button {
                            class: "px-4 py-2 text-sm text-accent border border-accent hover:bg-accent hover:text-text-inverse rounded-lg transition-colors font-medium",
                            onclick: move |_| {
                                if let Some(t) = &*EXPORT_TOKEN.read() {
                                    copy_text(t);
                                }
                                copied.set(true);
                                #[cfg(target_arch = "wasm32")]
                                {
                                    let mut signal = copied;
                                    wasm_bindgen_futures::spawn_local(async move {
                                        gloo_timers::future::sleep(std::time::Duration::from_secs(2)).await;
                                        signal.set(false);
                                    });
                                }
                            },
                            if *copied.read() { "Copied!" } else { "Copy to Clipboard" }
                        }
                        button {
                            class: "px-4 py-2 text-sm text-text-muted hover:text-text transition-colors rounded",
                            onclick: move |_| *SHOW_EXPORT.write() = false,
                            "Close"
                        }
                    }
                } else {
                    p { class: "text-xs text-text-muted-light mb-4",
                        "This token contains your private signing key. Treat it like a password - do not share it publicly. Use it to import this site's ownership on another device."
                    }
                    p { class: "text-sm text-text-muted-light py-8 text-center",
                        "Retrieving signing key..."
                    }
                }
            }
        }
    }
}

fn copy_text(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        if let Some(window) = web_sys::window() {
            if let Some(doc) = window.document() {
                if let Ok(el) = doc.create_element("textarea") {
                    if let Some(textarea) = el.dyn_ref::<web_sys::HtmlTextAreaElement>() {
                        textarea.set_value(text);
                        if let Some(style) = textarea
                            .dyn_ref::<web_sys::HtmlElement>()
                            .map(|e| e.style())
                        {
                            let _ = style.set_property("position", "fixed");
                            let _ = style.set_property("opacity", "0");
                        }
                        if let Some(body) = doc.body() {
                            let _ = body.append_child(textarea);
                            textarea.select();
                            if let Some(html_doc) = doc.dyn_ref::<web_sys::HtmlDocument>() {
                                let _ = html_doc.exec_command("copy");
                            }
                            let _ = body.remove_child(textarea);
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}
