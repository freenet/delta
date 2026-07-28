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

/// Length of the site prefix (first N chars of base58-encoded owner pubkey).
pub const SITE_PREFIX_LEN: usize = 10;

/// Contract parameters = the 10-char site prefix.
///
/// This is the ONLY parameter — it determines the contract key via
/// `BLAKE3(BLAKE3(site_contract.wasm) || prefix_bytes)`.
///
/// The contract validates that the owner's public key (in the state)
/// produces this prefix when base58-encoded. This means anyone who
/// knows the 10-char prefix can reconstruct the full contract key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteParameters {
    /// The 10-char base58 prefix derived from the owner's public key.
    pub prefix: String,
}

impl SiteParameters {
    /// Create parameters from an owner's public key.
    pub fn from_owner(owner: &VerifyingKey) -> Self {
        Self {
            prefix: pubkey_to_prefix(owner),
        }
    }

    /// Validate that a public key matches these parameters.
    pub fn matches_owner(&self, owner: &VerifyingKey) -> bool {
        pubkey_to_prefix(owner) == self.prefix
    }
}

/// Derive the 10-char site prefix from an owner's public key.
pub fn pubkey_to_prefix(owner: &VerifyingKey) -> String {
    let encoded = bs58::encode(owner.as_bytes()).into_string();
    encoded[..SITE_PREFIX_LEN.min(encoded.len())].to_string()
}

// ---------------------------------------------------------------------------
// Site state
// ---------------------------------------------------------------------------

/// Top-level state for a Delta site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteState {
    /// The site owner's public key. All signatures are verified against this.
    pub owner: VerifyingKey,
    pub config: SignedConfig,
    pub pages: BTreeMap<PageId, Page>,
    /// Next page ID to assign. Monotonically increasing.
    pub next_page_id: PageId,
    /// Tombstones for deleted pages - prevents re-adding during merge.
    #[serde(default)]
    pub deleted_pages: BTreeMap<PageId, SignedPageDeletion>,
}

/// The owner key [`SiteState::default`] carries. The contract starts from a
/// default state when no state exists yet, so this value marks "no owner has
/// claimed this address"; it never matches a real key.
fn placeholder_owner() -> VerifyingKey {
    // A zeroed key — only valid for empty/placeholder states
    let zero_bytes = [0u8; 32];
    VerifyingKey::from_bytes(&zero_bytes).unwrap_or_else(|_| {
        // Fallback: this will fail verification but won't panic
        VerifyingKey::from_bytes(&[
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ])
        .expect("hardcoded valid point")
    })
}

impl Default for SiteState {
    fn default() -> Self {
        Self {
            owner: placeholder_owner(),
            config: SignedConfig::default(),
            pages: BTreeMap::new(),
            next_page_id: 1,
            deleted_pages: BTreeMap::new(),
        }
    }
}

impl SiteState {
    /// Create a new site with initial config, signed by the owner.
    pub fn new(config: SiteConfig, owner_key: &SigningKey) -> Self {
        Self {
            owner: owner_key.verifying_key(),
            config: SignedConfig::new(config, owner_key),
            pages: BTreeMap::new(),
            next_page_id: 1,
            deleted_pages: BTreeMap::new(),
        }
    }

    /// Verify the entire state against the site parameters.
    /// Checks that the owner pubkey produces the expected prefix,
    /// and all signatures are valid.
    pub fn verify(&self, params: &SiteParameters) -> Result<(), String> {
        // Verify that the owner's pubkey matches the prefix in parameters
        if !params.matches_owner(&self.owner) {
            return Err(format!(
                "owner pubkey doesn't match parameters prefix: expected {}, got {}",
                params.prefix,
                pubkey_to_prefix(&self.owner)
            ));
        }
        self.config.verify(&self.owner)?;
        for (&page_id, page) in &self.pages {
            page.verify(page_id, &self.owner)?;
        }
        // Tombstones are as destructive as pages are constructive: `merge`
        // applies them unconditionally and they permanently suppress the page
        // id. They MUST be authenticated here, or any peer can copy this
        // site's public `owner` and owner-signed `config`, ship no pages, and
        // attach tombstones signed by nobody to wipe the site.
        for (&page_id, deletion) in &self.deleted_pages {
            // The map key is what `merge` deletes by, so a tombstone re-keyed
            // under a different id would retarget a genuine owner signature at
            // a page the owner never deleted.
            if deletion.page_id != page_id {
                return Err(format!(
                    "tombstone stored under page {page_id} is signed for page {}",
                    deletion.page_id
                ));
            }
            deletion.verify(&self.owner)?;
        }
        Ok(())
    }

