use dioxus::prelude::*;

use crate::state;

#[component]
pub fn AddSiteDialog() -> Element {
    let mut site_name = use_signal(|| "My New Site".to_string());

    rsx! {
        div { class: "flex items-center justify-center h-full bg-panel",
            div { class: "max-w-md w-full mx-8",
                // Header
                div { class: "mb-8 text-center",
                    span { class: "delta-mark inline-flex mb-4", "\u{0394}" }
                    h2 { class: "text-2xl font-semibold text-text", "Create a new site" }
                    p { class: "text-sm text-text-muted-light mt-2",
                        "Your site will be published on the Freenet network."
                    }
                }

                // Form
                div { class: "space-y-4",
                    div {
                        label { class: "block text-xs font-medium text-text-muted-light mb-1.5 uppercase tracking-wide",
                            "Site name"
                        }
                        input {
                            class: "w-full px-4 py-3 bg-panel-warm border border-border-light rounded-lg text-text outline-none focus:border-accent text-sm",
                            r#type: "text",
                            value: "{site_name}",
                            placeholder: "My Site",
                            autofocus: true,
                            oninput: move |evt| site_name.set(evt.value().to_string()),
                            onkeypress: move |evt| {
                                if evt.key() == Key::Enter {
                                    let name = site_name.read().clone();
                                    if !name.trim().is_empty() {
                                        state::create_new_site(name);
                                    }
                                }
                            },
                        }
                    }

                    div { class: "flex gap-3 pt-2",
                        button {
                            class: "btn-primary flex-1 px-4 py-3 text-sm",
                            onclick: move |_| {
                                let name = site_name.read().clone();
                                if !name.trim().is_empty() {
                                    state::create_new_site(name);
                                }
                            },
                            "Create Site"
                        }
                        button {
                            class: "btn-ghost px-4 py-3 text-sm",
                            onclick: move |_| {
                                *state::SHOW_ADD_SITE.write() = false;
                            },
                            "Cancel"
                        }
                    }
                }
            }
        }
    }
}
