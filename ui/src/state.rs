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
pub static EDITOR_TITLE: GlobalSignal<String> = GlobalSignal::new(String::new);
pub static EDITOR_CONTENT: GlobalSignal<String> = GlobalSignal::new(String::new);

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize from URL hash if present (e.g. user arrived via a shared link).
/// Pending page to select after site loads from network.
pub static PENDING_PAGE_ID: GlobalSignal<Option<PageId>> = GlobalSignal::new(|| None);

pub fn init_from_hash() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let hash = window.location().hash().unwrap_or_default();
            if let Some((prefix, page_id)) = parse_hash_route(&hash) {
                // Store page_id to select once the site content loads
                if let Some(pid) = page_id {
                    *PENDING_PAGE_ID.write() = Some(pid);
                }
                visit_site(prefix);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Route parsing / updating
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub fn parse_hash_route(hash: &str) -> Option<(String, Option<PageId>)> {
    let hash = hash.trim_start_matches('#').trim_start_matches('/');
    if hash.is_empty() {
        return None;
    }
    let parts: Vec<&str> = hash.splitn(3, '/').collect();
    let prefix = parts[0].to_string();
    let page_id = parts.get(1).and_then(|s| s.parse::<PageId>().ok());
    Some((prefix, page_id))
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
}

/// Rename a site. Updates local state and saves to delegate.
pub fn rename_site(prefix: &str, new_name: String) {
    SITES.with_mut(|sites| {
        if let Some(site) = sites.get_mut(prefix) {
            site.name = new_name.clone();
            site.state.config.config.name = new_name;
            // TODO: sign the new config via delegate and UPDATE the contract
        }
    });
    crate::freenet_api::delegate::save_known_sites();
}

/// Remove a site from the sidebar.
pub fn remove_site(prefix: &str) {
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

    // Store signing key in delegate for persistence
    let sk_bytes = signing_key.to_bytes();
    crate::freenet_api::delegate::store_signing_key(&sk_bytes);

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
pub fn visit_site(input: String) {
    let prefix = input.trim().to_string();
    if prefix.is_empty() {
        return;
    }

    // If already known, just select it
    if SITES.read().contains_key(&prefix) {
        *SHOW_ADD_SITE.write() = false;
        select_site(&prefix);
        return;
    }

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

    let id = site.state.next_page_id;
    let now = now_secs();
    let contract_key = site.contract_key;
    let is_owner = site.role == SiteRole::Owner;
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
            );
            // Optimistically add to local state with placeholder sig
            let mut sites = SITES.write();
            if let Some(site) = sites.get_mut(&prefix) {
                let page = Page {
                    title,
                    content: String::new(),
                    updated_at: now,
                    signature: Signature::from_bytes(&[0u8; 64]),
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
    let now = now_secs();

    let sites = SITES.read();
    let contract_key = sites.get(&prefix).and_then(|s| s.contract_key);
    let is_owner = sites
        .get(&prefix)
        .map(|s| s.role == SiteRole::Owner)
        .unwrap_or(false);
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