    /// True while this is the placeholder state the contract starts from when
    /// no state exists at the address yet.
    ///
    /// Structural rather than owner-only, because the placeholder key is NOT
    /// self-protecting. `[0u8; 32]` decompresses to an order-4 point, and
    /// every signature check here uses ed25519-dalek's non-strict `verify`,
    /// which is cofactorless and does not reject weak keys — so signatures
    /// under the placeholder are forgeable without any private key. Its base58
    /// form is also 32 `'1'` characters, so a site whose prefix is
    /// `"1111111111"` would let a fully placeholder-owned state clear
    /// `params.matches_owner`. Requiring empty pages and tombstones as well
    /// keeps the address-claiming branch from resting on any of that.
    pub fn is_uninitialized(&self) -> bool {
        self.owner == placeholder_owner() && self.pages.is_empty() && self.deleted_pages.is_empty()
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
        // Store tombstone so merge doesn't re-add the page
        self.deleted_pages
            .insert(deletion.page_id, deletion.clone());
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
    /// Sort order (lower = earlier). Defaults to 0 for backwards compatibility.
    #[serde(default)]
    pub order: u32,
}

impl Page {
    /// Create a new signed page (v2 signature includes order).
    pub fn new(
        page_id: PageId,
        title: String,
        content: String,
        updated_at: u64,
        owner_key: &SigningKey,
    ) -> Self {
        Self::new_with_order(page_id, title, content, updated_at, 0, owner_key)
    }

    /// Create a signed page with a specific order.
    pub fn new_with_order(
        page_id: PageId,
        title: String,
        content: String,
        updated_at: u64,
        order: u32,
        owner_key: &SigningKey,
    ) -> Self {
        let bytes = page_signing_bytes_v2(page_id, &title, &content, updated_at, order);
        Self {
            title,
            content,
            updated_at,
            signature: sign_bytes(&bytes, owner_key),
            order,
        }
    }

