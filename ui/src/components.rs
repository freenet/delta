mod add_site_dialog;
mod editor;
pub(crate) mod export_key;
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
        // Hard stop on the "looking for your sites" state, so a node that never
        // answers cannot leave it up forever (#52).
        freenet_api::delegate::arm_discovery_fallback();
        state::init_from_hash();
    });

    let show_add_site = *state::SHOW_ADD_SITE.read();
    let has_sites = !state::SITES.read().is_empty();
    let has_current = state::CURRENT_SITE.read().is_some();
    let mobile_nav_open = *state::MOBILE_NAV_OPEN.read();

    // The pages sidebar only exists once a site is selected and we're not in
    // the add-site / welcome states.
    let show_pages_sidebar = !show_add_site && has_sites && has_current;

    // Drawer slides in from the left on small screens and is a static column
    // from `md:` up. `absolute` (not `fixed`) satisfies the gateway iframe
    // constraint documented in AGENTS.md.
    let drawer_class = format!(
        "absolute inset-y-0 left-0 z-40 flex shadow-2xl transition-transform duration-200 ease-out \
         md:static md:z-auto md:shadow-none md:translate-x-0 {}",
        if mobile_nav_open { "translate-x-0" } else { "-translate-x-full" }
    );

    rsx! {
        document::Link { rel: "icon", href: asset!("/assets/favicon.svg") }
        export_key::ExportKeyModal {}
        div { class: "relative flex h-screen bg-bg text-text overflow-hidden",
            // Backdrop dims the content behind the open drawer (mobile only).
            if mobile_nav_open {
                div {
                    class: "absolute inset-0 z-30 bg-black/40 md:hidden",
                    onclick: move |_| *state::MOBILE_NAV_OPEN.write() = false,
                }
            }
            // Sidebar drawer: site list + (optional) page list.
            div { class: "{drawer_class}",
                sites_sidebar::SitesSidebar {}
                if show_pages_sidebar {
                    pages_sidebar::PagesSidebar {}
                }
            }
            // Main column: mobile header + content.
            div { class: "flex flex-1 flex-col min-w-0",
                MobileHeader {}
                if show_add_site {
                    main { class: "flex-1 overflow-y-auto bg-panel",
                        add_site_dialog::AddSiteDialog {}
                    }
                } else if !has_sites || !has_current {
                    // Nothing to show yet. Which of the two very different
                    // reasons that is — "you have no sites" vs "we are still
                    // recovering them from a previous version" — is decided by
                    // `empty_pane_copy`. See freenet/delta#52.
                    {
                        // Only claim to be searching when there is genuinely
                        // nothing to show. With sites present but none
                        // selected this pane also renders, and "Looking for
                        // your sites" beside a populated sidebar is nonsense.
                        let searching =
                            empty_pane_is_searching(has_sites, *state::SITE_DISCOVERY.read());
                        let (heading, body) = empty_pane_copy(searching);
                        rsx! {
                            main { class: "flex-1 overflow-y-auto bg-panel",
                                div { class: "flex items-center justify-center h-full",
                                    div { class: "text-center max-w-md mx-8",
                                        span { class: "delta-mark inline-flex mb-6 w-16 h-16 text-[32px] rounded-2xl", "\u{0394}" }
                                        div { class: "flex items-center justify-center gap-3 mb-2",
                                            if searching {
                                                div { class: "w-4 h-4 rounded-full border-2 border-text-muted-light border-t-transparent animate-spin" }
                                            }
                                            h1 { class: "text-2xl font-semibold text-text", "{heading}" }
                                        }
                                        p { class: "text-sm text-text-muted-light mb-8 leading-relaxed", "{body}" }
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
                        }
                    }
                } else {
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
}

/// Whether the empty main pane should say Delta is still looking.
///
/// Extracted from `App` so the POLARITY is unit-testable. Inline, the
/// condition was only reachable through RSX that no host test can execute, so
/// flipping `!=` to `==` inverted #52 exactly — bare "Welcome to Delta" during
/// recovery, spinner after it finished — while every test stayed green.
///
/// Two rules, both load-bearing:
/// - Never claim to be searching once discovery has `Settled`; that is the
///   whole point of settling.
/// - Never claim to be searching when sites are already listed. This pane also
///   renders with sites present but none selected, and "Looking for your
///   sites" beside a populated sidebar is nonsense.
pub(crate) fn empty_pane_is_searching(has_sites: bool, discovery: state::SiteDiscovery) -> bool {
    !has_sites && discovery != state::SiteDiscovery::Settled
}

/// Heading and body copy for the empty main pane.
///
/// Delta re-keys its delegate on essentially every release, so after an upgrade
/// a returning user's site list has to be recovered from a legacy delegate. If
/// the bare "Welcome to Delta" screen is rendered during that window it is the
/// exact screen a brand-new user sees, so it reads as total data loss — which
/// once led a tester to file a false "migration is broken" report, and gives a
/// real user no reason to wait rather than assume everything is gone
/// (freenet/delta#52).
///
/// The searching variant deliberately keeps the "Get Started" button: a genuine
/// first-time user must never be blocked behind a spinner just because we
/// cannot yet prove they are one.
///
/// Split out of the component so the rule "never show the new-user copy while
/// discovery is outstanding" is unit-testable.
pub(crate) fn empty_pane_copy(searching: bool) -> (&'static str, &'static str) {
    if searching {
        (
            "Looking for your sites",
            "Checking this node for sites saved by an earlier version of Delta. \
             If you have used Delta before, your sites will appear here shortly — \
             nothing has been lost.",
        )
    } else {
        (
            "Welcome to Delta",
            "Decentralized publishing on Freenet. Create your own site or visit one using a site code.",
        )
    }
}

/// Top bar shown only on small screens (`md:hidden`). Holds the hamburger
/// button that toggles the sidebar drawer, plus the current context label so
/// the user knows where they are once the drawer closes.
#[component]
fn MobileHeader() -> Element {
    // Derive a short title: current page title if any, else the site name,
    // else the app name.
    let title = {
        let current = state::CURRENT_SITE.read().clone();
        let page = *state::CURRENT_PAGE.read();
        current
            .as_ref()
            .and_then(|prefix| {
                let sites = state::SITES.read();
                sites.get(prefix).map(|site| {
                    page.and_then(|pid| site.state.pages.get(&pid).map(|p| p.title.clone()))
                        .unwrap_or_else(|| site.name.clone())
                })
            })
            .unwrap_or_else(|| "Delta".to_string())
    };

    rsx! {
        div { class: "md:hidden flex items-center gap-3 px-4 py-3 border-b border-border bg-bg flex-shrink-0",
            button {
                class: "flex items-center justify-center w-9 h-9 -ml-1 rounded-lg text-text hover:bg-surface-hover transition-colors flex-shrink-0",
                aria_label: "Toggle navigation",
                onclick: move |_| {
                    let open = *state::MOBILE_NAV_OPEN.read();
                    *state::MOBILE_NAV_OPEN.write() = !open;
                },
                svg {
                    class: "w-5 h-5",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M4 6h16M4 12h16M4 18h16" }
                }
            }
            span { class: "delta-mark flex-shrink-0", "\u{0394}" }
            span { class: "text-sm font-semibold text-text-light truncate", "{title}" }
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

#[cfg(test)]
mod tests {
    use super::empty_pane_copy;

    #[test]
    fn the_new_user_welcome_is_never_shown_while_discovery_is_outstanding() {
        // freenet/delta#52. During recovery the screen must not be the one a
        // brand-new user sees, because that reads as total data loss.
        let (settled_heading, settled_body) = empty_pane_copy(false);
        let (searching_heading, searching_body) = empty_pane_copy(true);

        assert_ne!(
            searching_heading, settled_heading,
            "the searching state must not reuse the new-user heading"
        );
        assert_ne!(searching_body, settled_body);
        assert_eq!(settled_heading, "Welcome to Delta");
        assert!(
            !searching_heading.contains("Welcome"),
            "a returning user mid-recovery must not be welcomed as a newcomer"
        );
    }

    #[test]
    fn the_empty_pane_actually_consults_discovery_state() {
        // The copy tests above only prove the two variants differ. The rule
        // that matters is that `App` picks between them by READING
        // SITE_DISCOVERY into the value it branches on.
        //
        // Searching the whole file for the two needles independently is not
        // enough, and was the earlier form of this test. It passes under:
        //
        //     let _unused = *state::SITE_DISCOVERY.read();
        //     let searching = false;
        //     let (heading, body) = empty_pane_copy(searching);
        //
        // which reintroduces #52 in full — the recovery state never renders
        // and every returning user sees "Welcome to Delta" again. So isolate
        // the assignment that computes `searching` and require the read to
        // occur INSIDE it, i.e. to actually flow into the value.
        //
        // Needles are assembled at runtime so this test's own text cannot
        // satisfy them.
        // Whitespace is stripped before matching, per house convention: the
        // needles otherwise break on a rustfmt rewrap of unrelated code.
        let src: String = include_str!("components.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();

        let marker = format!("{}{}", "letsearch", "ing=");
        let start = src
            .find(&marker)
            .expect("`App` must compute a `searching` flag for the empty pane");
        let rhs = &src[start + marker.len()..];
        let rhs = &rhs[..rhs
            .find(';')
            .expect("the `searching` assignment must be `;`-terminated")];

        // STARTS WITH, not contains. `contains` admits a leading `!`, which
        // inverts the entire fix while leaving the scrape green — the same
        // hole this test was written to close, one level up.
        let predicate = format!("{}{}", "empty_pane_is_", "searching(");
        assert!(
            rhs.starts_with(&predicate),
            "`searching` must be exactly the unit-tested predicate, with no \
             wrapping or negation — a leading `!` inverts the fix and the \
             truth table cannot see it; found: {rhs:?}"
        );

        let needle = format!("{}{}", "state::SITE_", "DISCOVERY.read()");
        assert!(
            rhs.contains(&needle),
            "the discovery state must flow INTO the predicate, not merely be \
             read alongside it; found: {rhs:?}"
        );

        let call = format!("{}{}", "empty_pane_", "copy(searching)");
        assert!(
            src.contains(&call),
            "the computed flag must actually drive the copy"
        );
    }

    /// The full truth table for the searching predicate, including the
    /// polarity a source scrape cannot see. Flipping `!=` to `==` in
    /// `empty_pane_is_searching` inverts #52 — the bare newcomer welcome shows
    /// DURING recovery and the spinner shows after it finished — and this is
    /// the test that catches it.
    #[test]
    fn the_searching_predicate_has_the_right_polarity() {
        use super::empty_pane_is_searching;
        use crate::state::SiteDiscovery::{Pending, Settled};

        // The #52 case: nothing to show yet and discovery still running.
        assert!(
            empty_pane_is_searching(false, Pending),
            "no sites and discovery outstanding is precisely when we must say \
             we are still looking"
        );

        // Discovery finished and found nothing: the newcomer welcome is now
        // honest.
        assert!(
            !empty_pane_is_searching(false, Settled),
            "once discovery has settled, an empty list honestly means no sites"
        );

        // Sites present: this pane renders when none is selected, and claiming
        // to search beside a populated sidebar is nonsense.
        assert!(!empty_pane_is_searching(true, Pending));
        assert!(!empty_pane_is_searching(true, Settled));
    }

    /// The single most safety-critical line in this change, and otherwise
    /// uncovered: without the `rerun-if-changed` directive, Cargo reuses cached
    /// build-script output and the bundle ships a STALE migration table, so the
    /// startup sweep never asks the delegate holding a returning user's data.
    ///
    /// This is a source scrape and is labelled as one. It catches deletion of
    /// the directive; it cannot detect a bundle that already shipped stale.
    /// A stronger consistency check — comparing the baked-in `LEGACY_DELEGATES`
    /// against the file on disk — belongs with the wider build-input work and
    /// may supersede this. Until then this PR does not leave its own core fix
    /// unguarded on `main`.
    #[test]
    fn the_migration_table_is_declared_as_a_build_input() {
        let src = include_str!("../build.rs");
        let needle = format!(
            "{}{}",
            "cargo:rerun-if-changed=../legacy_", "delegates.toml"
        );
        assert!(
            src.contains(&needle),
            "ui/build.rs bakes legacy_delegates.toml into the bundle but does \
             not declare it as a build input, so an edited migration registry \
             will not invalidate the cached build script and the bundle will \
             ship a stale table"
        );
    }

    /// Without this call the 90 s hard stop never arms, so a node whose
    /// delegate never answers leaves the user on "Looking for your sites"
    /// forever. The call is in `App`'s mount effect, which no host test can
    /// run, so deleting it compiles clean and leaves every other test green.
    /// Needle assembled at runtime.
    #[test]
    fn the_discovery_hard_fallback_is_armed_at_startup() {
        let src = include_str!("components.rs");
        let needle = format!("{}{}", "arm_discovery_", "fallback()");
        assert!(
            src.contains(&needle),
            "App must arm the discovery hard fallback at startup, or a node \
             that never answers pins the user on the searching state forever"
        );
    }

    #[test]
    fn the_searching_copy_says_data_is_not_lost() {
        // The specific harm in #52 is the data-loss impression, so the copy has
        // to contradict it explicitly rather than just being vaguely busy.
        let (heading, body) = empty_pane_copy(true);
        let text = format!("{heading} {body}").to_lowercase();
        assert!(
            text.contains("earlier version") || text.contains("previous version"),
            "must explain that sites are being recovered from an earlier version: {text}"
        );
        assert!(
            text.contains("nothing has been lost"),
            "must state outright that nothing is lost: {text}"
        );
    }
}
