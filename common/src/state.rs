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

        // Pages the peer's summary still reports holding that we've since
        // deleted: forward the already-signed tombstone. We already have it
        // in `deleted_pages` (signed at delete time), so there is nothing
        // "retroactive" about it. Skipping this made a pending deletion
        // relative to a specific peer indistinguishable from genuine
        // convergence (delta#43): freenet-core's InterestSync backstop reads
        // an EMPTY result from this method as "converged, nothing to send"
        // and SUPPRESSES the heal it would otherwise trigger. Without this
        // filter, a peer that missed the live delete broadcast has no other
        // path to learn about the deletion: this method staying (wrongly)
        // empty is exactly what would have suppressed the resync that could
        // have told it.
        let page_deletions: Vec<SignedPageDeletion> = self
            .deleted_pages
            .iter()
            .filter(|(id, _)| summary.pages.contains_key(id))
            .map(|(_, deletion)| deletion.clone())
            .collect();

        if config.is_none() && page_updates.is_empty() && page_deletions.is_empty() {
            None
        } else {
            Some(SiteStateDelta {
                config,
                page_updates,
                page_deletions,
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
        // A delta has no owner field of its own to establish or verify
        // against, unlike a full state (which `merge` can adopt wholesale
        // once it has cleared `other.verify(params)` against a real owner).
        // Applying a delta to an uninitialized (placeholder-owner) state
        // would verify every signature against `placeholder_owner()`, which
        // is not self-protecting: `is_uninitialized`'s rustdoc already
        // documents that ed25519-dalek's non-strict `verify` doesn't reject
        // the placeholder's weak (order-4) key, so a signature under it is
        // forgeable without any private key. Refuse outright rather than
        // silently "verifying" against a key nobody controls; first capture
        // for an unclaimed address happens via `merge`, not `apply_delta`.
        if self.is_uninitialized() {
            return Err("cannot apply a delta to an uninitialized site".to_string());
        }

        let owner = self.owner;

        if let Some(new_config) = &delta.config {
            new_config.verify(&owner)?;
            if new_config.config.version > self.config.config.version {
                self.config = new_config.clone();
            }
        }

        for (&page_id, page) in &delta.page_updates {
            // #18: don't resurrect a page we've already tombstoned. Without
            // this, a delta that arrives before its sender learns about our
            // deletion (this round's summary predates it) re-creates the
            // page here, and if the sender is ALSO stale relative to us in
            // the same round, the next round flips the roles and oscillates
            // forever instead of converging. `merge` already has the
            // equivalent check for full-state application; this is the
            // matching guard for delta application. See
            // `deletion_converges_under_simultaneous_bidirectional_exchange`.
            if self.deleted_pages.contains_key(&page_id) {
                continue;
            }
            let dominated = self
                .pages
                .get(&page_id)
                .is_some_and(|existing| existing.updated_at >= page.updated_at);
            if !dominated {
                self.upsert_page(page_id, page.clone(), &owner)?;
            }
        }

        // Deletions are applied after updates. The #18 guard above is what
        // actually guarantees deletion wins as an observable property
        // across an exchange (see
        // `deletion_converges_under_simultaneous_bidirectional_exchange`):
        // once a tombstone exists and gets communicated, every later update
        // for that id is rejected on arrival. This ordering only covers a
        // narrower, single-call case the guard above can't see yet: if ONE
        // delta carries both a page_update and a page_deletion for the same
        // id, applying updates first makes that mixed delta resolve to
        // deletion-wins too, instead of depending on iteration order.
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

    /// delta#43 follow-up: a peer whose summary still lists a page we've
    /// since deleted must get a non-empty delta carrying the tombstone.
    /// Without this, `compute_delta` only diffs `pages` and never looks at
    /// `deleted_pages`, so a pending deletion silently produces the SAME
    /// `None` as genuine convergence — which is exactly what a byte-empty
    /// `get_state_delta` (delta#43) reports to freenet-core's InterestSync
    /// staleness backstop as "converged despite differing summary bytes",
    /// permanently suppressing the one heal that would have delivered the
    /// tombstone to a peer that missed the live delete broadcast.
    /// Review follow-up (F1): `apply_delta` verifies signatures against
    /// `self.owner`, but a `SiteState::default()` (uninitialized) state has
    /// `owner == placeholder_owner()` — a `[0u8; 32]` key that decompresses
    /// to a weak (order-4) point ed25519-dalek's non-strict `verify` does
    /// not reject. A zero signature verifies for a meaningful fraction of
    /// messages against it, so a delta could be forged against any site
    /// whose local state happens to still be the placeholder (e.g. a
    /// `KnownSite` installed with `state: SiteState::default()` before its
    /// real content ever arrived). This grinds `deleted_at` for a
    /// zero-signature `SignedPageDeletion` that verifies under the
    /// placeholder key (mirroring how the forgery was found in review),
    /// then confirms `apply_delta` refuses it outright rather than
    /// "verifying" a signature nobody's private key produced.
    #[test]
    fn apply_delta_rejects_a_forged_delta_against_an_uninitialized_state() {
        let mut site = SiteState::default();
        assert!(site.is_uninitialized());
        let params = SiteParameters {
            prefix: pubkey_to_prefix(&placeholder_owner()),
        };

        let zero_sig = Signature::from_bytes(&[0u8; 64]);
        let forged_deletion = (0u64..10_000)
            .find_map(|deleted_at| {
                let bytes = deletion_signing_bytes(1, deleted_at);
                placeholder_owner()
                    .verify(&bytes, &zero_sig)
                    .ok()
                    .map(|()| SignedPageDeletion {
                        page_id: 1,
                        deleted_at,
                        signature: zero_sig,
                    })
            })
            .expect(
                "a zero-signature deleted_at that verifies under the placeholder \
                 key should exist well within 10_000 tries (review found one in 4)",
            );

        let delta = SiteStateDelta {
            config: None,
            page_updates: BTreeMap::new(),
            page_deletions: vec![forged_deletion],
        };

        let result = site.apply_delta(&delta, &params);
        assert!(
            result.is_err(),
            "a delta must never apply to an uninitialized (placeholder-owner) state"
        );
        assert!(site.pages.is_empty());
        assert!(
            site.deleted_pages.is_empty(),
            "the forged tombstone must not be recorded"
        );
    }

    #[test]
    fn deletion_pending_relative_to_peer_summary_is_not_an_empty_delta() {
        let owner = gen_key();
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Home".into(), "# Hello".into(), 1000, &owner);
        site.upsert_page(1, page, &owner.verifying_key()).unwrap();

        // A peer's summary from before the deletion: it still reports
        // holding page 1.
        let peer_summary = site.summarize();

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site.delete_page(&deletion, &owner.verifying_key()).unwrap();

        let delta = site
            .compute_delta(&peer_summary)
            .expect("a pending deletion the peer doesn't know about must not be None");

        assert_eq!(
            delta.page_deletions.len(),
            1,
            "delta should carry the tombstone for the page the peer still reports holding"
        );
        assert_eq!(delta.page_deletions[0].page_id, 1);
        assert!(delta.page_updates.is_empty());
    }

    #[test]
    fn deletion_delta_heals_a_peer_with_the_stale_page() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Home".into(), "# Hello".into(), 1000, &owner);
        site.upsert_page(1, page.clone(), &owner.verifying_key())
            .unwrap();

        // A peer that received the page before the deletion (e.g. missed the
        // live delete broadcast) and is still serving the stale copy.
        let mut peer = SiteState::new(SiteConfig::default(), &owner);
        peer.upsert_page(1, page, &owner.verifying_key()).unwrap();
        let peer_summary = peer.summarize();

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site.delete_page(&deletion, &owner.verifying_key()).unwrap();

        let delta = site
            .compute_delta(&peer_summary)
            .expect("pending deletion must produce a delta");
        peer.apply_delta(&delta, &params).unwrap();

        assert!(
            peer.pages.is_empty(),
            "peer should have dropped the deleted page"
        );
        assert!(
            peer.deleted_pages.contains_key(&1),
            "peer should have adopted the tombstone"
        );

        // Once both sides agree, the delta against the healed peer's own
        // (now tombstone-consistent) summary is genuinely empty again — not
        // because deletions are invisible to compute_delta, but because
        // there is truly nothing left to communicate.
        let converged_summary = peer.summarize();
        assert!(site.compute_delta(&converged_summary).is_none());
    }

    /// Review follow-up: the #18 guard in `apply_delta` (`if
    /// self.deleted_pages.contains_key(&page_id) { continue; }`) checks the
    /// SPECIFIC id being applied, not merely "do I have any tombstone at
    /// all". Neither convergence test can distinguish those two readings,
    /// since neither delivers a live page alongside an unrelated tombstone.
    /// A peer holding a tombstone for page 1 must still accept an unrelated
    /// update to page 2.
    #[test]
    fn apply_delta_delivers_unrelated_pages_when_a_tombstone_exists() {
        let owner = gen_key();
        let params = make_params(&owner);
        let mut peer = SiteState::new(SiteConfig::default(), &owner);

        let page1 = Page::new(1, "One".into(), "# One".into(), 1000, &owner);
        let page2 = Page::new(2, "Two".into(), "# Two".into(), 1000, &owner);
        peer.upsert_page(1, page1, &owner.verifying_key()).unwrap();
        peer.upsert_page(2, page2, &owner.verifying_key()).unwrap();

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        peer.delete_page(&deletion, &owner.verifying_key()).unwrap();

        // A delta with a newer page 2, no mention of page 1 at all.
        let page2_v2 = Page::new(2, "Two".into(), "# Two v2".into(), 2000, &owner);
        let mut page_updates = BTreeMap::new();
        page_updates.insert(2, page2_v2);
        let delta = SiteStateDelta {
            config: None,
            page_updates,
            page_deletions: Vec::new(),
        };

        peer.apply_delta(&delta, &params).unwrap();

        assert_eq!(
            peer.pages[&2].content, "# Two v2",
            "an unrelated tombstone must not block an update to a different page"
        );
        assert!(!peer.pages.contains_key(&1), "page 1 must stay deleted");
    }

    /// Review follow-up: the `summary.pages.contains_key(id)` filter in
    /// `compute_delta` checks the SPECIFIC id, not merely "does the peer's
    /// summary have any pages at all". Deletes two pages, but the peer's
    /// summary only still lists one of them (it already learned about the
    /// other's deletion, or never held it) — only that one tombstone should
    /// be forwarded, not every locally-known deletion.
    #[test]
    fn compute_delta_emits_only_tombstones_the_peer_still_holds() {
        let owner = gen_key();
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page1 = Page::new(1, "One".into(), "# One".into(), 1000, &owner);
        let page2 = Page::new(2, "Two".into(), "# Two".into(), 1000, &owner);
        site.upsert_page(1, page1, &owner.verifying_key()).unwrap();
        site.upsert_page(2, page2, &owner.verifying_key()).unwrap();

        let deletion1 = SignedPageDeletion::new(1, 2000, &owner);
        site.delete_page(&deletion1, &owner.verifying_key())
            .unwrap();
        let deletion2 = SignedPageDeletion::new(2, 2000, &owner);
        site.delete_page(&deletion2, &owner.verifying_key())
            .unwrap();

        let mut peer_summary = SiteStateSummary::default();
        peer_summary.pages.insert(1, (blake3::hash(b"# One"), 1000));

        let delta = site
            .compute_delta(&peer_summary)
            .expect("should have a delta for page 1's tombstone");
        let ids: Vec<PageId> = delta.page_deletions.iter().map(|d| d.page_id).collect();
        assert_eq!(
            ids,
            vec![1],
            "only the tombstone the peer's summary still reflects should be sent"
        );
    }

    /// Review follow-up on delta#43: `compute_delta` populating
    /// `page_deletions` makes `apply_delta`'s deletion loop reachable via
    /// real network sync for the first time (previously `compute_delta`
    /// never emitted one, so only a hand-built delta could exercise it).
    /// `delete_page`'s `deleted_pages.insert` is last-write-wins, unlike
    /// `merge`'s `or_insert_with` (first-write-wins) — see the analogous,
    /// already-pinned `conflicting_tombstone_deleted_at_is_order_dependent_but_page_stays_deleted`
    /// in `ui/src/freenet_api/operations.rs` for the `merge` path. Both are
    /// order-dependent in which `deleted_at`/signature ends up recorded, but
    /// harmless: the page stays deleted either way. Pinning the `apply_delta`
    /// side explicitly now that it is reachable, not changing the behavior.
    #[test]
    fn apply_delta_conflicting_tombstone_is_order_dependent_but_page_stays_deleted() {
        let owner = gen_key();
        let params = make_params(&owner);
        let page = Page::new(1, "Home".into(), "# Hello".into(), 1000, &owner);

        // Order A: local tombstone recorded with the SMALLER value (1000)
        // first, incoming delta carries the LARGER one (2000).
        let mut peer_a = SiteState::new(SiteConfig::default(), &owner);
        peer_a
            .upsert_page(1, page.clone(), &owner.verifying_key())
            .unwrap();
        peer_a
            .delete_page(
                &SignedPageDeletion::new(1, 1000, &owner),
                &owner.verifying_key(),
            )
            .unwrap();
        let delta_a = SiteStateDelta {
            config: None,
            page_updates: BTreeMap::new(),
            page_deletions: vec![SignedPageDeletion::new(1, 2000, &owner)],
        };
        peer_a.apply_delta(&delta_a, &params).unwrap();

        // Order B: the SAME two values, but paired the OTHER way around
        // (local gets the LARGER value first, incoming carries the
        // SMALLER one). This is what actually proves order-dependence
        // (last-applied-wins) rather than a max-wins implementation that
        // would happen to agree with order A's result alone.
        let mut peer_b = SiteState::new(SiteConfig::default(), &owner);
        peer_b.upsert_page(1, page, &owner.verifying_key()).unwrap();
        peer_b
            .delete_page(
                &SignedPageDeletion::new(1, 2000, &owner),
                &owner.verifying_key(),
            )
            .unwrap();
        let delta_b = SiteStateDelta {
            config: None,
            page_updates: BTreeMap::new(),
            page_deletions: vec![SignedPageDeletion::new(1, 1000, &owner)],
        };
        peer_b.apply_delta(&delta_b, &params).unwrap();

        // Page stays deleted in BOTH orders (the harmless part).
        assert!(peer_a.pages.is_empty());
        assert!(peer_b.pages.is_empty());

        assert_eq!(
            peer_a.deleted_pages[&1].deleted_at, 2000,
            "apply_delta overwrites with the incoming tombstone"
        );
        assert_eq!(
            peer_b.deleted_pages[&1].deleted_at, 1000,
            "apply_delta overwrites with the incoming tombstone even when it is \
             SMALLER, ruling out max-wins as an alternative explanation"
        );
        assert_ne!(
            peer_a.deleted_pages[&1].deleted_at, peer_b.deleted_pages[&1].deleted_at,
            "conflicting-tombstone deleted_at is order-dependent (documented, harmless)"
        );
    }

    /// A delta can legitimately carry both a config bump and a pending
    /// deletion in the same message. NOTE: this does NOT pin the `None`
    /// gate's three-field requirement — with `config` already `Some`, the
    /// `config.is_none() && ...` check short-circuits to `false` regardless
    /// of `page_deletions`, so this test would pass identically even if the
    /// `page_deletions.is_empty()` term were removed from the gate entirely.
    /// The gate's `page_deletions` term is actually pinned by
    /// `deletion_pending_relative_to_peer_summary_is_not_an_empty_delta`
    /// (config `None`, page_updates empty, page_deletions non-empty still
    /// forces `Some`). This test is a combinatorial sanity check that the
    /// two fields coexist correctly, not a gate-necessity proof.
    #[test]
    fn delta_carries_both_a_config_change_and_a_pending_deletion() {
        let owner = gen_key();
        let mut site = SiteState::new(SiteConfig::default(), &owner);

        let page = Page::new(1, "Home".into(), "# Hello".into(), 1000, &owner);
        site.upsert_page(1, page, &owner.verifying_key()).unwrap();

        // Peer's summary predates both the deletion and the config bump.
        let peer_summary = site.summarize();

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site.delete_page(&deletion, &owner.verifying_key()).unwrap();
        site.config = SignedConfig::new(
            SiteConfig {
                version: 2,
                name: "Renamed".into(),
                description: String::new(),
            },
            &owner,
        );

        let delta = site
            .compute_delta(&peer_summary)
            .expect("config bump plus pending deletion must not be None");

        assert!(delta.config.is_some(), "delta should carry the config bump");
        assert_eq!(delta.page_deletions.len(), 1);
        assert!(delta.page_updates.is_empty());
    }

    /// Review follow-up (#18 interaction): `compute_delta` now emits
    /// tombstones, but `apply_delta`'s page-update loop upserts without
    /// consulting `deleted_pages` (unlike `merge`, which skips a tombstoned
    /// id). Under a SIMULTANEOUS bidirectional exchange, that combination
    /// oscillates instead of converging: whichever peer currently holds the
    /// page sends it as a page_update (its delta is computed against the
    /// OTHER peer's summary from BEFORE this round, so it doesn't yet know
    /// about a same-round deletion), while the peer holding the tombstone
    /// sends the deletion; each side's incoming page_update resurrects the
    /// page locally, feeding the same shape into the next round.
    ///
    /// This is specifically a CONCURRENT-exchange bug: an alternating
    /// (ping-pong) exchange converges fine, because each side's compute_delta
    /// always runs against the OTHER's most recent summary. The test
    /// therefore snapshots BOTH summaries before applying EITHER delta each
    /// round, matching how a real heartbeat round works (every peer's
    /// summary at the start of the round is what every other peer computes
    /// against) — computing serially (compute A, apply A, compute B against
    /// A's now-updated state, apply B) would silently degrade this into the
    /// alternating case and pass regardless of the bug.
    #[test]
    fn deletion_converges_under_simultaneous_bidirectional_exchange() {
        let owner = gen_key();
        let params = make_params(&owner);

        let mut a = SiteState::new(SiteConfig::default(), &owner);
        let page = Page::new(1, "Home".into(), "# Hello".into(), 1000, &owner);
        a.upsert_page(1, page.clone(), &owner.verifying_key())
            .unwrap();

        // b is a-before-the-deletion: it still has page 1 and doesn't know
        // a deleted it.
        let mut b = SiteState::new(SiteConfig::default(), &owner);
        b.upsert_page(1, page, &owner.verifying_key()).unwrap();

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        a.delete_page(&deletion, &owner.verifying_key()).unwrap();

        const MAX_ROUNDS: usize = 10;
        for round in 0..MAX_ROUNDS {
            // Snapshot BOTH summaries before applying either delta — see the
            // doc comment above for why this matters.
            let a_summary = a.summarize();
            let b_summary = b.summarize();

            let delta_to_b = a.compute_delta(&b_summary);
            let delta_to_a = b.compute_delta(&a_summary);

            if delta_to_b.is_none() && delta_to_a.is_none() {
                assert!(a.pages.is_empty(), "a should have converged with no page 1");
                assert!(b.pages.is_empty(), "b should have converged with no page 1");
                assert!(
                    b.deleted_pages.contains_key(&1),
                    "the deletion must win, not the page"
                );
                return;
            }

            if let Some(d) = delta_to_b {
                b.apply_delta(&d, &params).unwrap();
            }
            if let Some(d) = delta_to_a {
                a.apply_delta(&d, &params).unwrap();
            }

            if round == MAX_ROUNDS - 1 {
                panic!(
                    "no fixpoint after {MAX_ROUNDS} rounds of simultaneous exchange; \
                     a.pages={:?} b.pages={:?}",
                    a.pages.keys().collect::<Vec<_>>(),
                    b.pages.keys().collect::<Vec<_>>()
                );
            }
        }
    }

    /// Second-lens follow-up: the oscillation above isn't limited to a
    /// stale peer that missed a delete broadcast. The SAME owner deleting a
    /// DIFFERENT page on each of two devices (e.g. offline edits on a phone
    /// and a laptop) hits it too: `a` ends up with `pages=[2] tomb=[1]`,
    /// `b` with `pages=[1] tomb=[2]`, and without the guard each side's
    /// page_update for its own surviving page resurrects the other's
    /// deleted page, forever. Same guard, same fixpoint check.
    #[test]
    fn concurrent_deletes_of_different_pages_converge() {
        let owner = gen_key();
        let params = make_params(&owner);

        let mut a = SiteState::new(SiteConfig::default(), &owner);
        let page1 = Page::new(1, "One".into(), "# One".into(), 1000, &owner);
        let page2 = Page::new(2, "Two".into(), "# Two".into(), 1000, &owner);
        a.upsert_page(1, page1.clone(), &owner.verifying_key())
            .unwrap();
        a.upsert_page(2, page2.clone(), &owner.verifying_key())
            .unwrap();

        // b starts as a's twin (same two pages, e.g. synced before either
        // device went offline).
        let mut b = SiteState::new(SiteConfig::default(), &owner);
        b.upsert_page(1, page1, &owner.verifying_key()).unwrap();
        b.upsert_page(2, page2, &owner.verifying_key()).unwrap();

        // a deletes page 1 (offline); b deletes page 2 (offline, on a
        // different device), neither aware of the other's edit.
        let a_deletion = SignedPageDeletion::new(1, 2000, &owner);
        a.delete_page(&a_deletion, &owner.verifying_key()).unwrap();
        let b_deletion = SignedPageDeletion::new(2, 2000, &owner);
        b.delete_page(&b_deletion, &owner.verifying_key()).unwrap();

        const MAX_ROUNDS: usize = 10;
        for round in 0..MAX_ROUNDS {
            let a_summary = a.summarize();
            let b_summary = b.summarize();

            let delta_to_b = a.compute_delta(&b_summary);
            let delta_to_a = b.compute_delta(&a_summary);

            if delta_to_b.is_none() && delta_to_a.is_none() {
                assert!(
                    !a.pages.contains_key(&1) && !a.pages.contains_key(&2),
                    "a should have converged with both pages gone"
                );
                assert!(
                    !b.pages.contains_key(&1) && !b.pages.contains_key(&2),
                    "b should have converged with both pages gone"
                );
                assert!(a.deleted_pages.contains_key(&1));
                assert!(a.deleted_pages.contains_key(&2));
                assert!(b.deleted_pages.contains_key(&1));
                assert!(b.deleted_pages.contains_key(&2));
                return;
            }

            if let Some(d) = delta_to_b {
                b.apply_delta(&d, &params).unwrap();
            }
            if let Some(d) = delta_to_a {
                a.apply_delta(&d, &params).unwrap();
            }

            if round == MAX_ROUNDS - 1 {
                panic!(
                    "no fixpoint after {MAX_ROUNDS} rounds; a.pages={:?} b.pages={:?}",
                    a.pages.keys().collect::<Vec<_>>(),
                    b.pages.keys().collect::<Vec<_>>()
                );
            }
        }
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