    /// Verify the page signature. Tries v2 (with order) first, then falls
    /// back to v1 (without order) for pages signed before order was added.
    pub fn verify(&self, page_id: PageId, owner: &VerifyingKey) -> Result<(), String> {
        // Try v2 first (includes order)
        let v2_bytes = page_signing_bytes_v2(
            page_id,
            &self.title,
            &self.content,
            self.updated_at,
            self.order,
        );
        if owner.verify(&v2_bytes, &self.signature).is_ok() {
            return Ok(());
        }
        // Fall back to v1 (no order) for backwards compatibility
        let v1_bytes = page_signing_bytes_v1(page_id, &self.title, &self.content, self.updated_at);
        owner
            .verify(&v1_bytes, &self.signature)
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

    /// Apply a delta to this state. Verifies all signatures against the
    /// owner pubkey embedded in the state.
    pub fn apply_delta(
        &mut self,
        delta: &SiteStateDelta,
        params: &SiteParameters,
    ) -> Result<(), String> {
        let owner = self.owner;

        if let Some(new_config) = &delta.config {
            new_config.verify(&owner)?;
            if new_config.config.version > self.config.config.version {
                self.config = new_config.clone();
            }
        }

        for (&page_id, page) in &delta.page_updates {
            let dominated = self
                .pages
                .get(&page_id)
                .is_some_and(|existing| existing.updated_at >= page.updated_at);
            if !dominated {
                self.upsert_page(page_id, page.clone(), &owner)?;
            }
        }

        for deletion in &delta.page_deletions {
            self.delete_page(deletion, &owner)?;
        }

        let _ = params;
        Ok(())
    }

    /// Merge another complete state into this one. Keeps the newer version of
    /// each page. Used when receiving a full state via UpdateData::State.
    pub fn merge(&mut self, params: &SiteParameters, other: &SiteState) -> Result<(), String> {
        other.verify(params)?;

        // `params` only pins the first `SITE_PREFIX_LEN` characters of the
        // owner key, so a second key sharing that prefix passes `verify` and
        // would otherwise graft its config and pages onto an established
        // site. Bind the merge to the owner already at this address instead.
        if self.is_uninitialized() {
            // Nobody has claimed this address yet — the contract starts from
            // `default()` when there is no stored state. There is nothing to
            // merge into, so adopt `other` wholesale; it has already cleared
            // `verify` against these params.
            //
            // Adopting only `owner` is NOT enough. The placeholder carries an
            // unsigned default config (version 1, 64 zero bytes), a real newly
            // created site is ALSO version 1, and the config merge below is
            // strictly-greater — so `1 > 1` is false and the unsigned config
            // would survive, leaving a state with the right owner that fails
            // its own `verify`. Pinned by
            // `first_publish_adopts_the_owner_signed_config`.
            *self = other.clone();
            return Ok(());
        } else if self.owner != other.owner {
            return Err(format!(
                "refusing merge from a different owner at the same address: {} != {}",
                bs58::encode(self.owner.as_bytes()).into_string(),
                bs58::encode(other.owner.as_bytes()).into_string()
            ));
        }

        if other.config.config.version > self.config.config.version {
            self.config = other.config.clone();
        }

        // Merge tombstones from other
        for (&page_id, deletion) in &other.deleted_pages {
            self.deleted_pages
                .entry(page_id)
                .or_insert_with(|| deletion.clone());
            // Also remove from our pages if present
            self.pages.remove(&page_id);
        }

        for (&page_id, page) in &other.pages {
            // Don't re-add deleted pages
            if self.deleted_pages.contains_key(&page_id) {
                continue;
            }
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

/// V1 signing bytes: does NOT include order (for backwards compatibility).
fn page_signing_bytes_v1(page_id: PageId, title: &str, content: &str, updated_at: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"delta:page:");
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.extend_from_slice(title.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(&updated_at.to_le_bytes());
    buf
}

/// V2 signing bytes: includes order to prevent unauthorized reordering.
fn page_signing_bytes_v2(
    page_id: PageId,
    title: &str,
    content: &str,
    updated_at: u64,
    order: u32,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"delta:page:v2:");
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.extend_from_slice(title.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(&updated_at.to_le_bytes());
    buf.extend_from_slice(&order.to_le_bytes());
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

// ---------------------------------------------------------------------------
// Site key export/import
// ---------------------------------------------------------------------------

/// Exportable site key - contains the signing key for portability.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SiteKeyExport {
    /// Ed25519 signing key bytes (32 bytes).
    pub signing_key: Vec<u8>,
    /// Owner's public key bytes (for verification).
    pub owner_pubkey: Vec<u8>,
    /// Site prefix (10-char code).
    pub prefix: String,
    /// Site name (convenience).
    pub name: String,
}

const ARMOR_BEGIN: &str = "-----BEGIN DELTA SITE KEY-----";
const ARMOR_END: &str = "-----END DELTA SITE KEY-----";

impl SiteKeyExport {
    /// Serialize to armored text format.
    pub fn to_armored(&self) -> String {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(self, &mut buf).expect("CBOR serialization");
        let encoded = bs58::encode(&buf).into_string();
        // Line-wrap at 64 characters
        let lines: Vec<&str> = encoded
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!("{}\n{}\n{}", ARMOR_BEGIN, lines.join("\n"), ARMOR_END)
    }

    /// Parse from armored text format.
    pub fn from_armored(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if !text.starts_with(ARMOR_BEGIN) || !text.ends_with(ARMOR_END) {
            return Err("Invalid format: missing armor markers".into());
        }
        let inner = text
            .trim_start_matches(ARMOR_BEGIN)
            .trim_end_matches(ARMOR_END)
            .split_whitespace()
            .collect::<String>();
        let bytes = bs58::decode(&inner)
            .into_vec()
            .map_err(|e| format!("Base58 decode error: {e}"))?;
        ciborium::de::from_reader(bytes.as_slice()).map_err(|e| format!("CBOR decode error: {e}"))
    }
}

/// Determine whether a site should be treated as owned by the current user.
///
/// A site is owned if EITHER the stored record says so OR a PublicKey response
/// has confirmed ownership at runtime (which may arrive before the record is loaded).
pub fn is_site_owned(
    record_is_owner: bool,
    prefix: &str,
    confirmed_owner_prefixes: &[String],
) -> bool {
    record_is_owner || confirmed_owner_prefixes.contains(&prefix.to_string())
}

/// A lightweight record of a known site (stored in delegate for persistence).
///
/// Records with `name == TOMBSTONE_NAME_SENTINEL` are tombstones for sites
/// the user explicitly removed. They are stored alongside real records in
/// the delegate so that deletions survive a page refresh and cannot be
/// resurrected by a legacy delegate returning stale data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownSiteRecord {
    pub prefix: String,
    pub name: String,
    pub is_owner: bool,
    /// Base58-encoded contract key from when this site was last accessed.
    /// Used to detect contract WASM upgrades and migrate state.
    #[serde(default)]
    pub contract_key_b58: Option<String>,
}

/// Sentinel value stored in `KnownSiteRecord::name` to mark a record as a
/// tombstone for a removed site. The NUL prefix guarantees the sentinel
/// can never collide with a user-supplied site name (UI input strips NULs).
pub const TOMBSTONE_NAME_SENTINEL: &str = "\u{0000}__delta_removed__";

impl KnownSiteRecord {
    /// Build a tombstone record for a removed site prefix.
    pub fn tombstone(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            name: TOMBSTONE_NAME_SENTINEL.to_string(),
            is_owner: false,
            contract_key_b58: None,
        }
    }

    /// Returns true if this record is a removed-site tombstone.
    pub fn is_tombstone(&self) -> bool {
        self.name == TOMBSTONE_NAME_SENTINEL
    }
}

/// Requests from the UI to the delegate.
///
/// **Migration rule:** Every `Get*` variant that reads persisted data MUST
/// be covered by the legacy delegate migration path in
/// `ui/src/freenet_api/delegate.rs`. When a delegate WASM upgrade changes
/// the delegate key, data stored under the old key is only accessible via
/// legacy migration. If a `Get*` variant is missing from the migration
/// path, that data type is silently lost on upgrade.
///
/// Currently migrated:
/// - `GetPublicKey` -- in `fire_legacy_migration()`
/// - `GetSigningKey` -- in `fire_legacy_migration()`
/// - `GetKnownSites` -- in `fire_legacy_migration()`
/// - `GetSiteState` -- in KnownSites handler + `request_site_state_backup()`
/// - `GetSigningKeyForPrefix` -- NOT migrated (V7+ only; per-prefix keys
///   are discovered via `GetPublicKey` response and then individually
///   migrated via `store_signing_key`)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DelegateRequest {
    /// Store the owner signing key. If prefix is set, stored per-site.
    StoreSigningKey {
        key_bytes: Vec<u8>,
        #[serde(default)]
        prefix: Option<String>,
    },
    /// Sign a page update.
    SignPage {
        page_id: PageId,
        title: String,
        content: String,
        updated_at: u64,
        #[serde(default)]
        order: u32,
        #[serde(default)]
        prefix: Option<String>,
    },
    /// Sign a page deletion.
    SignPageDeletion {
        page_id: PageId,
        deleted_at: u64,
        #[serde(default)]
        prefix: Option<String>,
    },
    /// Sign a config update.
    SignConfig {
        config: SiteConfig,
        #[serde(default)]
        prefix: Option<String>,
    },
    /// Get the owner's public key. If prefix is set, returns per-site key.
    GetPublicKey,
    /// Get the owner's signing key from the legacy single-key slot.
    ///
    /// Kept as a unit variant for CBOR wire-format compatibility with
    /// pre-V7 delegates: the legacy-migration path probes old delegates
    /// with this request to rescue signing keys stranded there after a
    /// delegate WASM upgrade. Changing the shape of this variant would
    /// break deserialization on those old delegates and silently kill
    /// the migration path. For export, use `GetSigningKeyForPrefix`.
    GetSigningKey,
    /// Get the owner's signing key for a specific site prefix (for export).
    ///
    /// Introduced in V7 to fix a bug where `GetSigningKey` returned the
    /// legacy single-key slot regardless of which site the user was
    /// exporting, producing tokens whose signing_key did not match the
    /// site's owner_pubkey. Only the current (V7+) delegate understands
    /// this variant; pre-V7 delegates will fail to deserialize it, which
    /// is fine because pre-V7 delegates did not have per-prefix storage.
    GetSigningKeyForPrefix { prefix: String },
    /// Store the list of known sites (for persistence across refreshes).
    StoreKnownSites { sites: Vec<KnownSiteRecord> },
    /// Retrieve the list of known sites.
    GetKnownSites,
    /// Back up a site's state in delegate storage.
    StoreSiteState {
        prefix: String,
        state_bytes: Vec<u8>,
    },
    /// Retrieve a backed-up site state.
    GetSiteState { prefix: String },
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
    /// The owner's signing key (for export).
    SigningKey(Vec<u8>),
    /// Stored known sites.
    SitesStored,
    /// Retrieved known sites.
    KnownSites(Vec<KnownSiteRecord>),
    /// Site state backed up.
    SiteStateStored,
    /// Retrieved site state backup.
    SiteState {
        prefix: String,
        state_bytes: Vec<u8>,
    },
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
        SiteParameters::from_owner(&owner.verifying_key())
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
        site.upsert_page(1, page, &owner.verifying_key()).unwrap();

        assert_eq!(site.pages.len(), 1);
        assert_eq!(site.pages[&1].title, "Home");
        assert!(site.verify(&params).is_ok());
    }

