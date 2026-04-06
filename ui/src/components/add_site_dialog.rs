use dioxus::prelude::*;

use crate::state;

#[component]
pub fn AddSiteDialog() -> Element {
    let mut tab = use_signal(|| "create");
    let mut site_name = use_signal(|| "My New Site".to_string());
    let mut contract_id = use_signal(String::new);
    let mut import_token = use_signal(String::new);
    let mut import_error = use_signal(|| None::<String>);

    let active_tab = *tab.read();

    rsx! {
        div { class: "flex items-center justify-center h-full bg-panel",
            div { class: "max-w-md w-full mx-8",
                // Header
                div { class: "mb-6 text-center",
                    span { class: "delta-mark inline-flex mb-4", "\u{0394}" }
                }

                // Tabs
                div { class: "flex gap-1 mb-6 bg-panel-warm rounded-lg p-1",
                    button {
                        class: if active_tab == "create" { "flex-1 px-4 py-2 text-sm font-medium rounded-md bg-panel shadow-sm text-text" } else { "flex-1 px-4 py-2 text-sm font-medium rounded-md text-text-muted-light hover:text-text" },
                        onclick: move |_| tab.set("create"),
                        "Create Site"
                    }
                    button {
                        class: if active_tab == "visit" { "flex-1 px-4 py-2 text-sm font-medium rounded-md bg-panel shadow-sm text-text" } else { "flex-1 px-4 py-2 text-sm font-medium rounded-md text-text-muted-light hover:text-text" },
                        onclick: move |_| tab.set("visit"),
                        "Visit Site"
                    }
                    button {
                        class: if active_tab == "import" { "flex-1 px-4 py-2 text-sm font-medium rounded-md bg-panel shadow-sm text-text" } else { "flex-1 px-4 py-2 text-sm font-medium rounded-md text-text-muted-light hover:text-text" },
                        onclick: move |_| tab.set("import"),
                        "Import Key"
                    }
                }

                if active_tab == "create" {
                    // Create site form
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
                        p { class: "text-xs text-text-muted-light",
                            "Your site will be published on the Freenet network with a unique address."
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
                                "Create"
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
                if active_tab == "visit" {
                    // Visit site form
                    div { class: "space-y-4",
                        div {
                            label { class: "block text-xs font-medium text-text-muted-light mb-1.5 uppercase tracking-wide",
                                "Site Code"
                            }
                            input {
                                class: "w-full px-4 py-3 bg-panel-warm border border-border-light rounded-lg text-text outline-none focus:border-accent text-sm font-mono tracking-wider",
                                r#type: "text",
                                value: "{contract_id}",
                                placeholder: "8uYFEDnGJk",
                                maxlength: "10",
                                autofocus: true,
                                oninput: move |evt| contract_id.set(evt.value().to_string()),
                                onkeypress: move |evt| {
                                    if evt.key() == Key::Enter {
                                        let id = contract_id.read().clone();
                                        if !id.trim().is_empty() {
                                            state::visit_site(id.trim().to_string());
                                        }
                                    }
                                },
                            }
                        }
                        p { class: "text-xs text-text-muted-light",
                            "Enter the 10-character site code to browse it."
                        }
                        div { class: "flex gap-3 pt-2",
                            button {
                                class: "btn-primary flex-1 px-4 py-3 text-sm",
                                onclick: move |_| {
                                    let id = contract_id.read().clone();
                                    if !id.trim().is_empty() {
                                        state::visit_site(id.trim().to_string());
                                    }
                                },
                                "Visit"
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
                if active_tab == "import" {
                    // Import key form
                    div { class: "space-y-4",
                        div {
                            label { class: "block text-xs font-medium text-text-muted-light mb-1.5 uppercase tracking-wide",
                                "Site Key Token"
                            }
                            textarea {
                                class: "w-full h-32 px-4 py-3 bg-panel-warm border border-border-light rounded-lg text-text outline-none focus:border-accent text-xs font-mono",
                                value: "{import_token}",
                                placeholder: "-----BEGIN DELTA SITE KEY-----\n...\n-----END DELTA SITE KEY-----",
                                oninput: move |evt| {
                                    import_token.set(evt.value().to_string());
                                    import_error.set(None);
                                },
                            }
                        }
                        if let Some(err) = &*import_error.read() {
                            p { class: "text-xs text-red-400", "{err}" }
                        }
                        p { class: "text-xs text-text-muted-light",
                            "Paste a site key token to become the owner of that site on this device."
                        }
                        div { class: "flex gap-3 pt-2",
                            button {
                                class: "btn-primary flex-1 px-4 py-3 text-sm",
                                onclick: move |_| {
                                    let token = import_token.read().clone();
                                    match state::import_site_key(token) {
                                        Ok(_) => {}
                                        Err(e) => import_error.set(Some(e)),
                                    }
                                },
                                "Import"
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
}
