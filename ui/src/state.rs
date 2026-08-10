use delta_core::{Page, PageId, SiteConfig, SiteParameters, SiteState};
use dioxus::prelude::*;
use ed25519_dalek::{Signature, SigningKey};
use freenet_stdlib::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Known site entry
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownSite {
    pub name: String,
    pub prefix: String,
    pub role: SiteRole,
    pub state: SiteState,
    pub owner_pubkey: [u8; 32],
    #[serde(skip)]
    pub contract_key: Option<ContractKey>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SiteRole {
    Owner,
    Visitor,
}

// ---------------------------------------------------------------------------
// Global signals
// ---------------------------------------------------------------------------

pub static SITES: GlobalSignal<BTreeMap<String, KnownSite>> = GlobalSignal::new(BTreeMap::new);
pub static CURRENT_SITE: GlobalSignal<Option<String>> = GlobalSignal::new(|| None);
pub static CURRENT_PAGE: GlobalSignal<Option<PageId>> = GlobalSignal::new(|| None);
pub static EDITING: GlobalSignal<bool> = GlobalSignal::new(|| false);
pub static SHOW_ADD_SITE: GlobalSignal<bool> = GlobalSignal::new(|| false);
/// Whether the sidebar drawer is open on small (mobile) screens.
/// Ignored on `md:` and larger, where the sidebars are always visible.
pub static MOBILE_NAV_OPEN: GlobalSignal<bool> = GlobalSignal::new(|| false);
pub static EDITOR_TITLE: GlobalSignal<String> = GlobalSignal::new(String::new);
pub static EDITOR_CONTENT: GlobalSignal<String> = GlobalSignal::new(String::new);

/// Prefixes that the user has explicitly removed this session.
/// Prevents network responses from re-adding them.
pub static REMOVED_PREFIXES: GlobalSignal<Vec<String>> = GlobalSignal::new(Vec::new);

/// How far the startup search for the user's sites has got.
///
/// Delta re-keys its delegate on essentially every release, so a returning
/// user's first load after an upgrade finds the current delegate empty and has
/// to recover the site list from a legacy delegate. While that is outstanding
/// an empty `SITES` means "we have not found them yet", not "you have none" —
/// and rendering the bare "Welcome to Delta" screen (the same one a brand-new
/// user sees) is indistinguishable from total data loss. See freenet/delta#52.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteDiscovery {
    /// Still looking. An empty site list must be presented as "searching".
    Pending,
    /// Finished. An empty site list now honestly means the user has no sites.
    Settled,
}

pub static SITE_DISCOVERY: GlobalSignal<SiteDiscovery> =
    GlobalSignal::new(|| SiteDiscovery::Pending);

/// Mark startup discovery finished. Idempotent.
///
/// Settling is not permanent: `reopen_site_discovery` puts it back to `Pending`
/// when a genuinely new search starts. Timers must therefore never assume the
/// state they settle is the one they were armed for — they are deadlines on a
/// search, not on the page.
///
/// Only ever called from wasm paths (delegate discovery / gateway detection),
/// so the native build sees it as dead — same reason `PENDING_HASH` above is
/// annotated.
#[allow(dead_code)]
pub fn settle_site_discovery() {
    if *SITE_DISCOVERY.read() != SiteDiscovery::Settled {
        *SITE_DISCOVERY.write() = SiteDiscovery::Settled;
    }
}