    #[test]
    fn reject_page_with_wrong_signer() {
        let owner = gen_key();
        let attacker = gen_key();
        let _params = make_params(&owner);

        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Hacked".into(), "bad content".into(), 1000, &attacker);
        let result = site.upsert_page(1, page, &owner.verifying_key());
        assert!(result.is_err());
    }

    #[test]
    fn page_update_replaces_content() {
        let owner = gen_key();
        let _params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page_v1 = Page::new(1, "Home".into(), "# V1".into(), 1000, &owner);
        site.upsert_page(1, page_v1, &owner.verifying_key())
            .unwrap();

        let page_v2 = Page::new(1, "Home".into(), "# V2".into(), 2000, &owner);
        site.upsert_page(1, page_v2, &owner.verifying_key())
            .unwrap();

        assert_eq!(site.pages[&1].content, "# V2");
    }

    #[test]
    fn rename_preserves_id() {
        let owner = gen_key();
        let _params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Old Title".into(), "content".into(), 1000, &owner);
        site.upsert_page(1, page, &owner.verifying_key()).unwrap();

        let renamed = Page::new(1, "New Title".into(), "content".into(), 2000, &owner);
        site.upsert_page(1, renamed, &owner.verifying_key())
            .unwrap();

        assert_eq!(site.pages[&1].title, "New Title");
        assert_eq!(site.pages.len(), 1);
    }

    #[test]
    fn delete_page() {
        let owner = gen_key();
        let _params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Home".into(), "content".into(), 1000, &owner);
        site.upsert_page(1, page, &owner.verifying_key()).unwrap();
        assert_eq!(site.pages.len(), 1);

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site.delete_page(&deletion, &owner.verifying_key()).unwrap();
        assert!(site.pages.is_empty());
    }

    #[test]
    fn delta_sync() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let summary_before = site.summarize();

        let page = Page::new(1, "Home".into(), "# Hello".into(), 1000, &owner);
        site.upsert_page(1, page, &owner.verifying_key()).unwrap();

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
        site_a.upsert_page(1, old, &owner.verifying_key()).unwrap();

        let new = Page::new(1, "Home".into(), "new".into(), 2000, &owner);
        site_b.upsert_page(1, new, &owner.verifying_key()).unwrap();

        site_a.merge(&params, &site_b).unwrap();
        assert_eq!(site_a.pages[&1].content, "new");
    }

