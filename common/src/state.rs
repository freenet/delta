use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable page identifier. Monotonically increasing, never reused.
pub type PageId = u64;

// ---------------------------------------------------------------------------
// Parameters (fixed at contract creation, determines contract key)
// ---------------------------------------------------------------------------

/// Contract parameters — the owner's public key and a unique site ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteParameters {
    pub owner: VerifyingKey,
    /// Random bytes to make the contract key unique per site.
    pub site_id: [u8; 32],
}

// ---------------------------------------------------------------------------
// Site state
// ---------------------------------------------------------------------------

/// Top-level state for a Delta site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteState {
    pub config: SignedConfig,
    pub pages: BTreeMap<PageId, Page>,
    /// Next page ID to assign. Monotonically increasing.
    pub next_page_id: PageId,
}

impl Default for SiteState {
    fn default() -> Self {
        Self {
            config: SignedConfig::default(),
            pages: BTreeMap::new(),
            next_page_id: 1,
        }
    }
}

impl SiteState {
    /// Create a new site with initial config, signed by the owner.
    pub fn new(config: SiteConfig, owner_key: &SigningKey) -> Self {
        Self {
            config: SignedConfig::new(config, owner_key),
            pages: BTreeMap::new(),
            next_page_id: 1,
        }
    }

    /// Verify the entire state against the site parameters.
    pub fn verify(&self, params: &SiteParameters) -> Result<(), String> {
        self.config.verify(&params.owner)?;
        for (&page_id, page) in &self.pages {
            page.verify(page_id, &params.owner)?;
        }
        Ok(())
    }

    /// Add or update a page. The page must be signed by the owner.
    pub fn upsert_page(
        &mut self,
        page_id: PageId,
        page: Page,
        owner: &VerifyingKey,
    ) -> Result<(), String> {
        page.verify(page_id, owner)?;

        if !self.pages.contains_key(&page_id) && page_id >= self.next_page_id {
            self.next_page_id = page_id + 1;
        }
        self.pages.insert(page_id, page);
        Ok(())
    }

