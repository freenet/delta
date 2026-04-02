use delta_core::{Page, PageId, SiteState};
use dioxus::prelude::*;
use ed25519_dalek::Signature;
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SiteRole {
    Owner,
    Visitor,
}

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

/// Editor content (buffered separately from saved state).
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
    *CURRENT_SITE.write() = Some(prefix.to_string());

    // Select first page of the site
    let sites = SITES.read();
    if let Some(site) = sites.get(prefix) {
        let first_page = site.state.pages.keys().next().copied();
        *CURRENT_PAGE.write() = first_page;
        // Update URL hash
        if let Some(page_id) = first_page {
            let title = site.state.pages.get(&page_id).map(|p| p.title.as_str());
            update_hash(&build_hash_route(prefix, Some(page_id), title));
        } else {
            update_hash(&build_hash_route(prefix, None, None));
        }
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

    // Update URL hash
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
    let mut sites = SITES.write();
    let Some(site) = sites.get_mut(&prefix) else {
        return;
    };

    let id = site.state.next_page_id;
    let page = Page {
        title,
        content: String::new(),
        updated_at: now_secs(),
        signature: Signature::from_bytes(&[0u8; 64]),
    };
    site.state.pages.insert(id, page);
    site.state.next_page_id = id + 1;

    drop(sites);
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

    let mut sites = SITES.write();
    if let Some(site) = sites.get_mut(&prefix) {
        if let Some(page) = site.state.pages.get_mut(&page_id) {
            page.title = title;
            page.content = content;
            page.updated_at = now_secs();
        }
    }
    *EDITING.write() = false;
}

pub fn delete_page(page_id: PageId) {
    let Some(prefix) = (*CURRENT_SITE.read()).clone() else {
        return;
    };
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

/// Navigate to a page by ID (used by page links).
#[allow(dead_code)]
pub fn navigate_to_page(page_id: PageId) {
    // Check if page exists in current site
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
            if let Ok(location) = window.location().set_hash(hash) {
                let _ = location;
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = hash;
    }
}