    /// Builds a site with one page, plus the hostile state a peer can assemble
    /// from that site's PUBLIC data alone: the same `owner`, the same
    /// owner-signed `config`, no pages, and caller-supplied tombstones.
    ///
    /// Asserts the shell is verify-clean BEFORE the tombstones go in, so a
    /// later rejection is attributable to the tombstone. Without that, both
    /// tombstone tests would keep passing while testing nothing if some
    /// unrelated part of the shell stopped verifying.
    fn victim_and_hostile_shell(
        owner: &SigningKey,
        params: &SiteParameters,
        deleted_pages: BTreeMap<PageId, SignedPageDeletion>,
    ) -> (SiteState, SiteState) {
        let mut victim = SiteState::new(SiteConfig::default(), owner);
        let page = Page::new(1, "Home".into(), "content".into(), 1000, owner);
        victim.upsert_page(1, page, &owner.verifying_key()).unwrap();
        assert_eq!(victim.pages.len(), 1);

        let mut hostile = SiteState {
            owner: owner.verifying_key(),
            config: victim.config.clone(),
            pages: BTreeMap::new(),
            next_page_id: victim.next_page_id,
            deleted_pages: BTreeMap::new(),
        };
        hostile
            .verify(params)
            .expect("shell must be verify-clean before tombstones are attached");
        hostile.deleted_pages = deleted_pages;
        (victim, hostile)
    }

    /// A hostile peer needs no key material and no prefix collision to wipe a
    /// site: every field `verify` inspects is public. Copy the victim's `owner`
    /// and owner-signed `config` out of the published state, ship no pages so
    /// there are no page signatures to check, and attach a tombstone signed by
    /// nobody. `verify` must reject it, or `merge` applies the tombstone and
    /// permanently suppresses the page id.
    #[test]
    fn unsigned_tombstone_is_rejected() {
        let owner = gen_key();
        let params = make_params(&owner);
        let attacker = gen_key();

        let forged = SignedPageDeletion::new(1, 9999, &attacker);
        let (mut victim, hostile) =
            victim_and_hostile_shell(&owner, &params, [(1, forged)].into_iter().collect());

        let err = hostile
            .verify(&params)
            .expect_err("a tombstone signed by an unrelated key must not validate");
        assert!(
            err.contains("invalid deletion signature"),
            "must reject on the signature, not some earlier guard: {err}"
        );
        assert!(
            victim.merge(&params, &hostile).is_err(),
            "merge must refuse a state carrying an unauthenticated tombstone"
        );
        assert_eq!(victim.pages.len(), 1, "victim's page survives");
    }

    /// Signature checking alone is not enough. A genuine tombstone the owner
    /// published for one page is public, and its signature covers only
    /// `(page_id, deleted_at)`. `merge` deletes by the MAP KEY, so re-filing a
    /// real tombstone under a different id would point a valid owner signature
    /// at a page the owner never deleted.
    #[test]
    fn tombstone_rekeyed_under_another_page_is_rejected() {
        let owner = gen_key();
        let params = make_params(&owner);

        // Genuine: the owner really did delete page 7.
        let genuine = SignedPageDeletion::new(7, 5000, &owner);
        // Re-filed under page 1, which the owner never deleted.
        let (mut victim, hostile) =
            victim_and_hostile_shell(&owner, &params, [(1, genuine)].into_iter().collect());

        let err = hostile
            .verify(&params)
            .expect_err("tombstone's own page_id must match the key it is stored under");
        assert!(
            err.contains("signed for page"),
            "must reject on the key mismatch; the signature itself is genuine: {err}"
        );
        assert!(victim.merge(&params, &hostile).is_err());
        assert_eq!(victim.pages.len(), 1, "victim's page survives");
    }

    /// The prefix in `params` pins only the first `SITE_PREFIX_LEN` characters
    /// of the owner key, so a second key sharing it lands on the same contract
    /// address and passes `verify`. It must still not be able to graft its
    /// config onto an established site.
    ///
    /// Standing in for a ground prefix collision: `params` are built from the
    /// IMPOSTOR's key, so their state clears `other.verify(params)` exactly as
    /// it would under a real collision. `merge` never compares `self.owner`
    /// against `params`, so the branch under test is reached identically.
    ///
    /// Building `params` from the victim instead makes this test VACUOUS:
    /// `verify` rejects on the prefix before the owner binding is ever
    /// consulted, so it passes even with that binding deleted. Caught by
    /// mutation testing; do not "simplify" it back.
    #[test]
    fn merge_from_a_different_owner_is_rejected() {
        let owner = gen_key();
        let mut victim = SiteState::new(SiteConfig::default(), &owner);
        let page = Page::new(1, "Home".into(), "content".into(), 1000, &owner);
        victim.upsert_page(1, page, &owner.verifying_key()).unwrap();
        let original_version = victim.config.config.version;

        let colliding = gen_key();
        let params = make_params(&colliding);
        let mut impostor = SiteState::new(
            SiteConfig {
                version: 99,
                ..SiteConfig::default()
            },
            &colliding,
        );
        let their_page = Page::new(1, "Pwned".into(), "pwned".into(), 9999, &colliding);
        impostor
            .upsert_page(1, their_page, &colliding.verifying_key())
            .unwrap();
        impostor
            .verify(&params)
            .expect("impostor must clear verify, as it would under a real collision");

        let err = victim
            .merge(&params, &impostor)
            .expect_err("merge must refuse a different owner at the same address");
        assert!(
            err.contains("different owner"),
            "must reject on the owner binding, not incidentally on the prefix: {err}"
        );
        assert_eq!(
            victim.pages[&1].content, "content",
            "victim's page survives"
        );
        assert_eq!(
            victim.config.config.version, original_version,
            "victim's config survives"
        );
    }

