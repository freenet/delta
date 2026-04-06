use delta_core::SiteKeyExport;
use dioxus::prelude::*;

use crate::state;

/// Signal to control export modal visibility.
pub static SHOW_EXPORT: GlobalSignal<bool> = GlobalSignal::new(|| false);

#[component]
pub fn ExportKeyModal() -> Element {
    let Some(site) = state::current_site() else {
        return rsx! {};
    };

    if !*SHOW_EXPORT.read() {
        return rsx! {};
    }

    // Build the export token - we need the signing key from the delegate.
    // For now, show a placeholder until we wire up GetSigningKey from delegate.
    // The signing key was stored in the delegate on site creation.
    let export = SiteKeyExport {
        signing_key: Vec::new(), // Placeholder - needs delegate round-trip
        owner_pubkey: site.owner_pubkey.to_vec(),
        prefix: site.prefix.clone(),
        name: site.name.clone(),
    };
    let armored = export.to_armored();
    let mut copied = use_signal(|| false);

    rsx! {
        // Modal overlay
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| *SHOW_EXPORT.write() = false,
            // Modal content
            div {
                class: "bg-panel rounded-xl shadow-lg max-w-lg w-full mx-4 p-6",
                onclick: move |evt| evt.stop_propagation(),
                h2 { class: "text-lg font-semibold text-text mb-1", "Export Site Key" }
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
                            copy_text(&armored);
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