/// Reopen discovery because a NEW search is starting — specifically, a
/// reconnect that re-runs the legacy sweep.
///
/// Settling was originally one-way, on the reasoning that a later reconnect
/// should not put "looking for your sites" back after the user has been shown
/// the real state. That is wrong when the reconnect genuinely restarts the
/// search: a socket flap after the grace period leaves the user staring at the
/// bare "Welcome to Delta" for the entire second sweep — the exact screen
/// freenet/delta#52 is about, in the exact scenario the reconnect path exists
/// to handle. Reopening is only correct because it is paired with re-arming the
/// settle timers; reopening without them would strand the spinner.
#[allow(dead_code)]
pub fn reopen_site_discovery() {
    if *SITE_DISCOVERY.read() != SiteDiscovery::Pending {
        *SITE_DISCOVERY.write() = SiteDiscovery::Pending;
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize from URL hash if present (e.g. user arrived via a shared link).
/// Pending page to select after site loads from network.
pub static PENDING_PAGE_ID: GlobalSignal<Option<PageId>> = GlobalSignal::new(|| None);

/// Pending hash route to process after WebSocket connects.
#[allow(dead_code)]
pub static PENDING_HASH: GlobalSignal<Option<String>> = GlobalSignal::new(|| None);

/// Read hash from the iframe URL and queue it for navigation after
/// the WebSocket connects. Does NOT try to visit immediately since
/// the WebSocket isn't ready yet during init.
pub fn init_from_hash() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let hash = window.location().hash().unwrap_or_default();
            if parse_hash_route(&hash).is_some() {
                web_sys::console::log_1(
                    &format!("Delta: queuing hash from iframe src: {hash}").into(),
                );
                *PENDING_HASH.write() = Some(hash);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Route parsing / updating
// ---------------------------------------------------------------------------

/// Parse a URL hash into a `(site_prefix, optional_page_id)` route.
///
/// Returns `None` for hashes that don't look like a Delta route — in
/// particular, in-page anchor links generated by the heading-id
/// injector (`#some-heading`) and any other hash whose first segment
/// isn't a valid 10-character base58 site prefix. Without this guard
/// the hashchange listener would feed `parse_hash_route("#heading")`
/// to `visit_site("heading")` on a page reload, which placeholder-
/// inserts a phantom site, fires a doomed contract GET, and leaves
/// the user staring at "Loading…". (#10, Ivvor 2026-05-03)
#[allow(dead_code)]
pub fn parse_hash_route(hash: &str) -> Option<(String, Option<PageId>)> {
    let hash = hash.trim_start_matches('#').trim_start_matches('/');
    if hash.is_empty() {
        return None;
    }
    let parts: Vec<&str> = hash.splitn(3, '/').collect();
    let prefix = parts[0];
    if !is_site_prefix_shape(prefix) {
        return None;
    }
    let page_id = parts.get(1).and_then(|s| s.parse::<PageId>().ok());
    Some((prefix.to_string(), page_id))
}

/// Whether `s` looks like a Delta site prefix: exactly 10 base58
/// characters. Real prefixes are produced by
/// `delta_core::pubkey_to_prefix` and always satisfy this.
fn is_site_prefix_shape(s: &str) -> bool {
    s.len() == 10 && s.chars().all(is_base58_char)
}

fn is_base58_char(c: char) -> bool {
    // Bitcoin-style base58: drops 0, O, I, l from base62 to avoid
    // visual confusion. Same alphabet `delta_core::pubkey_to_prefix`
    // produces.
    matches!(
        c,
        '1'..='9' | 'A'..='H' | 'J'..='N' | 'P'..='Z' | 'a'..='k' | 'm'..='z'
    )
}

pub fn build_hash_route(prefix: &str, page_id: Option<PageId>, title: Option<&str>) -> String {
    match (page_id, title) {
        (Some(id), Some(t)) => format!("#{}/{}/{}", prefix, id, slugify(t)),
        (Some(id), None) => format!("#{}/{}", prefix, id),
        _ => format!("#{}", prefix),
    }
}

// ---------------------------------------------------------------------------
// Site operations
// ---------------------------------------------------------------------------

pub fn select_site(prefix: &str) {
    *EDITING.write() = false;
    *SHOW_ADD_SITE.write() = false;
    *CURRENT_SITE.write() = Some(prefix.to_string());
    // Keep the drawer open on mobile: picking a site reveals its page list,
    // which lives in the same drawer. Picking a page is what closes it.

    let sites = SITES.read();
    if let Some(site) = sites.get(prefix) {
        // Check if there's a pending page from hash route
        let pending = *PENDING_PAGE_ID.read();
        let target_page = if let Some(pid) = pending {
            if site.state.pages.contains_key(&pid) {
                // Found the pending page — consume it
                *PENDING_PAGE_ID.write() = None;
                Some(pid)
            } else if site.state.pages.is_empty() {
                // Site not loaded yet (placeholder) — keep pending for later
                None
            } else {
                // Site loaded but page doesn't exist — consume and fall back
                *PENDING_PAGE_ID.write() = None;
                site.state.pages.keys().next().copied()
            }
        } else {
            site.state.pages.keys().next().copied()
        };

        *CURRENT_PAGE.write() = target_page;
        if let Some(page_id) = target_page {
            let page_title = site.state.pages.get(&page_id).map(|p| p.title.as_str());
            update_hash(&build_hash_route(prefix, Some(page_id), page_title));
            update_document_title(Some(&site.name), page_title);
        } else {
            update_hash(&build_hash_route(prefix, None, None));
            update_document_title(Some(&site.name), None);
        }
    }
}

pub fn show_add_site_prompt() {
    *SHOW_ADD_SITE.write() = true;
    *MOBILE_NAV_OPEN.write() = false;
}

/// Rename a site. Updates local state, signs new config via delegate,
/// and UPDATEs the contract on the network.
pub fn rename_site(prefix: &str, new_name: String) {
    let contract_key = {
        let mut sites = SITES.write();
        if let Some(site) = sites.get_mut(prefix) {
            site.name = new_name.clone();
            site.state.config.config.name = new_name.clone();
            site.state.config.config.version += 1;
            site.contract_key
        } else {
            None
        }
    };
    crate::freenet_api::delegate::save_known_sites();

    // Sign the new config and UPDATE the contract
    if let Some(ck) = contract_key {
        crate::freenet_api::delegate::request_sign_config(prefix, ck, new_name);
    }
}

/// Clear a tombstone for a previously-removed site. Called by any code path
/// that represents explicit user intent to (re-)add a site with this prefix:
/// `create_new_site`, `import_site_key`, `visit_site`. Without this, a
/// persisted tombstone would silently filter the new site out of
/// `restore_known_sites` and it would appear to "not work".
///
/// Note: the caller is responsible for triggering `save_known_sites()` after
/// inserting the site; that save also re-persists the (shrunk)
/// REMOVED_PREFIXES list.
pub fn clear_tombstone(prefix: &str) {
    REMOVED_PREFIXES.with_mut(|removed| {
        removed.retain(|p| p != prefix);
    });
}

/// Remove a site from the sidebar.
pub fn remove_site(prefix: &str) {
    REMOVED_PREFIXES.with_mut(|removed| {
        if !removed.contains(&prefix.to_string()) {
            removed.push(prefix.to_string());
        }
    });
    SITES.with_mut(|sites| {
        sites.remove(prefix);
    });
    crate::freenet_api::delegate::save_known_sites();
    // If we removed the currently selected site, select another
    if CURRENT_SITE.read().as_deref() == Some(prefix) {
        let next = SITES.read().keys().next().cloned();
        if let Some(next_prefix) = next {
            select_site(&next_prefix);
        } else {
            *CURRENT_SITE.write() = None;
            *CURRENT_PAGE.write() = None;
        }
    }
}

/// Create a new owned site. Signs initial state locally (key is in memory
/// momentarily), PUTs to network, then stores key in delegate for future
/// signing. The key is NOT kept in browser memory after this.
pub fn create_new_site(name: String) {
    let signing_key = SigningKey::generate(&mut rand::thread_rng());
    let verifying_key = signing_key.verifying_key();

    let params = SiteParameters::from_owner(&verifying_key);
    let prefix = params.prefix.clone();

    let config = SiteConfig {
        version: 1,
        name: name.clone(),
        description: String::new(),
    };
    let mut site_state = SiteState::new(config, &signing_key);
    let now = now_secs();
    let home_page = Page::new(
        1,
        "Home".into(),
        format!("# {name}\n\nWelcome to your new site.\n"),
        now,
        &signing_key,
    );
    site_state
        .upsert_page(1, home_page, &verifying_key)
        .expect("valid signed page");

    // Store signing key in delegate for persistence (per-prefix)
    let sk_bytes = signing_key.to_bytes();
    crate::freenet_api::delegate::store_signing_key(&sk_bytes, Some(&prefix));

    clear_tombstone(&prefix);
    let site = KnownSite {
        name: name.clone(),
        prefix: prefix.clone(),
        role: SiteRole::Owner,
        state: site_state.clone(),
        owner_pubkey: verifying_key.to_bytes(),
        contract_key: Some(contract_key_from_prefix(&prefix)),
    };
    SITES.with_mut(|sites| {
        sites.insert(prefix.clone(), site);
    });
    crate::freenet_api::delegate::save_known_sites();

    // PUT to Freenet network (if connected)
    crate::freenet_api::put_site(&params, &site_state);

    *SHOW_ADD_SITE.write() = false;

    // Defer site selection so Dioxus can re-render with the new site first
    #[cfg(target_arch = "wasm32")]
    {
        let prefix_clone = prefix.clone();
        wasm_bindgen_futures::spawn_local(async move {
            select_site(&prefix_clone);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        select_site(&prefix);
    }
}

/// Import a site key from armored token. Makes this device the owner.
pub fn import_site_key(token: String) -> Result<(), String> {
    let export = delta_core::SiteKeyExport::from_armored(&token)?;

    if export.signing_key.len() != 32 {
        return Err("Invalid signing key length".into());
    }
    if export.owner_pubkey.len() != 32 {
        return Err("Invalid public key length".into());
    }

    let prefix = export.prefix.clone();
    let name = export.name.clone();

    // Store signing key in delegate (per-prefix)
    let mut sk_bytes = [0u8; 32];
    sk_bytes.copy_from_slice(&export.signing_key);
    crate::freenet_api::delegate::store_signing_key(&sk_bytes, Some(&prefix));

    // Compute contract key and add as owned site
    let contract_key = contract_key_from_prefix(&prefix);

    let mut owner_bytes = [0u8; 32];
    owner_bytes.copy_from_slice(&export.owner_pubkey);

    clear_tombstone(&prefix);
    let site = KnownSite {
        name,
        prefix: prefix.clone(),
        role: SiteRole::Owner,
        state: SiteState::default(),
        owner_pubkey: owner_bytes,
        contract_key: Some(contract_key),
    };
    SITES.with_mut(|sites| {
        sites.insert(prefix.clone(), site);
    });
    crate::freenet_api::delegate::save_known_sites();

    // GET the site content from network
    crate::freenet_api::get_site(&contract_key);

    *SHOW_ADD_SITE.write() = false;

    #[cfg(target_arch = "wasm32")]
    {
        let prefix_clone = prefix.clone();
        wasm_bindgen_futures::spawn_local(async move {
            select_site(&prefix_clone);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        select_site(&prefix);
    }

    Ok(())
}

/// Site contract WASM (for computing contract keys from prefixes).
const SITE_CONTRACT_WASM: &[u8] = include_bytes!("../public/contracts/site_contract.wasm");

/// Compute a contract key from a site prefix.
/// Anyone can do this — the WASM is public and the prefix is the only parameter.
pub fn contract_key_from_prefix(prefix: &str) -> ContractKey {
    let params = SiteParameters {
        prefix: prefix.to_string(),
    };
    let mut params_buf = Vec::new();
    ciborium::ser::into_writer(&params, &mut params_buf).expect("CBOR params");
    let contract_code = ContractCode::from(SITE_CONTRACT_WASM);
    ContractKey::from_params_and_code(Parameters::from(params_buf), &contract_code)
}

/// Visit an existing site by its 10-char prefix. Computes the contract key,
/// sends GET + SUBSCRIBE.
///
/// Refuses inputs that don't look like a real Delta site prefix
/// (10 base58 chars). Without this guard, the dialog and hash-replay
/// paths would happily insert a phantom site and fire a doomed
/// contract GET — the exact symptom #10 fixes for the URL parser.
pub fn visit_site(input: String) {
    let prefix = input.trim().to_string();
    if !is_site_prefix_shape(&prefix) {
        return;
    }

    // If already known, just select it
    if SITES.read().contains_key(&prefix) {
        *SHOW_ADD_SITE.write() = false;
        select_site(&prefix);
        return;
    }

    clear_tombstone(&prefix);
    let contract_key = contract_key_from_prefix(&prefix);

    let placeholder = KnownSite {
        name: "Loading...".to_string(),
        prefix: prefix.clone(),
        role: SiteRole::Visitor,
        state: SiteState::default(),
        owner_pubkey: [0u8; 32],
        contract_key: Some(contract_key),
    };
    SITES.with_mut(|sites| {
        sites.insert(prefix.clone(), placeholder);
    });
    crate::freenet_api::delegate::save_known_sites();

    // GET the site — SUBSCRIBE happens after GET succeeds
    crate::freenet_api::get_site(&contract_key);

    *SHOW_ADD_SITE.write() = false;

    #[cfg(target_arch = "wasm32")]
    {
        let prefix_clone = prefix.clone();
        wasm_bindgen_futures::spawn_local(async move {
            select_site(&prefix_clone);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        select_site(&prefix);
    }
}

// ---------------------------------------------------------------------------
// Page operations
// ---------------------------------------------------------------------------

pub fn current_site() -> Option<KnownSite> {
    let prefix = (*CURRENT_SITE.read()).clone()?;
    SITES.read().get(&prefix).cloned()
}

pub fn current_page() -> Option<(PageId, Page)> {
    let prefix = (*CURRENT_SITE.read()).clone()?;
    let page_id = (*CURRENT_PAGE.read())?;
    let sites = SITES.read();
    let site = sites.get(&prefix)?;
    site.state.pages.get(&page_id).map(|p| (page_id, p.clone()))
}

pub fn select_page(page_id: PageId) {
    *EDITING.write() = false;
    *CURRENT_PAGE.write() = Some(page_id);
    *MOBILE_NAV_OPEN.write() = false;

    if let Some(prefix) = &*CURRENT_SITE.read() {
        let sites = SITES.read();
        let site = sites.get(prefix);
        let page_title = site
            .and_then(|s| s.state.pages.get(&page_id))
            .map(|p| p.title.as_str());
        let site_name = site.map(|s| s.name.as_str());
        update_hash(&build_hash_route(prefix, Some(page_id), page_title));
        update_document_title(site_name, page_title);
    }
}

/// Create a new page. For owned sites with a contract key, sends to delegate
/// for signing. For example data, creates with placeholder signature.
pub fn create_page(title: String) {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };

    let sites = SITES.read();
    let Some(site) = sites.get(&prefix) else {
        return;
    };

    // Allocate an id strictly greater than every id ever used on this site —
    // live pages AND tombstoned (deleted) pages — not just `next_page_id`.
    // A tombstone-aware `delta-core::merge` can advance the live set without
    // advancing `next_page_id` (merge only bumps the counter when it INSERTS
    // a live page), so a stale `next_page_id` may point at/below a tombstoned
    // id. Reusing a tombstoned id makes `handle_signed_page` drop the new
    // page (`deleted_pages.contains_key`), silently losing it. See
    // `next_free_page_id`.
    let id = next_free_page_id(&site.state);
    let now = now_secs();
    let contract_key = site.contract_key;
    let is_owner = site.role == SiteRole::Owner;
    // Assign an order strictly greater than any existing page so the
    // new page sorts after the others. The previous default (0)
    // caused new pages to land at the front of the sidebar (sort key
    // is `(order, id)` and 0 dominates any explicit order), and on
    // refresh the page would be visually relocated to a position the
    // user did not pick. See issue #15.
    let next_order = next_create_order(site.state.pages.values().map(|p| p.order));
    drop(sites);

    if is_owner {
        if let Some(ck) = contract_key {
            // Send to delegate for signing — response handler will update state + network
            crate::freenet_api::delegate::request_sign_page(
                &prefix,
                ck,
                id,
                title.clone(),
                String::new(),
                now,
                next_order,
            );
            // Optimistically add to local state with placeholder sig
            let mut sites = SITES.write();
            if let Some(site) = sites.get_mut(&prefix) {
                let page = Page {
                    title,
                    content: String::new(),
                    updated_at: now,
                    signature: Signature::from_bytes(&[0u8; 64]),
                    order: next_order,
                };
                site.state.pages.insert(id, page);
                site.state.next_page_id = id + 1;
            }
        } else {
            // Example data / offline — unsigned placeholder
            let mut sites = SITES.write();
            if let Some(site) = sites.get_mut(&prefix) {
                let page = Page {
                    title,
                    content: String::new(),
                    updated_at: now,
                    signature: Signature::from_bytes(&[0u8; 64]),
                    order: next_order,
                };
                site.state.pages.insert(id, page);
                site.state.next_page_id = id + 1;
            }
        }
    }

    *CURRENT_PAGE.write() = Some(id);
    *EDITING.write() = true;
}

/// Save the current page edit. Routes through delegate for signing if connected.
pub fn save_current_page() {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };
    let Some(page_id) = *CURRENT_PAGE.read() else {
        return;
    };
    let title = EDITOR_TITLE.read().clone();
    let content = EDITOR_CONTENT.read().clone();
    let now = next_page_updated_at(&prefix, page_id);

    let sites = SITES.read();
    let contract_key = sites.get(&prefix).and_then(|s| s.contract_key);
    let is_owner = sites
        .get(&prefix)
        .map(|s| s.role == SiteRole::Owner)
        .unwrap_or(false);
    let order = sites
        .get(&prefix)
        .and_then(|s| s.state.pages.get(&page_id))
        .map(|p| p.order)
        .unwrap_or(0);
    drop(sites);

    if is_owner {
        if let Some(ck) = contract_key {
            // Send to delegate for signing
            crate::freenet_api::delegate::request_sign_page(
                &prefix,
                ck,
                page_id,
                title.clone(),
                content.clone(),
                now,
                order,
            );
        }
    }

    // Optimistically update local state
    let mut sites = SITES.write();
    if let Some(site) = sites.get_mut(&prefix) {
        if let Some(page) = site.state.pages.get_mut(&page_id) {
            page.title = title;
            page.content = content;
            page.updated_at = now;
        }
    }

    *EDITING.write() = false;
}

/// Rename a page. Updates locally and signs via delegate.
pub fn rename_page(page_id: PageId, new_title: String) {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };

    let sites = SITES.read();
    let site = match sites.get(&prefix) {
        Some(s) => s,
        None => return,
    };
    let contract_key = site.contract_key;
    let content = site
        .state
        .pages
        .get(&page_id)
        .map(|p| p.content.clone())
        .unwrap_or_default();
    drop(sites);

    let now = next_page_updated_at(&prefix, page_id);

    // Update local state optimistically
    SITES.with_mut(|sites| {
        if let Some(site) = sites.get_mut(&prefix) {
            if let Some(page) = site.state.pages.get_mut(&page_id) {
                page.title = new_title.clone();
                page.updated_at = now;
            }
        }
    });

    // Sign via delegate and UPDATE network
    if let Some(ck) = contract_key {
        let order = SITES
            .read()
            .get(&prefix)
            .and_then(|s| s.state.pages.get(&page_id))
            .map(|p| p.order)
            .unwrap_or(0);
        crate::freenet_api::delegate::request_sign_page(
            &prefix, ck, page_id, new_title, content, now, order,
        );
    }
}

/// Swap the order of two pages. Used for move up/down.
///
/// If any page on the site still has `order == 0` (legacy pages signed
/// before the order field existed, or new pages created before issue
/// #15 was fixed), this also migrates **every** page to an explicit
/// order and signs+propagates each one. The local-only fallback used
/// previously left the unmigrated pages at `order = 0` on the network
/// so they clumped to the front of the sidebar after a refresh, even
/// though the local view looked correct. See PR description for the
/// concrete scenario.
pub fn swap_page_order(page_a: PageId, page_b: PageId) {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };

    // Single read-pass: collect everything the orchestration needs in
    // one shot so we don't race with a network UPDATE landing between
    // independent `SITES.read()` calls. (Single-threaded UI in
    // practice, but cheaper to be obviously correct than to argue
    // about it in a future refactor.)
    let (current_orders, current_timestamps, contract_key) = {
        let sites = SITES.read();
        let Some(site) = sites.get(&prefix) else {
            return;
        };
        let orders: Vec<(PageId, u32)> = site
            .state
            .pages
            .iter()
            .map(|(id, p)| (*id, p.order))
            .collect();
        let timestamps: BTreeMap<PageId, u64> = site
            .state
            .pages
            .iter()
            .map(|(id, p)| (*id, p.updated_at))
            .collect();
        (orders, timestamps, site.contract_key)
    };
    let Some(contract_key) = contract_key else {
        return;
    };

    let plan = plan_swap(&current_orders, page_a, page_b);
    if plan.pages_to_sign.is_empty() {
        return; // No-op: same-order swap, missing pages, etc.
    }

    // Strict-monotonic timestamp per page, derived from the snapshot
    // taken above so all pages observe the same `now` and the same
    // existing values.
    let now = now_secs();
    let new_timestamps: BTreeMap<PageId, u64> = plan
        .pages_to_sign
        .iter()
        .map(|&pid| {
            let ts = monotonic_updated_at(now, current_timestamps.get(&pid).copied());
            (pid, ts)
        })
        .collect();

    // Apply the new orders and timestamps in a single mutation so
    // local state never advertises the new orders alongside the
    // previous timestamps. The signature on disk is still the old one
    // until the delegate echoes back a fresh signed page; that is the
    // pre-existing optimistic-update pattern (see also `create_page`,
    // which inserts pages with a zeroed signature placeholder).
    SITES.with_mut(|sites| {
        if let Some(site) = sites.get_mut(&prefix) {
            for (&pid, &order) in &plan.new_orders {
                if let Some(p) = site.state.pages.get_mut(&pid) {
                    p.order = order;
                }
            }
            for (&pid, &ts) in &new_timestamps {
                if let Some(p) = site.state.pages.get_mut(&pid) {
                    p.updated_at = ts;
                }
            }
        }
    });

    // Order is part of the v2 signature, so re-sign every page whose
    // order changed. In the migration case that's all pages; in the
    // steady state it's just the two pages being swapped.
    let sites = SITES.read();
    let Some(site) = sites.get(&prefix) else {
        return;
    };
    for &pid in &plan.pages_to_sign {
        if let (Some(page), Some(&ts)) = (site.state.pages.get(&pid), new_timestamps.get(&pid)) {
            crate::freenet_api::delegate::request_sign_page(
                &prefix,
                contract_key,
                pid,
                page.title.clone(),
                page.content.clone(),
                ts,
                page.order,
            );
        }
    }
}

pub fn delete_page(page_id: PageId) {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };

    let sites = SITES.read();
    let contract_key = sites.get(&prefix).and_then(|s| s.contract_key);
    let is_owner = sites
        .get(&prefix)
        .map(|s| s.role == SiteRole::Owner)
        .unwrap_or(false);
    drop(sites);

    if is_owner {
        if let Some(ck) = contract_key {
            // Deletions don't need the strictly-monotonic-timestamp
            // dance: dominance for tombstones is via tombstone
            // *presence* in `deleted_pages`, not by comparing
            // `deleted_at` (`SiteState::delete_page` /
            // `SiteState::merge` in delta-core).
            crate::freenet_api::delegate::request_sign_deletion(&prefix, ck, page_id, now_secs());
        }
    }

    // Optimistically remove locally
    let mut sites = SITES.write();
    if let Some(site) = sites.get_mut(&prefix) {
        site.state.pages.remove(&page_id);
        if *CURRENT_PAGE.read() == Some(page_id) {
            let next = site.state.pages.keys().next().copied();
            drop(sites);
            *CURRENT_PAGE.write() = next;
        }
    }
}

pub fn start_editing() {
    if let Some((_, page)) = current_page() {
        *EDITOR_TITLE.write() = page.title.clone();
        *EDITOR_CONTENT.write() = page.content.clone();
        *EDITING.write() = true;
    }
}

#[allow(dead_code)]
pub fn navigate_to_page(page_id: PageId) {
    let sites = SITES.read();
    if let Some(prefix) = &*CURRENT_SITE.read() {
        if let Some(site) = sites.get(prefix) {
            if site.state.pages.contains_key(&page_id) {
                drop(sites);
                select_page(page_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

/// Compute a `updated_at` value for a page-update that is **strictly
/// greater** than the page's current `updated_at`.
///
/// `apply_delta` / `merge` in `delta-core` dominate equal timestamps
/// (`existing.updated_at >= incoming.updated_at` -> incoming dropped),
/// so two updates produced inside the same wall-clock second collide
/// and the second is silently rejected on the network even though the
/// UI optimistically applied it. Reorder is the most user-visible
/// surface for this (Ivvor, 2026-04-29: "Delta seems to have lost the
/// ability to reorder pages consistently") because each click swaps
/// two pages and consecutive clicks within a second produce three
/// updates with the same timestamp.
///
/// Forcing `max(now_secs(), existing + 1)` per page restores the
/// monotonicity the contract relies on. The drift is bounded by the
/// click rate: each click bumps the per-page timestamp by 1 second,
/// so a held arrow at a typical 30 Hz repeat rate adds ~30 s of
/// drift per second held. Drift gradually decays once the wall
/// clock catches up. Pathological abuse is filed for follow-up;
/// this PR addresses the user-reported same-second collision.
pub(crate) fn next_page_updated_at(prefix: &str, page_id: PageId) -> u64 {
    let existing = SITES
        .read()
        .get(prefix)
        .and_then(|s| s.state.pages.get(&page_id))
        .map(|p| p.updated_at);
    monotonic_updated_at(now_secs(), existing)
}

/// Pure helper for `next_page_updated_at` so the monotonicity rule
/// can be unit-tested without spinning up the Dioxus signal layer.
fn monotonic_updated_at(now: u64, existing: Option<u64>) -> u64 {
    match existing {
        Some(existing) => now.max(existing.saturating_add(1)),
        None => now,
    }
}

/// Step between page orders. Used by both `next_create_order` and the
/// migration in `compute_swap_orders` so newly-created pages and
/// migrated pages share the same grid (pages with explicit orders
/// `10, 20, 30, ...` and a fresh page slotted in at `40`).
const ORDER_STEP: u32 = 10;

/// The next free page id: strictly greater than `next_page_id` AND every
/// live and TOMBSTONED page id. A tombstone-aware merge can advance the live
/// or deleted set without advancing `next_page_id`, so `next_page_id` alone
/// is not a safe allocator — reusing a tombstoned id makes the delegate's
/// signed page get dropped by the `deleted_pages` guard, silently losing the
/// new page. Pure so the allocation rule is unit-testable.
fn next_free_page_id(state: &SiteState) -> PageId {
    let highest_used = state
        .pages
        .keys()
        .chain(state.deleted_pages.keys())
        .copied()
        .max();
    let used_ceiling = highest_used.map(|h| h.saturating_add(1)).unwrap_or(0);
    state.next_page_id.max(used_ceiling)
}

/// Pick an `order` value for a freshly-created page. The new page
/// should sort after every existing page so the user-visible position
/// stays stable across a refresh — issuing `0` (the previous default)
/// caused new pages to clump at the front of the sidebar because the
/// sort key is `(order, id)` and 0 dominates any explicit order.
fn next_create_order<I: IntoIterator<Item = u32>>(existing_orders: I) -> u32 {
    existing_orders
        .into_iter()
        .max()
        .map(|max| max.saturating_add(ORDER_STEP))
        .unwrap_or(ORDER_STEP)
}

/// What `swap_page_order` is going to do, computed without touching
/// the live signal state. Returned by `plan_swap` so the orchestration
/// rule (which pages need a fresh signature, which orders they end up
/// with) is unit-testable end-to-end, not just at the `compute_swap_orders`
/// step.
#[derive(Debug, PartialEq, Eq)]
struct SwapPlan {
    /// New `order` value for every page on the site, keyed by
    /// `PageId`. Pages whose order didn't change still appear here
    /// (with their existing value) so callers don't have to merge.
    new_orders: BTreeMap<PageId, u32>,
    /// Pages that need a fresh signed UPDATE on the network — i.e.
    /// whose order actually changed. Empty when the swap is a no-op
    /// (e.g. both pages had the same order, or the swap target IDs
    /// don't exist in `pages`).
    pages_to_sign: Vec<PageId>,
}

/// Plan a swap end-to-end: compute the new orders AND the set of
/// pages whose UPDATEs must be propagated to the network. Pure helper
/// so the user-visible "every page must be signed when migrating"
/// invariant is exercised by unit tests, not just at the
/// `compute_swap_orders` boundary.
fn plan_swap(pages: &[(PageId, u32)], page_a: PageId, page_b: PageId) -> SwapPlan {
    let new_orders = compute_swap_orders(pages, page_a, page_b);
    // Pages-to-sign = pages whose new order differs from current.
    // In the migration case that's every page on the site; in steady
    // state it's just the two pages being swapped. Deriving from the
    // diff means a regression that re-narrows the migration set
    // (e.g. someone reverts to "just the two clicked pages") is
    // automatically caught.
    let pages_to_sign: Vec<PageId> = pages
        .iter()
        .filter_map(|&(pid, current)| {
            new_orders
                .get(&pid)
                .copied()
                .filter(|&new| new != current)
                .map(|_| pid)
        })
        .collect();
    SwapPlan {
        new_orders,
        pages_to_sign,
    }
}

/// Pure helper for `swap_page_order`'s order-assignment step so the
/// migration + swap logic can be unit-tested without spinning up the
/// Dioxus signal layer.
///
/// Returns the new `order` value for every page on the site after a
/// swap of `page_a` and `page_b`. If any page is currently at
/// `order == 0` (legacy / pre-#15 state), the entire site is first
/// migrated to explicit orders by `(current_order, page_id)` sort key
/// so the migration preserves the user's visible page sequence; the
/// swap is then applied on top.
///
/// `pages` is the current `(page_id, order)` list. The output is keyed
/// by page_id so the caller can iterate it independently of input
/// order. If either swap target is not present in `pages` the swap
/// is skipped — the helper is otherwise a foot-gun that would clobber
/// the present page's order to 0 via `unwrap_or(0)`.
fn compute_swap_orders(
    pages: &[(PageId, u32)],
    page_a: PageId,
    page_b: PageId,
) -> BTreeMap<PageId, u32> {
    let needs_migration = pages.iter().any(|(_, o)| *o == 0);

    let mut next: BTreeMap<PageId, u32> = if needs_migration {
        // Sort by `(order, id)` — same key the sidebar uses — so the
        // migration matches what the user is currently looking at.
        let mut sorted: Vec<(u32, PageId)> = pages.iter().map(|&(id, o)| (o, id)).collect();
        sorted.sort();
        sorted
            .iter()
            .enumerate()
            .map(|(i, &(_, id))| (id, (i as u32 + 1) * ORDER_STEP))
            .collect()
    } else {
        pages.iter().copied().collect()
    };

    // Skip the swap if either target is missing. Without this guard
    // `unwrap_or(0)` clobbers the present page's order to 0,
    // re-introducing the very symptom this PR is fixing.
    if let (Some(order_a), Some(order_b)) = (next.get(&page_a).copied(), next.get(&page_b).copied())
    {
        if let Some(o) = next.get_mut(&page_a) {
            *o = order_b;
        }
        if let Some(o) = next.get_mut(&page_b) {
            *o = order_a;
        }
    }
    next
}

/// Update the browser tab title: "Page — Site — Delta"
fn update_document_title(site_name: Option<&str>, page_title: Option<&str>) {
    let title = match (page_title, site_name) {
        (Some(page), Some(site)) => format!("{page} — {site} — Delta"),
        (None, Some(site)) => format!("{site} — Delta"),
        _ => "Delta".to_string(),
    };
    crate::components::set_document_title(&title);
}

fn update_hash(hash: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        // Use history.replaceState to update the hash without triggering
        // navigation — set_hash causes "Unsafe attempt to load URL" errors
        // inside the gateway's sandboxed iframe.
        if let Some(window) = web_sys::window() {
            let _ = window.history().ok().and_then(|h| {
                h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(hash))
                    .ok()
            });
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = hash;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compute_swap_orders, is_site_prefix_shape, monotonic_updated_at, next_create_order,
        next_free_page_id, parse_hash_route, plan_swap,
    };
    use delta_core::PageId;
    use std::collections::BTreeMap;

    #[test]
    fn next_free_page_id_skips_tombstoned_ids() {
        // BLOCKER (review): a tombstone-aware `merge` can advance the deleted
        // set without advancing `next_page_id`, leaving `next_page_id`
        // pointing at/below a TOMBSTONED id. Allocating that id makes
        // `handle_signed_page` drop the new page (`deleted_pages` guard),
        // silently losing it. `create_page` must allocate above every id ever
        // used — live OR tombstoned.
        let owner = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
        let mut state = delta_core::SiteState::new(delta_core::SiteConfig::default(), &owner);
        let home = delta_core::Page::new(1, "Home".into(), "c".into(), 100, &owner);
        state.upsert_page(1, home, &owner.verifying_key()).unwrap();
        // High ids 2 and 3 were created then deleted (tombstoned)...
        state
            .deleted_pages
            .insert(2, delta_core::SignedPageDeletion::new(2, 200, &owner));
        state
            .deleted_pages
            .insert(3, delta_core::SignedPageDeletion::new(3, 200, &owner));
        // ...but a merge left the counter stale, pointing at a tombstoned id.
        state.next_page_id = 2;

        let id = next_free_page_id(&state);
        assert_eq!(
            id, 4,
            "must allocate strictly above the highest tombstoned id"
        );
        assert!(!state.pages.contains_key(&id));
        assert!(!state.deleted_pages.contains_key(&id));
        // Revert-check: the OLD allocator (`next_page_id`) returns a tombstoned
        // id, which would silently drop the new page.
        assert!(
            state.deleted_pages.contains_key(&state.next_page_id),
            "the stale next_page_id must be a tombstoned id for this test to be meaningful"
        );
    }

    #[test]
    fn next_free_page_id_normal_case_uses_counter() {
        // With a healthy counter and no tombstones, allocation is unchanged.
        let owner = ed25519_dalek::SigningKey::from_bytes(&[2u8; 32]);
        let mut state = delta_core::SiteState::new(delta_core::SiteConfig::default(), &owner);
        let home = delta_core::Page::new(1, "Home".into(), "c".into(), 100, &owner);
        state.upsert_page(1, home, &owner.verifying_key()).unwrap();
        // upsert_page advanced next_page_id to 2; no tombstones.
        assert_eq!(next_free_page_id(&state), 2);
    }

    #[test]
    fn parse_hash_route_accepts_real_site_prefix() {
        // Real prefix from the user's site (Ivvor's report URL).
        assert_eq!(
            parse_hash_route("#AmcVD92D3U"),
            Some(("AmcVD92D3U".to_string(), None))
        );
    }

    #[test]
    fn parse_hash_route_accepts_prefix_with_page_id() {
        assert_eq!(
            parse_hash_route("#AmcVD92D3U/2"),
            Some(("AmcVD92D3U".to_string(), Some(2)))
        );
    }

    #[test]
    fn parse_hash_route_accepts_prefix_with_page_id_and_slug() {
        assert_eq!(
            parse_hash_route("#AmcVD92D3U/2/some-page-title"),
            Some(("AmcVD92D3U".to_string(), Some(2)))
        );
    }

    #[test]
    fn parse_hash_route_rejects_in_page_anchor() {
        // The bug Ivvor reported: clicking `[link](#test-heading)`
        // would feed `parse_hash_route("#test-heading")` to
        // `visit_site("test-heading")` on a page reload, fire a
        // doomed contract GET, and leave the user staring at
        // "Loading…". Reject anything that isn't shaped like a real
        // 10-char base58 prefix.
        assert_eq!(parse_hash_route("#test-heading"), None);
        assert_eq!(parse_hash_route("#some-anchor"), None);
        assert_eq!(parse_hash_route("#a"), None);
    }

    #[test]
    fn parse_hash_route_rejects_empty_hash() {
        assert_eq!(parse_hash_route(""), None);
        assert_eq!(parse_hash_route("#"), None);
        assert_eq!(parse_hash_route("#/"), None);
    }

    #[test]
    fn parse_hash_route_rejects_wrong_length_prefix() {
        // Must be exactly 10 chars.
        assert_eq!(parse_hash_route("#AmcVD92D3"), None); // 9
        assert_eq!(parse_hash_route("#AmcVD92D3UX"), None); // 11
    }

    #[test]
    fn parse_hash_route_rejects_non_base58_chars_in_prefix() {
        // Bitcoin-style base58 drops `0`, `O`, `I`, `l`. A prefix
        // that contains any of those isn't a real site prefix.
        assert_eq!(parse_hash_route("#0AmcVD92D3"), None);
        assert_eq!(parse_hash_route("#OAmcVD92D3"), None);
        assert_eq!(parse_hash_route("#IAmcVD92D3"), None);
        assert_eq!(parse_hash_route("#lAmcVD92D3"), None);
        // Hyphen — common in heading slugs — is also rejected.
        assert_eq!(parse_hash_route("#test-headi"), None);
    }

    #[test]
    fn is_site_prefix_shape_pins_alphabet() {
        // Real prefix.
        assert!(is_site_prefix_shape("AmcVD92D3U"));
        // All-digits 1-9 (no 0).
        assert!(is_site_prefix_shape("123456789a"));
        // Mixed-case alphabetic.
        assert!(is_site_prefix_shape("AbcdefghJK"));
    }

    #[test]
    fn is_site_prefix_shape_rejects_excluded_chars() {
        // Each excluded base58 character makes the prefix invalid.
        for excluded in ['0', 'O', 'I', 'l'] {
            let s: String = std::iter::once(excluded)
                .chain("23456789a".chars())
                .collect();
            assert!(
                !is_site_prefix_shape(&s),
                "prefix containing '{excluded}' should be rejected: {s}"
            );
        }
    }

    #[test]
    fn is_site_prefix_shape_pins_alphabet_boundaries() {
        // Uppercase L is INCLUDED (`'J'..='N'`), lowercase l EXCLUDED.
        // Easy to break if someone "fixes" the alphabet ranges.
        assert!(is_site_prefix_shape("LLLLLLLLLL"));
        assert!(!is_site_prefix_shape("llllllllll"));
        // J is included, I is excluded.
        assert!(is_site_prefix_shape("JJJJJJJJJJ"));
        assert!(!is_site_prefix_shape("IIIIIIIIII"));
        // Lowercase i and j are both included.
        assert!(is_site_prefix_shape("iiiiiiiiii"));
        assert!(is_site_prefix_shape("jjjjjjjjjj"));
        // Lowercase k and m are both included; lowercase l is the gap.
        assert!(is_site_prefix_shape("kkkkkkkkkk"));
        assert!(is_site_prefix_shape("mmmmmmmmmm"));
    }

    #[test]
    fn returns_now_for_brand_new_page() {
        assert_eq!(monotonic_updated_at(100, None), 100);
    }

    #[test]
    fn returns_now_when_strictly_after_existing() {
        assert_eq!(monotonic_updated_at(200, Some(100)), 200);
    }

    #[test]
    fn forces_strictly_greater_when_now_equals_existing() {
        // The reorder bug: same wall-clock second produces equal
        // timestamps, which apply_delta/merge dominate. The helper
        // must bump past the existing value.
        assert_eq!(monotonic_updated_at(100, Some(100)), 101);
    }

    #[test]
    fn forces_strictly_greater_when_now_is_behind_existing() {
        // Possible after NTP correction or if a peer state with a
        // future-dated timestamp arrived. Stay strictly above it.
        assert_eq!(monotonic_updated_at(100, Some(150)), 151);
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        // existing == u64::MAX is unreachable in practice but the
        // helper must not panic.
        assert_eq!(monotonic_updated_at(0, Some(u64::MAX)), u64::MAX);
    }

    #[test]
    fn three_rapid_updates_within_one_second_get_strictly_increasing_ts() {
        // Simulates three reorder clicks at the same wall-clock
        // second. Each pass feeds the previous result back as
        // `existing` to mimic the loop where `next_page_updated_at`
        // reads the just-written page state. The chain must be
        // strictly increasing; otherwise the contract dominates the
        // second and third updates and the user-visible bug recurs.
        let now = 1_000;
        let t1 = monotonic_updated_at(now, Some(now));
        let t2 = monotonic_updated_at(now, Some(t1));
        let t3 = monotonic_updated_at(now, Some(t2));
        assert!(t1 > now);
        assert!(t2 > t1);
        assert!(t3 > t2);
        assert_eq!((t1, t2, t3), (now + 1, now + 2, now + 3));
    }

    #[test]
    fn next_create_order_on_empty_site_starts_at_step() {
        // First page on a brand-new site gets order=10 instead of 0
        // so when a SECOND page is created we don't fall into the
        // "all zero" all-pages-have-the-same-order bucket.
        assert_eq!(next_create_order(std::iter::empty()), 10);
    }

    #[test]
    fn next_create_order_with_zero_only_pages_still_picks_step() {
        // Legacy v1-signed pages all live at order=0. A new page on
        // such a site MUST sort after them, so it has to skip past 0.
        assert_eq!(next_create_order([0, 0, 0]), 10);
    }

    #[test]
    fn next_create_order_picks_strictly_greater_than_existing_max() {
        // Steady-state case: pages have orders 10, 20, 30. New page
        // gets 40 so it sorts at the end of the sidebar.
        assert_eq!(next_create_order([10, 20, 30]), 40);
    }

    #[test]
    fn next_create_order_handles_unsorted_input() {
        // Iterator order is whatever BTreeMap::values() produces; the
        // helper must compute max regardless.
        assert_eq!(next_create_order([30, 10, 20]), 40);
    }

    #[test]
    fn next_create_order_saturates_at_u32_max() {
        // Pathological — would take >400 million page creations. The
        // helper must not panic.
        assert_eq!(next_create_order([u32::MAX]), u32::MAX);
    }

    fn pages(entries: &[(PageId, u32)]) -> Vec<(PageId, u32)> {
        entries.to_vec()
    }

    #[test]
    fn swap_with_all_explicit_orders_just_swaps_the_two() {
        // Steady-state: every page has an explicit order. No migration
        // needed; the swap only touches the two pages clicked.
        let result = compute_swap_orders(&pages(&[(1, 10), (2, 20), (3, 30)]), 1, 2);
        let expected: BTreeMap<PageId, u32> = [(1, 20), (2, 10), (3, 30)].into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn swap_with_all_zero_orders_migrates_every_page() {
        // The user-reported bug: a site with all legacy pages at
        // order=0 used to mutate orders only locally. The pure helper
        // must produce explicit orders for *every* page so the caller
        // can sign and propagate them all to the network — otherwise
        // the unmigrated pages stay at 0 on the network and clump to
        // the front on refresh.
        let result = compute_swap_orders(&pages(&[(1, 0), (2, 0), (3, 0), (4, 0)]), 1, 2);
        // Sorted by (order=0, id): 1, 2, 3, 4 -> assigned 10, 20, 30, 40.
        // Then swap pages 1 and 2: page 1 gets 2's order (20), page 2 gets 1's (10).
        // Pages 3 and 4 keep their migrated orders.
        let expected: BTreeMap<PageId, u32> =
            [(1, 20), (2, 10), (3, 30), (4, 40)].into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn swap_with_mixed_zero_and_explicit_orders_migrates_everyone() {
        // After issue #15 was filed but before it was fixed, a site
        // could have a mix of legacy pages at order=0 and newly-
        // created pages at order=10/20/.... Migrating only the all-
        // zero subset would leave the explicit-order pages
        // overlapping the migrated grid — the helper migrates the
        // whole site whenever ANY page is still at 0.
        let result = compute_swap_orders(&pages(&[(1, 0), (2, 0), (3, 10)]), 1, 3);
        // Sorted by (order, id): (0,1), (0,2), (10,3) -> migrated to
        // 10, 20, 30 respectively. Then swap pages 1 and 3: page 1
        // gets 30, page 3 gets 10.
        let expected: BTreeMap<PageId, u32> = [(1, 30), (2, 20), (3, 10)].into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn migration_sort_key_matches_sidebar_sort_key() {
        // The sidebar sorts pages by `(order, id)`. The migration must
        // use the same key so the post-migration sequence visually
        // matches what the user clicked. Here pages 5 and 1 share
        // order=0 — the sort tiebreaker by id puts 1 before 5.
        let result = compute_swap_orders(&pages(&[(5, 0), (1, 0), (3, 5)]), 5, 1);
        // Sorted: (0,1), (0,5), (5,3) -> migrated 10, 20, 30.
        // Then swap pages 5 and 1: page 5 (was 20) gets 10, page 1
        // (was 10) gets 20.
        let expected: BTreeMap<PageId, u32> = [(1, 20), (5, 10), (3, 30)].into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn swap_is_a_no_op_when_pages_share_an_order_after_migration() {
        // Defensive: if both swap targets happen to land on the same
        // migrated order (impossible in normal flow but cheap to
        // pin), the result is a no-op rather than corruption.
        let result = compute_swap_orders(&pages(&[(1, 10), (2, 10), (3, 30)]), 1, 2);
        let expected: BTreeMap<PageId, u32> = [(1, 10), (2, 10), (3, 30)].into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn swap_skips_when_target_id_not_in_pages() {
        // Prior implementation called `unwrap_or(0)` on the missing
        // target's order and clobbered the present page's order to
        // 0 — re-introducing the very symptom this PR is fixing.
        let result = compute_swap_orders(&pages(&[(1, 10), (2, 20)]), 1, 999);
        let expected: BTreeMap<PageId, u32> = [(1, 10), (2, 20)].into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn swap_skips_when_neither_target_id_in_pages() {
        let result = compute_swap_orders(&pages(&[(1, 10), (2, 20)]), 998, 999);
        let expected: BTreeMap<PageId, u32> = [(1, 10), (2, 20)].into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn swap_with_empty_pages_returns_empty() {
        let result = compute_swap_orders(&pages(&[]), 1, 2);
        assert!(result.is_empty());
    }

    #[test]
    fn self_swap_is_a_no_op() {
        // Sidebar arrow handler can't trigger this (it always picks a
        // distinct neighbor) but defend the helper anyway.
        let result = compute_swap_orders(&pages(&[(1, 10), (2, 20)]), 1, 1);
        let expected: BTreeMap<PageId, u32> = [(1, 10), (2, 20)].into_iter().collect();
        assert_eq!(result, expected);
    }

    // -- plan_swap orchestration tests --
    //
    // These pin the user-visible "every page must be signed when
    // migrating" rule end-to-end. A regression that re-narrows
    // `pages_to_sign` (e.g. back to `vec![page_a, page_b]`) is caught
    // by these tests, not just by the lower-level
    // `compute_swap_orders` tests.

    #[test]
    fn plan_swap_in_steady_state_signs_only_the_two_clicked_pages() {
        // Site already migrated, every page has explicit orders. The
        // swap should only require fresh signatures for the pages
        // whose orders actually changed.
        let plan = plan_swap(&pages(&[(1, 10), (2, 20), (3, 30)]), 1, 2);
        assert_eq!(plan.pages_to_sign.to_vec(), vec![1, 2]);
        let expected_orders: BTreeMap<PageId, u32> =
            [(1, 20), (2, 10), (3, 30)].into_iter().collect();
        assert_eq!(plan.new_orders, expected_orders);
    }

    #[test]
    fn plan_swap_in_migration_signs_every_page_on_the_site() {
        // The user-reported bug: previously only the two clicked
        // pages got UPDATEs sent to the network. The other pages
        // stayed at order=0 on the network and clumped to the front
        // of the sidebar after a refresh. `plan_swap` must include
        // every page in `pages_to_sign` whenever migration fires.
        let plan = plan_swap(&pages(&[(1, 0), (2, 0), (3, 0), (4, 0)]), 1, 2);
        let mut signed = plan.pages_to_sign.clone();
        signed.sort();
        assert_eq!(signed, vec![1, 2, 3, 4]);
    }

    #[test]
    fn plan_swap_in_mixed_state_signs_every_page() {
        // Mixed-state site: some pages migrated to explicit orders,
        // some still at 0 (the symptom #15 left behind even after
        // PR #13's `updated_at` fix). Migration must fire and sign
        // every page on the site.
        let plan = plan_swap(&pages(&[(1, 0), (2, 10), (3, 0)]), 1, 3);
        let mut signed = plan.pages_to_sign.clone();
        signed.sort();
        assert_eq!(signed, vec![1, 2, 3]);
    }

    #[test]
    fn plan_swap_is_idempotent_after_a_migration_round() {
        // After the first reorder migrates the site, the second
        // reorder must NOT migrate again — it should just swap the
        // two clicked pages. Without this, every reorder on a
        // formerly-zero-order site would re-sign every page forever.
        let migrated = pages(&[(1, 20), (2, 10), (3, 30), (4, 40)]); // post-first-swap
        let plan = plan_swap(&migrated, 3, 4);
        let mut signed = plan.pages_to_sign.clone();
        signed.sort();
        assert_eq!(signed, vec![3, 4]);
    }

    #[test]
    fn plan_swap_returns_empty_pages_to_sign_when_swap_is_a_no_op() {
        // No page changed order -> nothing needs signing. The
        // orchestrator uses this signal to skip the network round-
        // trip entirely.
        let plan = plan_swap(&pages(&[(1, 10), (2, 10)]), 1, 2);
        assert!(plan.pages_to_sign.is_empty());
    }

    #[test]
    fn plan_swap_returns_empty_pages_to_sign_when_target_missing() {
        // Same skip behaviour for missing swap targets — the
        // orchestrator treats this as a no-op.
        let plan = plan_swap(&pages(&[(1, 10), (2, 20)]), 1, 999);
        assert!(plan.pages_to_sign.is_empty());
    }
}