    /// The owner's own deletions must still work end to end, and a brand new
    /// site must still be adoptable into the contract's default placeholder
    /// state.
    #[test]
    fn legitimate_deletion_and_first_publish_still_work() {
        let owner = gen_key();
        let params = make_params(&owner);

        let mut site = SiteState::new(SiteConfig::default(), &owner);
        let page = Page::new(1, "Home".into(), "content".into(), 1000, &owner);
        site.upsert_page(1, page, &owner.verifying_key()).unwrap();
        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site.delete_page(&deletion, &owner.verifying_key()).unwrap();
        site.verify(&params)
            .expect("owner-signed tombstone must validate");

        // A peer receiving this state for the first time starts from the
        // placeholder the contract builds when no state exists.
        let mut fresh = SiteState::default();
        assert!(fresh.is_uninitialized());
        fresh
            .merge(&params, &site)
            .expect("first publish must merge into the placeholder state");
        assert_eq!(fresh.owner, owner.verifying_key());
        assert!(fresh.deleted_pages.contains_key(&1));
        // Adopting the owner is not enough on its own: the result must pass
        // its OWN verify, or the peer stores something it will later reject.
        fresh
            .verify(&params)
            .expect("merged first-publish state must pass its own verify");
    }

    /// Field-wise merging into the placeholder silently keeps the placeholder's
    /// UNSIGNED config. `SignedConfig::default()` is version 1 with a 64-byte
    /// zero signature, a real newly created site is also version 1, and the
    /// config merge is strictly-greater — so `1 > 1` is false and the real
    /// owner-signed config never lands. The state then carries the right owner
    /// and a config signature that fails `SignedConfig::verify`.
    ///
    /// Reachable from `contracts/site-contract/src/lib.rs`, which builds
    /// `SiteState::default()` and merges into it whenever `update_state` runs
    /// against empty stored state.
    #[test]
    fn first_publish_adopts_the_owner_signed_config() {
        let owner = gen_key();
        let params = make_params(&owner);
        let site = SiteState::new(SiteConfig::default(), &owner);
        assert_eq!(
            site.config.config.version,
            SignedConfig::default().config.version,
            "premise: a real new site shares the placeholder's config version, \
             so a strictly-greater merge cannot replace it"
        );

        let mut fresh = SiteState::default();
        fresh
            .merge(&params, &site)
            .expect("first publish must merge");

        assert_eq!(
            fresh.config, site.config,
            "the owner-signed config must replace the unsigned placeholder"
        );
        fresh
            .verify(&params)
            .expect("merged first-publish state must pass its own verify");
    }

    /// Pins the OWNER half of `is_uninitialized`. A site that has a real owner
    /// but no pages and no tombstones (a freshly created, config-only site) is
    /// established, not unclaimed. Dropping the owner comparison would classify
    /// it as unclaimed and let a prefix-colliding impostor adopt the address.
    #[test]
    fn config_only_site_is_not_treated_as_unclaimed() {
        let owner = gen_key();
        let victim = SiteState::new(SiteConfig::default(), &owner);
        assert!(victim.pages.is_empty() && victim.deleted_pages.is_empty());
        assert!(
            !victim.is_uninitialized(),
            "a config-only site has a real owner and is not unclaimed"
        );

        let colliding = gen_key();
        let params = make_params(&colliding);
        let impostor = SiteState::new(
            SiteConfig {
                version: 99,
                ..SiteConfig::default()
            },
            &colliding,
        );
        impostor.verify(&params).expect("impostor clears verify");

        let mut victim = victim;
        let err = victim
            .merge(&params, &impostor)
            .expect_err("a config-only site must not be adoptable by another owner");
        assert!(err.contains("different owner"), "unexpected error: {err}");
        assert_eq!(victim.owner, owner.verifying_key(), "owner unchanged");
    }

    #[test]
    fn next_page_id_advances() {
        let owner = gen_key();
        let _params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let p1 = Page::new(1, "A".into(), "a".into(), 1000, &owner);
        site.upsert_page(1, p1, &owner.verifying_key()).unwrap();
        assert_eq!(site.next_page_id, 2);

        let p5 = Page::new(5, "B".into(), "b".into(), 2000, &owner);
        site.upsert_page(5, p5, &owner.verifying_key()).unwrap();
        assert_eq!(site.next_page_id, 6);
    }

    #[test]
    fn site_key_export_roundtrip() {
        let export = SiteKeyExport {
            signing_key: vec![1; 32],
            owner_pubkey: vec![2; 32],
            prefix: "abcdefghij".into(),
            name: "Test Site".into(),
        };
        let armored = export.to_armored();
        assert!(armored.starts_with(ARMOR_BEGIN));
        assert!(armored.ends_with(ARMOR_END));

        let parsed = SiteKeyExport::from_armored(&armored).unwrap();
        assert_eq!(parsed.signing_key, export.signing_key);
        assert_eq!(parsed.owner_pubkey, export.owner_pubkey);
        assert_eq!(parsed.prefix, export.prefix);
        assert_eq!(parsed.name, export.name);
    }

