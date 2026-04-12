use delta_core::SiteKeyExport;
use dioxus::prelude::*;

use crate::state;

/// Signal to control export modal visibility.
pub static SHOW_EXPORT: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// The armored export token (set by delegate response handler).
pub static EXPORT_TOKEN: GlobalSignal<Option<String>> = GlobalSignal::new(|| None);

/// Error message when export is not possible.
pub static EXPORT_ERROR: GlobalSignal<Option<String>> = GlobalSignal::new(|| None);

/// The site we are currently trying to export. Set when the user clicks
/// Export; consulted by `handle_signing_key_response` so that responses
/// are correlated with the request that triggered them. Without this, a
/// `SigningKey` response arriving from a concurrent legacy-migration probe,
/// or arriving after the user switched sites, could be paired with the
/// wrong site's `owner_pubkey`.
pub static PENDING_EXPORT: GlobalSignal<Option<PendingExport>> = GlobalSignal::new(|| None);

#[derive(Clone)]
pub struct PendingExport {
    pub prefix: String,
    pub name: String,
    pub owner_pubkey: [u8; 32],
}

/// Request the signing key from the delegate for export.
pub fn request_export() {
    // Clear previous state
    *EXPORT_TOKEN.write() = None;
    *EXPORT_ERROR.write() = None;
    *PENDING_EXPORT.write() = None;
    *SHOW_EXPORT.write() = true;

    let Some(site) = state::current_site() else {
        *EXPORT_ERROR.write() = Some("No site selected to export.".to_string());
        return;
    };

    // Only owners can export — Visitor sites have no signing key to fetch.
    if site.role != state::SiteRole::Owner {
        *EXPORT_ERROR.write() =
            Some("Only owned sites can be exported, not visited sites.".to_string());
        return;
    }

    // If the key is only in a legacy delegate, we can't extract raw bytes
    // (pre-V7 delegates did not support per-prefix GetSigningKey).
    if !crate::freenet_api::delegate::has_current_key() {
        *EXPORT_ERROR.write() = Some(
            "Signing key is stored in a previous delegate version and cannot be exported. \
             Create a new site to get an exportable key."
                .to_string(),
        );
        return;
    }

    // Snapshot the site we're exporting so the response handler can
    // correlate the reply with this specific request, not with whatever
    // `current_site()` happens to return when the reply arrives.
    *PENDING_EXPORT.write() = Some(PendingExport {
        prefix: site.prefix.clone(),
        name: site.name.clone(),
        owner_pubkey: site.owner_pubkey,
    });

    let request = delta_core::DelegateRequest::GetSigningKeyForPrefix {
        prefix: site.prefix.clone(),
    };
    crate::freenet_api::delegate::send_delegate_request_pub(&request);
}

/// Pure validation: does `key_bytes` correspond to the given `owner_pubkey`?
///
/// Extracted from `handle_signing_key_response` so the defence-in-depth
/// check can be unit-tested without touching Dioxus signals. Returns the
/// parsed `SigningKey` on success so the caller can reuse it without a
/// second allocation.
pub(crate) fn validate_signing_key_matches(
    key_bytes: &[u8],
    owner_pubkey: &[u8; 32],
) -> Result<ed25519_dalek::SigningKey, &'static str> {
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "Delegate returned malformed signing key.")?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&key_array);
    if signing_key.verifying_key().to_bytes() != *owner_pubkey {
        return Err(
            "Delegate returned a signing key that does not match this site's owner. \
                    Export aborted to avoid producing a broken token.",
        );
    }
    Ok(signing_key)
}

/// Called by the delegate response handler when SigningKey arrives.
pub fn handle_signing_key_response(key_bytes: Vec<u8>) {
    // Only consume the response if the export modal has an outstanding
    // request — legacy-migration probes also produce SigningKey responses
    // and must not populate the export state.
    let Some(pending) = PENDING_EXPORT.read().clone() else {
        return;
    };

    match validate_signing_key_matches(&key_bytes, &pending.owner_pubkey) {
        Ok(_) => {
            let export = SiteKeyExport {
                signing_key: key_bytes,
                owner_pubkey: pending.owner_pubkey.to_vec(),
                prefix: pending.prefix,
                name: pending.name,
            };
            *EXPORT_TOKEN.write() = Some(export.to_armored());
            *PENDING_EXPORT.write() = None;
        }
        Err(msg) => {
            *EXPORT_ERROR.write() = Some(msg.to_string());
            *PENDING_EXPORT.write() = None;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn validate_accepts_key_that_matches_owner() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        assert!(validate_signing_key_matches(&sk.to_bytes(), &pk).is_ok());
    }

    #[test]
    fn validate_rejects_key_that_does_not_match_owner() {
        // Exactly the pre-V7 bug: the delegate returned a key from the
        // legacy single-key slot that belonged to a different site than
        // the one the user was exporting. This regression test locks in
        // the defence-in-depth check that refuses to emit such a token.
        let exported_site_owner = SigningKey::generate(&mut OsRng).verifying_key().to_bytes();
        let wrong_key = SigningKey::generate(&mut OsRng);
        let result = validate_signing_key_matches(&wrong_key.to_bytes(), &exported_site_owner);
        assert!(
            result.is_err(),
            "export must refuse a signing key whose pubkey does not match the site owner"
        );
    }

    #[test]
    fn validate_rejects_malformed_key_bytes() {
        let owner = SigningKey::generate(&mut OsRng).verifying_key().to_bytes();
        assert!(validate_signing_key_matches(&[0u8; 16], &owner).is_err());
        assert!(validate_signing_key_matches(&[0u8; 64], &owner).is_err());
    }
}
