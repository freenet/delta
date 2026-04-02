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

pub fn init_example_data() {
    let sites = crate::example_data::create_example_sites();
    let first_prefix = sites.keys().next().cloned();
    *SITES.write() = sites;

    if let Some(prefix) = first_prefix {
        select_site(&prefix);
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
        let first_page = site.state.pages.keys().next().copied();
        *CURRENT_PAGE.write() = first_page;
        if let Some(page_id) = first_page {
            let title = site.state.pages.get(&page_id).map(|p| p.title.as_str());
            update_hash(&build_hash_route(prefix, Some(page_id), title));
        } else {
            update_hash(&build_hash_route(prefix, None, None));
        }
    }
}

pub fn show_add_site_prompt() {
    *SHOW_ADD_SITE.write() = true;
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
        contract_key: None,
    };
    SITES.with_mut(|sites| {
        sites.insert(prefix.clone(), site);
    });

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

/// Visit an existing site by contract ID string (base58). Sends GET + SUBSCRIBE.
pub fn visit_site(contract_id_str: String) {
    let contract_id: ContractInstanceId = match contract_id_str.parse() {
        Ok(id) => id,
        Err(_) => return,
    };

    let prefix = contract_id_str[..10.min(contract_id_str.len())].to_string();

    let placeholder = KnownSite {
        name: format!("Loading ({prefix}...)"),
        prefix: prefix.clone(),
        role: SiteRole::Visitor,
        state: SiteState::default(),
        owner_pubkey: [0u8; 32],
        contract_key: None,
    };
    SITES.write().insert(prefix.clone(), placeholder);

    crate::freenet_api::get_site_by_id(&contract_id);
    crate::freenet_api::subscribe_to_site_by_id(&contract_id);

    *SHOW_ADD_SITE.write() = false;
    select_site(&prefix);
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
        let title = sites
            .get(prefix)
            .and_then(|s| s.state.pages.get(&page_id))
            .map(|p| p.title.as_str());
        update_hash(&build_hash_route(prefix, Some(page_id), title));
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

fn update_hash(hash: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.location().set_hash(hash);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = hash;
    }
}