    #[test]
    fn site_key_export_handles_whitespace() {
        let export = SiteKeyExport {
            signing_key: vec![3; 32],
            owner_pubkey: vec![4; 32],
            prefix: "1234567890".into(),
            name: "My Site".into(),
        };
        let armored = export.to_armored();
        // Add extra whitespace
        let messy = format!("  \n{}\n  ", armored);
        let parsed = SiteKeyExport::from_armored(&messy).unwrap();
        assert_eq!(parsed.prefix, "1234567890");
    }

    #[test]
    fn is_site_owned_record_says_owner() {
        assert!(is_site_owned(true, "ABC", &[]));
    }

    #[test]
    fn is_site_owned_confirmed_by_public_key() {
        let confirmed = vec!["ABC".to_string(), "DEF".to_string()];
        assert!(is_site_owned(false, "ABC", &confirmed));
    }

    #[test]
    fn is_site_owned_neither() {
        let confirmed = vec!["DEF".to_string()];
        assert!(!is_site_owned(false, "ABC", &confirmed));
    }

    #[test]
    fn is_site_owned_both() {
        let confirmed = vec!["ABC".to_string()];
        assert!(is_site_owned(true, "ABC", &confirmed));
    }

    #[test]
    fn reorder_with_same_timestamp_is_dropped_by_apply_delta() {
        // Regression guard for the page-reorder bug Ivvor reported on
        // 2026-04-29: swapping two pages within the same wall-clock
        // second produces an UPDATE whose `updated_at` equals the
        // current state's `updated_at` for both pages. `apply_delta`
        // dominates equal timestamps, so the swap is silently dropped
        // on the network even though the local UI optimistically
        // applied it. The user sees the new order until reload, then
        // the network state (without the swap) wins.
        //
        // This test pins the contract behavior: if an UPDATE arrives
        // with `updated_at == existing.updated_at` and a different
        // `order`, the change MUST be rejected. The fix has to live in
        // the UI: every reorder must produce strictly greater
        // timestamps. See the matching `swap_page_order` test in
        // ui/src/state.rs.
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page_a = Page::new_with_order(1, "A".into(), "a".into(), 100, 10, &owner);
        let page_b = Page::new_with_order(2, "B".into(), "b".into(), 100, 20, &owner);
        site.upsert_page(1, page_a, &owner.verifying_key()).unwrap();
        site.upsert_page(2, page_b, &owner.verifying_key()).unwrap();

        // Same-second reorder: orders swapped, timestamp unchanged.
        let swap_a = Page::new_with_order(1, "A".into(), "a".into(), 100, 20, &owner);
        let swap_b = Page::new_with_order(2, "B".into(), "b".into(), 100, 10, &owner);
        let mut page_updates = BTreeMap::new();
        page_updates.insert(1, swap_a);
        page_updates.insert(2, swap_b);
        let delta = SiteStateDelta {
            config: None,
            page_updates,
            page_deletions: Vec::new(),
        };

        site.apply_delta(&delta, &params).unwrap();

        // Bug behavior pinned: equal timestamps are dominated, swap dropped.
        assert_eq!(site.pages[&1].order, 10, "swap was silently dropped");
        assert_eq!(site.pages[&2].order, 20, "swap was silently dropped");

        // Sanity: with a strictly greater timestamp, the swap applies.
        let swap_a = Page::new_with_order(1, "A".into(), "a".into(), 101, 20, &owner);
        let swap_b = Page::new_with_order(2, "B".into(), "b".into(), 101, 10, &owner);
        let mut page_updates = BTreeMap::new();
        page_updates.insert(1, swap_a);
        page_updates.insert(2, swap_b);
        let delta = SiteStateDelta {
            config: None,
            page_updates,
            page_deletions: Vec::new(),
        };
        site.apply_delta(&delta, &params).unwrap();
        assert_eq!(site.pages[&1].order, 20);
        assert_eq!(site.pages[&2].order, 10);
    }

    #[test]
    fn reorder_with_same_timestamp_is_dropped_by_merge() {
        // Same dominance bug as the apply_delta path, but exercised
        // through `merge` (used when a peer sends a full state via
        // `UpdateData::State` rather than a delta). Both code paths
        // use `>=` for tie-breaking, so the UI's strict-monotonicity
        // invariant is what makes either path actually replicate a
        // reorder. If `merge` ever drifted from `apply_delta` on
        // this rule, this test would fail.
        let owner = gen_key();
        let params = make_params(&owner);
        let mut local = SiteState::new(SiteConfig::default(), &owner);
        let mut peer = SiteState::new(SiteConfig::default(), &owner);

        let page_a = Page::new_with_order(1, "A".into(), "a".into(), 100, 10, &owner);
        let page_b = Page::new_with_order(2, "B".into(), "b".into(), 100, 20, &owner);
        local
            .upsert_page(1, page_a.clone(), &owner.verifying_key())
            .unwrap();
        local
            .upsert_page(2, page_b.clone(), &owner.verifying_key())
            .unwrap();

        // Peer has the swapped orders but the same timestamp.
        let swap_a = Page::new_with_order(1, "A".into(), "a".into(), 100, 20, &owner);
        let swap_b = Page::new_with_order(2, "B".into(), "b".into(), 100, 10, &owner);
        peer.upsert_page(1, swap_a, &owner.verifying_key()).unwrap();
        peer.upsert_page(2, swap_b, &owner.verifying_key()).unwrap();

        local.merge(&params, &peer).unwrap();
        assert_eq!(local.pages[&1].order, 10, "merge dropped same-ts swap");
        assert_eq!(local.pages[&2].order, 20, "merge dropped same-ts swap");

        // With +1 the merge does take.
        let swap_a = Page::new_with_order(1, "A".into(), "a".into(), 101, 20, &owner);
        let swap_b = Page::new_with_order(2, "B".into(), "b".into(), 101, 10, &owner);
        let mut peer2 = SiteState::new(SiteConfig::default(), &owner);
        peer2
            .upsert_page(1, swap_a, &owner.verifying_key())
            .unwrap();
        peer2
            .upsert_page(2, swap_b, &owner.verifying_key())
            .unwrap();
        local.merge(&params, &peer2).unwrap();
        assert_eq!(local.pages[&1].order, 20);
        assert_eq!(local.pages[&2].order, 10);
    }

