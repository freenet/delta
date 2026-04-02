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
    /// Full owner pubkey bytes (for resolving prefix back to params).
    pub owner_pubkey: [u8; 32],
    /// Contract key (if known — set after PUT or GET).
    #[serde(skip)]
    pub contract_key: Option<ContractKey>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SiteRole {
    Owner,
    Visitor,
}

/// Owner signing keys, keyed by site prefix.
/// Held in browser memory — lost on page refresh.
/// (Delegate persistence comes later.)
pub static SIGNING_KEYS: GlobalSignal<BTreeMap<String, SigningKey>> =
    GlobalSignal::new(BTreeMap::new);

// ---------------------------------------------------------------------------
// Global signals
// ---------------------------------------------------------------------------

/// All known sites, keyed by their 10-char prefix.
pub static SITES: GlobalSignal<BTreeMap<String, KnownSite>> = GlobalSignal::new(BTreeMap::new);

/// Currently selected site prefix.
pub static CURRENT_SITE: GlobalSignal<Option<String>> = GlobalSignal::new(|| None);

/// Currently selected page ID within the current site.
pub static CURRENT_PAGE: GlobalSignal<Option<PageId>> = GlobalSignal::new(|| None);

/// Whether we're in edit mode.
pub static EDITING: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Whether the "add site" dialog is showing.
pub static SHOW_ADD_SITE: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Editor content (buffered separately from saved state).
pub static EDITOR_TITLE: GlobalSignal<String> = GlobalSignal::new(String::new);
pub static EDITOR_CONTENT: GlobalSignal<String> = GlobalSignal::new(String::new);

/// Site contract WASM (for computing contract keys).
const SITE_CONTRACT_WASM: &[u8] = include_bytes!("../public/contracts/site_contract.wasm");

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

/// Parse hash route: #prefix/page_id/slug → (prefix, page_id)
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

/// Build a hash route for a site + optional page.
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

/// Create a new owned site — generates keypair, signs initial state,
/// PUTs to the Freenet network.
pub fn create_new_site(name: String) {
    // Generate Ed25519 keypair
    let signing_key = SigningKey::generate(&mut rand::thread_rng());
    let verifying_key = signing_key.verifying_key();

    // Build parameters
    let mut site_id = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut site_id);
    let params = SiteParameters {
        owner: verifying_key,
        site_id,
    };

    let prefix = params.site_prefix();

    // Create initial state with a signed home page
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

    // Compute contract key
    let mut params_buf = Vec::new();
    ciborium::ser::into_writer(&params, &mut params_buf).expect("CBOR params");
    let contract_code = ContractCode::from(SITE_CONTRACT_WASM);
    let contract_key =
        ContractKey::from_params_and_code(Parameters::from(params_buf.clone()), &contract_code);

    // Store signing key in memory and persist to delegate
    let sk_bytes = signing_key.to_bytes();
    SIGNING_KEYS.write().insert(prefix.clone(), signing_key);
    crate::freenet_api::delegate::store_signing_key(&prefix, &sk_bytes);

    // Add to known sites
    let site = KnownSite {
        name: name.clone(),
        prefix: prefix.clone(),
        role: SiteRole::Owner,
        state: site_state.clone(),
        owner_pubkey: verifying_key.to_bytes(),
        contract_key: Some(contract_key),
    };
    SITES.write().insert(prefix.clone(), site);

    // PUT to Freenet network
    crate::freenet_api::put_site(&params, &site_state);

    *SHOW_ADD_SITE.write() = false;
    select_site(&prefix);
}

/// Visit an existing site by contract ID string (base58). Sends GET + SUBSCRIBE.
pub fn visit_site(contract_id_str: String) {
    // Parse base58 contract ID
    let contract_id: ContractInstanceId = match contract_id_str.parse() {
        Ok(id) => id,
        Err(_) => return, // TODO: show error in UI
    };

    let prefix = contract_id_str[..10.min(contract_id_str.len())].to_string();

    // Add placeholder entry so it shows in the sidebar immediately
    let placeholder = KnownSite {
        name: format!("Loading ({prefix}...)"),
        prefix: prefix.clone(),
        role: SiteRole::Visitor,
        state: SiteState::default(),
        owner_pubkey: [0u8; 32],
        contract_key: None, // We only have the instance ID, not the full key
    };
    SITES.write().insert(prefix.clone(), placeholder);

    // GET the site state and SUBSCRIBE for updates
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

pub fn create_page(title: String) {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };

    // Get signing key if owner
    let signing_key = SIGNING_KEYS.read().get(&prefix).cloned();

    let mut sites = SITES.write();
    let Some(site) = sites.get_mut(&prefix) else {
        return;
    };

    let id = site.state.next_page_id;
    let now = now_secs();

    let page = if let Some(sk) = &signing_key {
        Page::new(id, title, String::new(), now, sk)
    } else {
        // Fallback for example data (unsigned)
        Page {
            title,
            content: String::new(),
            updated_at: now,
            signature: Signature::from_bytes(&[0u8; 64]),
        }
    };

    site.state.pages.insert(id, page.clone());
    site.state.next_page_id = id + 1;

    // Send UPDATE to network if we have a contract key
    let contract_key = site.contract_key;
    drop(sites);

    if let Some(ck) = contract_key {
        let mut updates = BTreeMap::new();
        updates.insert(id, page);
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates: updates,
            page_deletions: Vec::new(),
        };
        crate::freenet_api::update_site(&ck, &delta);
    }

    *CURRENT_PAGE.write() = Some(id);
    *EDITING.write() = true;
}

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

    let signing_key = SIGNING_KEYS.read().get(&prefix).cloned();

    let page = if let Some(sk) = &signing_key {
        Page::new(page_id, title, content, now, sk)
    } else {
        Page {
            title,
            content,
            updated_at: now,
            signature: Signature::from_bytes(&[0u8; 64]),
        }
    };

    let mut sites = SITES.write();
    let contract_key = if let Some(site) = sites.get_mut(&prefix) {
        site.state.pages.insert(page_id, page.clone());
        site.contract_key
    } else {
        None
    };
    drop(sites);

    // Send UPDATE to network
    if let Some(ck) = contract_key {
        let mut updates = BTreeMap::new();
        updates.insert(page_id, page);
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates: updates,
            page_deletions: Vec::new(),
        };
        crate::freenet_api::update_site(&ck, &delta);
    }

    *EDITING.write() = false;
}

pub fn delete_page(page_id: PageId) {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };

    let signing_key = SIGNING_KEYS.read().get(&prefix).cloned();

    let mut sites = SITES.write();
    if let Some(site) = sites.get_mut(&prefix) {
        site.state.pages.remove(&page_id);

        // Send signed deletion to network
        if let (Some(ck), Some(sk)) = (&site.contract_key, &signing_key) {
            let deletion = delta_core::SignedPageDeletion::new(page_id, now_secs(), sk);
            let delta = delta_core::SiteStateDelta {
                config: None,
                page_updates: BTreeMap::new(),
                page_deletions: vec![deletion],
            };
            crate::freenet_api::update_site(ck, &delta);
        }

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

/// Navigate to a page by ID (used by page links in rendered markdown).
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