    /// Delete a page by ID. Requires a signed deletion.
    pub fn delete_page(
        &mut self,
        deletion: &SignedPageDeletion,
        owner: &VerifyingKey,
    ) -> Result<(), String> {
        deletion.verify(owner)?;
        self.pages.remove(&deletion.page_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteConfig {
    /// Config version — must increase on each update.
    pub version: u32,
    pub name: String,
    pub description: String,
}

impl Default for SiteConfig {
    fn default() -> Self {
        Self {
            version: 1,
            name: "Untitled Site".to_string(),
            description: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedConfig {
    pub config: SiteConfig,
    pub signature: Signature,
}

impl Default for SignedConfig {
    fn default() -> Self {
        Self {
            config: SiteConfig::default(),
            signature: Signature::from_bytes(&[0u8; 64]),
        }
    }
}

impl SignedConfig {
    pub fn new(config: SiteConfig, owner_key: &SigningKey) -> Self {
        let sig = sign_bytes(&config_signing_bytes(&config), owner_key);
        Self {
            config,
            signature: sig,
        }
    }

    pub fn verify(&self, owner: &VerifyingKey) -> Result<(), String> {
        let bytes = config_signing_bytes(&self.config);
        owner
            .verify(&bytes, &self.signature)
            .map_err(|e| format!("invalid config signature: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub title: String,
    /// Markdown content. Links to other pages use `[[page_id|Display Text]]` syntax.
    pub content: String,
    /// Unix timestamp (seconds) of last update.
    pub updated_at: u64,
    /// Owner's signature over `(page_id, title, content, updated_at)`.
    pub signature: Signature,
}

impl Page {
    /// Create a new signed page.
    pub fn new(
        page_id: PageId,
        title: String,
        content: String,
        updated_at: u64,
        owner_key: &SigningKey,
    ) -> Self {
        let bytes = page_signing_bytes(page_id, &title, &content, updated_at);
        Self {
            title,
            content,
            updated_at,
            signature: sign_bytes(&bytes, owner_key),
        }
    }

    /// Verify the page signature.
    pub fn verify(&self, page_id: PageId, owner: &VerifyingKey) -> Result<(), String> {
        let bytes = page_signing_bytes(page_id, &self.title, &self.content, self.updated_at);
        owner
            .verify(&bytes, &self.signature)
            .map_err(|e| format!("invalid page signature for page {page_id}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Page deletion
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedPageDeletion {
    pub page_id: PageId,
    /// Unix timestamp of the deletion.
    pub deleted_at: u64,
    pub signature: Signature,
}

impl SignedPageDeletion {
    pub fn new(page_id: PageId, deleted_at: u64, owner_key: &SigningKey) -> Self {
        let bytes = deletion_signing_bytes(page_id, deleted_at);
        Self {
            page_id,
            deleted_at,
            signature: sign_bytes(&bytes, owner_key),
        }
    }

    pub fn verify(&self, owner: &VerifyingKey) -> Result<(), String> {
        let bytes = deletion_signing_bytes(self.page_id, self.deleted_at);
        owner
            .verify(&bytes, &self.signature)
            .map_err(|e| format!("invalid deletion signature for page {}: {e}", self.page_id))
    }
}

// ---------------------------------------------------------------------------
// Summary & Delta (for efficient sync)
// ---------------------------------------------------------------------------

/// Compact summary of site state — sent to peers to compute deltas.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct SiteStateSummary {
    pub config_version: u32,
    /// For each page: (content_hash, updated_at).
    pub pages: BTreeMap<PageId, (blake3::Hash, u64)>,
}

impl SiteState {
    pub fn summarize(&self) -> SiteStateSummary {
        SiteStateSummary {
            config_version: self.config.config.version,
            pages: self
                .pages
                .iter()
                .map(|(&id, page)| {
                    let hash = blake3::hash(page.content.as_bytes());
                    (id, (hash, page.updated_at))
                })
                .collect(),
        }
    }
}

/// Delta to bring a peer's state up to date.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteStateDelta {
    /// Updated config (if version increased).
    pub config: Option<SignedConfig>,
    /// Pages to add or update.
    pub page_updates: BTreeMap<PageId, Page>,
    /// Pages to delete (with signed proof).
    pub page_deletions: Vec<SignedPageDeletion>,
}

impl SiteState {
    /// Compute delta needed to bring a peer with the given summary up to date.
    pub fn compute_delta(&self, summary: &SiteStateSummary) -> Option<SiteStateDelta> {
        let config = if self.config.config.version > summary.config_version {
            Some(self.config.clone())
        } else {
            None
        };

        let mut page_updates = BTreeMap::new();
        for (&id, page) in &self.pages {
            let dominated = summary.pages.get(&id).is_some_and(|(hash, ts)| {
                *hash == blake3::hash(page.content.as_bytes()) && *ts == page.updated_at
            });
            if !dominated {
                page_updates.insert(id, page.clone());
            }
        }

        // Pages the peer has that we don't — they were deleted.
        // We can't produce signed deletions retroactively here,
        // so we skip this for now. Deletions must be explicitly
        // propagated via update_state.

        if config.is_none() && page_updates.is_empty() {
            None
        } else {
            Some(SiteStateDelta {
                config,
                page_updates,
                page_deletions: Vec::new(),
            })
        }
    }

    /// Apply a delta to this state. Verifies all signatures.
    pub fn apply_delta(
        &mut self,
        delta: &SiteStateDelta,
        params: &SiteParameters,
    ) -> Result<(), String> {
        if let Some(new_config) = &delta.config {
            new_config.verify(&params.owner)?;
            if new_config.config.version > self.config.config.version {
                self.config = new_config.clone();
            }
        }

        for (&page_id, page) in &delta.page_updates {
            // Only accept if newer than what we have
            let dominated = self
                .pages
                .get(&page_id)
                .is_some_and(|existing| existing.updated_at >= page.updated_at);
            if !dominated {
                self.upsert_page(page_id, page.clone(), &params.owner)?;
            }
        }

        for deletion in &delta.page_deletions {
            self.delete_page(deletion, &params.owner)?;
        }

        Ok(())
    }

    /// Merge another complete state into this one. Keeps the newer version of
    /// each page. Used when receiving a full state via UpdateData::State.
    pub fn merge(&mut self, params: &SiteParameters, other: &SiteState) -> Result<(), String> {
        other.verify(params)?;

        if other.config.config.version > self.config.config.version {
            self.config = other.config.clone();
        }

        for (&page_id, page) in &other.pages {
            let dominated = self
                .pages
                .get(&page_id)
                .is_some_and(|existing| existing.updated_at >= page.updated_at);
            if !dominated {
                self.pages.insert(page_id, page.clone());
                if page_id >= self.next_page_id {
                    self.next_page_id = page_id + 1;
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Signing helpers
// ---------------------------------------------------------------------------

fn sign_bytes(bytes: &[u8], key: &SigningKey) -> Signature {
    use ed25519_dalek::Signer;
    key.sign(bytes)
}

fn config_signing_bytes(config: &SiteConfig) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"delta:config:");
    buf.extend_from_slice(&config.version.to_le_bytes());
    buf.extend_from_slice(config.name.as_bytes());
    buf.extend_from_slice(config.description.as_bytes());
    buf
}

fn page_signing_bytes(page_id: PageId, title: &str, content: &str, updated_at: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"delta:page:");
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.extend_from_slice(title.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(&updated_at.to_le_bytes());
    buf
}

fn deletion_signing_bytes(page_id: PageId, deleted_at: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"delta:delete:");
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.extend_from_slice(&deleted_at.to_le_bytes());
    buf
}

// ---------------------------------------------------------------------------
// Delegate request/response types
// ---------------------------------------------------------------------------

/// Requests from the UI to the delegate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DelegateRequest {
    /// Store the owner signing key.
    StoreSigningKey { key_bytes: Vec<u8> },
    /// Sign a page update.
    SignPage {
        page_id: PageId,
        title: String,
        content: String,
        updated_at: u64,
    },
    /// Sign a page deletion.
    SignPageDeletion { page_id: PageId, deleted_at: u64 },
    /// Sign a config update.
    SignConfig { config: SiteConfig },
    /// Get the owner's public key.
    GetPublicKey,
}

/// Responses from the delegate to the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DelegateResponse {
    /// Signing key stored successfully.
    KeyStored,
    /// Signed page ready for publishing.
    SignedPage { page_id: PageId, page: Page },
    /// Signed deletion ready for publishing.
    SignedDeletion(SignedPageDeletion),
    /// Signed config ready for publishing.
    SignedConfig(SignedConfig),
    /// The owner's public key.
    PublicKey(VerifyingKey),
    /// An error occurred.
    Error(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn gen_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn make_params(owner: &SigningKey) -> SiteParameters {
        SiteParameters {
            owner: owner.verifying_key(),
            site_id: [0u8; 32],
        }
    }

    #[test]
    fn create_site_and_add_page() {
        let owner = gen_key();
        let params = make_params(&owner);

        let mut site = SiteState::new(
            SiteConfig {
                name: "My Site".into(),
                ..Default::default()
            },
            &owner,
        );

        let page = Page::new(1, "Home".into(), "# Welcome".into(), 1000, &owner);
        site.upsert_page(1, page, &params.owner).unwrap();

        assert_eq!(site.pages.len(), 1);
        assert_eq!(site.pages[&1].title, "Home");
        assert!(site.verify(&params).is_ok());
    }

    #[test]
    fn reject_page_with_wrong_signer() {
        let owner = gen_key();
        let attacker = gen_key();
        let params = make_params(&owner);

        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Hacked".into(), "bad content".into(), 1000, &attacker);
        let result = site.upsert_page(1, page, &params.owner);
        assert!(result.is_err());
    }

    #[test]
    fn page_update_replaces_content() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page_v1 = Page::new(1, "Home".into(), "# V1".into(), 1000, &owner);
        site.upsert_page(1, page_v1, &params.owner).unwrap();

        let page_v2 = Page::new(1, "Home".into(), "# V2".into(), 2000, &owner);
        site.upsert_page(1, page_v2, &params.owner).unwrap();

        assert_eq!(site.pages[&1].content, "# V2");
    }

    #[test]
    fn rename_preserves_id() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Old Title".into(), "content".into(), 1000, &owner);
        site.upsert_page(1, page, &params.owner).unwrap();

        let renamed = Page::new(1, "New Title".into(), "content".into(), 2000, &owner);
        site.upsert_page(1, renamed, &params.owner).unwrap();

        assert_eq!(site.pages[&1].title, "New Title");
        assert_eq!(site.pages.len(), 1);
    }

    #[test]
    fn delete_page() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Home".into(), "content".into(), 1000, &owner);
        site.upsert_page(1, page, &params.owner).unwrap();
        assert_eq!(site.pages.len(), 1);

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site.delete_page(&deletion, &params.owner).unwrap();
        assert!(site.pages.is_empty());
    }

    #[test]
    fn delta_sync() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let summary_before = site.summarize();

        let page = Page::new(1, "Home".into(), "# Hello".into(), 1000, &owner);
        site.upsert_page(1, page, &params.owner).unwrap();

        let delta = site
            .compute_delta(&summary_before)
            .expect("should have delta");

        let mut peer = SiteState::new(SiteConfig::default(), &owner);
        peer.apply_delta(&delta, &params).unwrap();

        assert_eq!(peer.pages.len(), 1);
        assert_eq!(peer.pages[&1].content, "# Hello");
    }

    #[test]
    fn merge_keeps_newer() {
        let owner = gen_key();
        let params = make_params(&owner);

        let mut site_a = SiteState::new(SiteConfig::default(), &owner);
        let mut site_b = SiteState::new(SiteConfig::default(), &owner);

        let old = Page::new(1, "Home".into(), "old".into(), 1000, &owner);
        site_a.upsert_page(1, old, &params.owner).unwrap();

        let new = Page::new(1, "Home".into(), "new".into(), 2000, &owner);
        site_b.upsert_page(1, new, &params.owner).unwrap();

        site_a.merge(&params, &site_b).unwrap();
        assert_eq!(site_a.pages[&1].content, "new");
    }

    #[test]
    fn next_page_id_advances() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let p1 = Page::new(1, "A".into(), "a".into(), 1000, &owner);
        site.upsert_page(1, p1, &params.owner).unwrap();
        assert_eq!(site.next_page_id, 2);

        let p5 = Page::new(5, "B".into(), "b".into(), 2000, &owner);
        site.upsert_page(5, p5, &params.owner).unwrap();
        assert_eq!(site.next_page_id, 6);
    }
}