    #[test]
    fn page_order_is_signed() {
        let owner = gen_key();
        let page = Page::new_with_order(1, "Title".into(), "Content".into(), 100, 5, &owner);
        // Verify with correct order succeeds
        assert!(page.verify(1, &owner.verifying_key()).is_ok());
        // Tamper with order -- verification must fail
        let mut tampered = page.clone();
        tampered.order = 10;
        assert!(tampered.verify(1, &owner.verifying_key()).is_err());
    }

    #[test]
    fn known_site_record_tombstone_roundtrip() {
        // The tombstone sentinel must survive a CBOR roundtrip through the
        // existing KnownSiteRecord schema so that no delegate WASM change
        // is required to persist removed-site tombstones.
        let real = KnownSiteRecord {
            prefix: "abcdef1234".into(),
            name: "My Site".into(),
            is_owner: true,
            contract_key_b58: Some("ck".into()),
        };
        let tomb = KnownSiteRecord::tombstone("xyz9876543");
        assert!(!real.is_tombstone());
        assert!(tomb.is_tombstone());

        let records = vec![real.clone(), tomb.clone()];
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&records, &mut buf).unwrap();
        let decoded: Vec<KnownSiteRecord> = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(decoded.len(), 2);
        assert!(!decoded[0].is_tombstone());
        assert_eq!(decoded[0].prefix, "abcdef1234");
        assert!(decoded[1].is_tombstone());
        assert_eq!(decoded[1].prefix, "xyz9876543");

        // A record whose name happens to start with NUL but differs must
        // NOT be treated as a tombstone.
        let fake = KnownSiteRecord {
            prefix: "p".into(),
            name: "\u{0000}not_removed".into(),
            is_owner: false,
            contract_key_b58: None,
        };
        assert!(!fake.is_tombstone());
    }

    #[test]
    fn get_signing_key_for_prefix_roundtrips() {
        // Regression guard: export must be able to ask the delegate for the
        // per-site signing key, not the legacy single-key slot. If this
        // variant is dropped or its prefix field renamed, export silently
        // falls back to the legacy slot and produces tokens whose
        // signing_key does not match the site's owner_pubkey — edits on
        // the importing node then fail contract signature validation.
        let req = DelegateRequest::GetSigningKeyForPrefix {
            prefix: "abcdef1234".into(),
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&req, &mut buf).unwrap();
        let decoded: DelegateRequest = ciborium::de::from_reader(buf.as_slice()).unwrap();
        match decoded {
            DelegateRequest::GetSigningKeyForPrefix { prefix } => {
                assert_eq!(prefix, "abcdef1234");
            }
            other => panic!("expected GetSigningKeyForPrefix, got {other:?}"),
        }
    }

    #[test]
    fn get_signing_key_stays_wire_compatible_with_pre_v7_delegates() {
        // CBOR wire-format compatibility check: `GetSigningKey` must remain
        // a unit variant so the legacy-migration path can probe pre-V7
        // delegates for signing keys stranded in the legacy single-key
        // slot. Changing this variant to a struct variant would break
        // deserialization on pre-V7 delegates and silently kill key
        // migration.
        //
        // ciborium encodes externally-tagged unit variants as a bare
        // string containing the variant name. Assert exactly that.
        let req = DelegateRequest::GetSigningKey;
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&req, &mut buf).unwrap();
        let as_value: ciborium::Value = ciborium::de::from_reader(buf.as_slice()).unwrap();
        match as_value {
            ciborium::Value::Text(s) => assert_eq!(s, "GetSigningKey"),
            other => panic!(
                "GetSigningKey must serialize as a bare string (unit variant) \
                 for legacy-delegate wire compat, got: {other:?}"
            ),
        }
    }

    #[test]
    fn old_pages_without_order_still_verify() {
        let owner = gen_key();
        // Simulate a v1 page (signed without order)
        let bytes = page_signing_bytes_v1(1, "Title", "Content", 100);
        let sig = sign_bytes(&bytes, &owner);
        let page = Page {
            title: "Title".into(),
            content: "Content".into(),
            updated_at: 100,
            signature: sig,
            order: 0,
        };
        // Should pass via v1 fallback
        assert!(page.verify(1, &owner.verifying_key()).is_ok());
    }
}
