//! Delegate secret migration on `freenet-migrate` 0.5.0.
//!
//! A delegate's address is `BLAKE3(BLAKE3(wasm) ‖ params)`, so any rebuild
//! re-keys it and the successor starts with an EMPTY secret store. Delta's
//! delegate holds each site's Ed25519 **signing key**; losing it means
//! permanently losing write authority over that site. Delta has hand-rolled the
//! recovery sweep in [`super::delegate`] since V2; this module moves the
//! decision-making onto the shared, reviewed, property-tested library so the
//! walk order, marker bookkeeping, withholding and per-predecessor
//! classification live in one place instead of being re-derived per app.
//!
//! # This is a UI-side adoption — the delegate WASM does NOT change
//!
//! The delegate WASM is byte-identical before and after this module was added,
//! and that is load-bearing: changing it would re-key the delegate and create
//! exactly the data-loss event the module exists to prevent. It is possible
//! because Delta's existing delegate protocol already has both halves of the
//! writer seam — `StoreSigningKey { key_bytes, prefix }` and
//! `StoreKnownSites { sites }` — so the successor's own import path is
//! reachable entirely from the client.
//!
//! Concretely, nothing here lives in `delegates/site-delegate`, and the module
//! is not reachable from it. `delegate_migration_is_ui_side_only` pins that the
//! secret-key names this module writes are exactly the ones the delegate
//! already understands, so a future edit cannot quietly require a new delegate
//! request variant (which would re-key the WASM).
//!
//! # Why the app supplies the writer (0.5.0's breaking change)
//!
//! 0.5.0 replaced the raw `(key, value)` pair copier with
//! [`SuccessorSecretsIo`], because a pair copy is wrong for any app whose
//! stored entries have cross-entry invariants. **Delta is exactly such an app:**
//! `delta:known_sites` is a single entry holding the WHOLE site list, and
//! `StoreKnownSites` REPLACES it. A pair copier would either skip it
//! never-clobber (stranding every site that only exists on the predecessor) or
//! write it verbatim (destroying every site the user added on the successor).
//! Neither is acceptable, and no `SecretSelectionPolicy` setting fixes it —
//! which is the whole reason the writer seam exists. [`merge_known_sites`] does
//! the read-merge-write instead.
//!
//! # The four hard constraints this adapter is built around
//!
//! 1. **Never-clobber.** The library's driver does NOT enforce newest-wins;
//!    under [`SecretSelectionPolicy::UnionAllGenerations`] that guarantee rests
//!    ENTIRELY on this writer declining a key the successor already holds. An
//!    overwrite-semantics writer ends up installing the OLDEST generation's
//!    value and reporting cleanly. Every arm of [`DeltaSuccessorIo::write_secret`]
//!    therefore reads the successor first and returns
//!    [`ItemWrite::AlreadyAuthoritative`] when it already has a value.
//! 2. **`StoreKnownSites` replaces the whole list**, so it gets a read-merge-write
//!    (see [`merge_known_sites`]).
//! 3. **Markers must be durable when `record_marker` returns**, not batched with
//!    items — a writer that routes markers through the same flush loses the
//!    sticky-data flag on failure. [`MigrationMarkerStore`] is a separate,
//!    synchronous-on-return path for that reason. See the honest bound on
//!    marker durability in this module's `MigrationMarkerStore` docs.
//! 4. **[`SecretSelectionPolicy::UnionAllGenerations`]**, because the default
//!    halts the walk at the first silent predecessor and silence is the ORDINARY
//!    case for Delta's old delegates (a V6/V7 delegate cannot answer
//!    `GetSigningKeyForPrefix` at all).
//!
//! # Honest bounds (do not design as though these are covered)
//!
//! * **Withholding is run-scoped** (freenet-migrate#15). A transiently
//!   unreachable NEWEST predecessor on a later run can still let an older
//!   generation permanently seal its value over the newest one, with a report
//!   that reads clean. Nothing in this module fixes that; it is acknowledged by
//!   the [`UnionAck`] we construct.
//! * **`imported_total()` under-reports** (freenet-migrate#16), so it is never
//!   rendered to the user here.
//! * **Union resurrects delete-by-absence data.** For Delta this is bounded, not
//!   absent: removals since delegate V3 are TOMBSTONE records rather than
//!   absences, and [`merge_known_sites`] honours them, so a union cannot
//!   resurrect a site removed in the tombstone era. A site removed under V1/V2
//!   (pre-tombstone) can still come back. That is the same pre-existing residual
//!   the contract half carries for its oldest generation.

use freenet_migrate::{
    DelegateLineageEntry, DelegateMigrationReport, ItemWrite, MarkerQuery, MigrationAuthorization,
    MigrationMarker, PredecessorSecretsIo, RecoveredSecret, SecretPair, SecretSelectionPolicy,
    SuccessorSecretsIo, UnionAck,
};
use freenet_stdlib::prelude::DelegateKey;
use std::collections::BTreeMap;
use std::future::Future;

use delta_core::{DelegateRequest, DelegateResponse, KnownSiteRecord};

/// The delegate's legacy single-slot signing key.
///
/// Must match `LEGACY_SIGNING_KEY` in `delegates/site-delegate/src/lib.rs`.
pub const SECRET_SIGNING_KEY_LEGACY: &str = "delta:signing_key";
/// Prefix of the delegate's per-site signing-key slots.
///
/// Must match `signing_key_for` in `delegates/site-delegate/src/lib.rs`.
pub const SECRET_SIGNING_KEY_PREFIX: &str = "delta:signing_key:";
/// The delegate's known-sites list. A SINGLE entry holding the whole list.
///
/// Must match `KNOWN_SITES_STORAGE_KEY` in `delegates/site-delegate/src/lib.rs`.
pub const SECRET_KNOWN_SITES: &str = "delta:known_sites";
/// Prefix of the delegate's per-site state backups.
pub const SECRET_SITE_STATE_PREFIX: &str = "delta:site_state:";

// ---------------------------------------------------------------------------
// Transport seam
// ---------------------------------------------------------------------------

/// One request/response round-trip against a specific delegate.
///
/// Delta's production transport is fire-and-forget (`send_to_delegate_key`
/// pushes a `ClientRequest` and the reply lands later in the global
/// [`super::delegate::handle_delegate_response`]), so the wasm implementation
/// correlates replies by `(delegate_key, response-kind)`. Abstracting it here is
/// what lets the whole adapter — and the differential against the shipped sweep
/// — run natively in unit tests rather than only in a browser.
///
/// # Contract
///
/// * `Ok(Some(response))` — the delegate EXECUTED and replied.
///   [`DelegateResponse::Error`] is such a reply: an old delegate answering
///   "no signing key stored" has demonstrably executed, which is precisely the
///   distinction [`PredecessorSecretsIo::probe_executable`] needs.
/// * `Ok(None)` — no reply within the implementation's bound. The delegate could
///   not execute, is not registered on this node, or the request was lost.
/// * `Err(_)` — the transport itself is broken; aborts the migration.
pub trait DeltaDelegateChannel {
    /// Transport error type.
    type Error: core::fmt::Debug;

    /// Send `request` to `target` and await its reply.
    ///
    /// Takes `&self` deliberately. [`migrate_delegate_secrets`] holds the
    /// predecessor reader and the successor writer as two simultaneous `&mut`
    /// borrows, and BOTH need the transport, so a `&mut self` method here cannot
    /// be shared between them. Implementations use interior mutability (the wasm
    /// one is a thread-local pending-reply registry, which needs no `&mut`
    /// anyway). Implementations MUST NOT hold an internal borrow across the
    /// returned future's awaits.
    ///
    /// [`migrate_delegate_secrets`]: freenet_migrate::migrate_delegate_secrets
    fn request(
        &self,
        target: &DelegateKey,
        request: DelegateRequest,
    ) -> impl Future<Output = Result<Option<DelegateResponse>, Self::Error>>;
}

/// Durable storage for the library's per-predecessor migration markers.
///
/// # Durability is the whole point of this being separate
///
/// [`SuccessorSecretsIo::record_marker`] requires the marker to be durable WHEN
/// IT RETURNS, not merely queued. Routing markers through the same path as items
/// loses the `InProgress` marker exactly when a batch fails, which drops the
/// sticky-data flag: a retry then computes `saw_data` from that run alone, and a
/// predecessor that answers empty on the retry seals `Done { had_data: false }`,
/// misclassifying a data-bearing generation as `NoData`.
///
/// # Honest bound on where Delta can put them
///
/// Delta's delegate has NO generic secret-set request — it exposes only
/// `StoreSigningKey` / `StoreKnownSites` / `StoreSiteState`. Adding one would
/// re-key the delegate WASM, which is the exact event being migrated away from.
/// So markers cannot live in the delegate, and they go to `localStorage`
/// instead. In the production gateway iframe `localStorage` is unavailable (an
/// opaque origin makes `window.localStorage` throw), so there the markers are
/// PAGE-LIFETIME only.
///
/// The consequence is bounded, and it is a re-run cost rather than a correctness
/// one: with no durable `Done` marker the migration re-walks its predecessors on
/// the next page load. That is safe because every write is never-clobber and
/// idempotent, so a re-walk writes nothing new. What a durable marker would
/// additionally buy is anti-resurrection ("a completed `Done` is never
/// re-imported, so a stray re-run cannot resurrect secrets the user deleted
/// afterwards") — and for Delta that job is already done by the TOMBSTONE
/// convention, which is durable in the delegate itself. See
/// `AGENTS.md` "Known-Sites Tombstone Convention".
pub trait MigrationMarkerStore {
    /// Marker-store error type.
    type Error: core::fmt::Debug;

    /// Read the recorded marker for `predecessor`.
    fn load(&self, predecessor: &DelegateKey) -> Result<Option<MigrationMarker>, Self::Error>;

    /// Persist `marker` for `predecessor`. Must be durable when this returns.
    fn store(
        &mut self,
        predecessor: &DelegateKey,
        marker: MigrationMarker,
    ) -> Result<(), Self::Error>;
}

// ---------------------------------------------------------------------------
// Predecessor side: reconstructing pairs from Delta's own protocol
// ---------------------------------------------------------------------------

/// Reads predecessors through Delta's own delegate protocol.
///
/// Delta's delegate has no "enumerate all secrets" request, and adding one would
/// re-key it. So [`fetch_secrets`](PredecessorSecretsIo::fetch_secrets)
/// RECONSTRUCTS the pair list from the requests the delegate already
/// understands, which is what the trait means by "via the app's own delegate
/// protocol":
///
/// * `GetKnownSites` yields the site list, and with it the set of prefixes.
/// * `GetSigningKeyForPrefix { prefix }` yields each per-site key (V8+).
/// * `GetSigningKey` yields the legacy single-slot key (V1+).
/// * `GetSiteState { prefix }` yields each state backup.
///
/// `extra_prefixes` seeds prefixes the caller already knows about locally, so a
/// site whose key is stranded in a predecessor is still probed even when that
/// predecessor's own `GetKnownSites` comes back empty or unsupported.
pub struct DeltaPredecessorIo<'a, C: DeltaDelegateChannel> {
    channel: &'a C,
    extra_prefixes: Vec<String>,
}

impl<'a, C: DeltaDelegateChannel> DeltaPredecessorIo<'a, C> {
    /// Wrap `channel`, additionally probing `extra_prefixes` on every predecessor.
    pub fn new(channel: &'a C, extra_prefixes: Vec<String>) -> Self {
        Self {
            channel,
            extra_prefixes,
        }
    }
}

impl<C: DeltaDelegateChannel> PredecessorSecretsIo for DeltaPredecessorIo<'_, C> {
    type Error = C::Error;

    /// Cheap executability preflight: `GetPublicKey`.
    ///
    /// Chosen because it is the ONLY request every delegate generation back to V1
    /// understands. `GetKnownSites` (V3+) and `GetSigningKeyForPrefix` (V7+) would
    /// misreport a V1/V2 predecessor as unresponsive when it is merely older.
    ///
    /// Any reply counts as executed, INCLUDING [`DelegateResponse::Error`] — a
    /// delegate that answers "no signing key stored" has demonstrably run. Only
    /// silence is `Ok(false)`.
    async fn probe_executable(&mut self, predecessor: &DelegateKey) -> Result<bool, Self::Error> {
        Ok(self
            .channel
            .request(predecessor, DelegateRequest::GetPublicKey)
            .await?
            .is_some())
    }

    async fn fetch_secrets(
        &mut self,
        predecessor: &DelegateKey,
    ) -> Result<Vec<SecretPair>, Self::Error> {
        let mut pairs: Vec<SecretPair> = Vec::new();
        let mut prefixes: Vec<String> = self.extra_prefixes.clone();

        // 1. The known-sites list, as ONE pair (it is one delegate entry).
        if let Some(DelegateResponse::KnownSites(records)) = self
            .channel
            .request(predecessor, DelegateRequest::GetKnownSites)
            .await?
        {
            for record in &records {
                if !record.is_tombstone() && !prefixes.contains(&record.prefix) {
                    prefixes.push(record.prefix.clone());
                }
            }
            // Emit even an empty list only when it carries tombstones: an empty,
            // tombstone-free list is genuinely "no data" and emitting it would
            // make a data-free predecessor look data-bearing to the sticky-data
            // rule.
            if !records.is_empty() {
                pairs.push((
                    SECRET_KNOWN_SITES.as_bytes().to_vec(),
                    encode_known_sites(&records),
                ));
            }
        }

        // 2. The legacy single-slot signing key (V1+).
        if let Some(DelegateResponse::SigningKey(bytes)) = self
            .channel
            .request(predecessor, DelegateRequest::GetSigningKey)
            .await?
        {
            if !bytes.is_empty() {
                pairs.push((SECRET_SIGNING_KEY_LEGACY.as_bytes().to_vec(), bytes));
            }
        }

        // 3. Per-site signing keys and state backups.
        for prefix in &prefixes {
            if let Some(DelegateResponse::SigningKey(bytes)) = self
                .channel
                .request(
                    predecessor,
                    DelegateRequest::GetSigningKeyForPrefix {
                        prefix: prefix.clone(),
                    },
                )
                .await?
            {
                if !bytes.is_empty() {
                    pairs.push((per_prefix_signing_key(prefix).into_bytes(), bytes));
                }
            }

            if let Some(DelegateResponse::SiteState { state_bytes, .. }) = self
                .channel
                .request(
                    predecessor,
                    DelegateRequest::GetSiteState {
                        prefix: prefix.clone(),
                    },
                )
                .await?
            {
                if !state_bytes.is_empty() {
                    pairs.push((site_state_key(prefix).into_bytes(), state_bytes));
                }
            }
        }

        Ok(pairs)
    }
}

/// The delegate's per-site signing-key slot name for `prefix`.
pub fn per_prefix_signing_key(prefix: &str) -> String {
    format!("{SECRET_SIGNING_KEY_PREFIX}{prefix}")
}

/// The delegate's state-backup slot name for `prefix`.
pub fn site_state_key(prefix: &str) -> String {
    format!("{SECRET_SITE_STATE_PREFIX}{prefix}")
}

/// CBOR-encode a known-sites list for transport as one opaque secret value.
pub fn encode_known_sites(records: &[KnownSiteRecord]) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(records, &mut buf).expect("CBOR known sites");
    buf
}

/// Decode a known-sites list produced by [`encode_known_sites`].
pub fn decode_known_sites(bytes: &[u8]) -> Option<Vec<KnownSiteRecord>> {
    ciborium::de::from_reader(bytes).ok()
}

// ---------------------------------------------------------------------------
// The known-sites read-merge-write
// ---------------------------------------------------------------------------

/// Merge a predecessor's known-sites list into the successor's, never-clobber.
///
/// This is constraint 2 of the module docs, and the specific reason a raw pair
/// copy cannot be used for Delta. `StoreKnownSites` REPLACES the delegate's whole
/// list, so writing the predecessor's list verbatim would delete every site the
/// user created on the successor. The mirror-image bug — measured in another
/// adopter — HID keys by skipping an index; here the same shape would DELETE
/// sites by overwriting a list.
///
/// Rules, in priority order:
///
/// 1. **The successor's own record for a prefix always wins** (never-clobber, at
///    record granularity). This is what makes union's newest-wins guarantee hold.
/// 2. A predecessor record for a prefix the successor has never seen is ADDED.
///    This is what carries a site forward across a re-key.
/// 3. A tombstone is a removal and is preserved as such — it is never converted
///    into a live site by the merge, in either direction. A prefix the successor
///    tombstoned stays tombstoned even if the predecessor still holds a live
///    record for it, so a union cannot resurrect a site removed in the tombstone
///    era (delegate V3+).
///
/// Ordering is deterministic (by prefix) so the result does not depend on map
/// iteration order — the merge is run once per predecessor and must be
/// reproducible for the differential test to be meaningful.
pub fn merge_known_sites(
    successor: &[KnownSiteRecord],
    predecessor: &[KnownSiteRecord],
) -> Vec<KnownSiteRecord> {
    let mut merged: BTreeMap<String, KnownSiteRecord> = BTreeMap::new();
    // Rule 1 + 3: the successor is authoritative for every prefix it knows,
    // whether as a live record or as a tombstone.
    for record in successor {
        merged.insert(record.prefix.clone(), record.clone());
    }
    // Rule 2: add only prefixes the successor has never seen at all.
    for record in predecessor {
        merged
            .entry(record.prefix.clone())
            .or_insert_with(|| record.clone());
    }
    merged.into_values().collect()
}

/// The comparable content of one known-sites record.
///
/// `KnownSiteRecord` deliberately does NOT derive `PartialEq`, and this module
/// must not add one: the derive lives in `delta-core`, which BOTH the contract
/// and the delegate WASM depend on, so touching it risks changing the delegate
/// bytes — and a changed delegate re-keys it, destroying the very secrets this
/// module migrates. Comparing a projection keeps the whole adoption inside
/// `ui/`. (`site_sets_match` is also order-independent, which a derived
/// `Vec::eq` would not be — the delegate returns its list in storage order.)
fn site_fields(record: &KnownSiteRecord) -> (String, bool, Option<String>) {
    (
        record.name.clone(),
        record.is_owner,
        record.contract_key_b58.clone(),
    )
}

/// Whether two known-sites lists describe the same set of sites, ignoring order.
fn site_sets_match(a: &[KnownSiteRecord], b: &[KnownSiteRecord]) -> bool {
    let index = |records: &[KnownSiteRecord]| -> BTreeMap<String, (String, bool, Option<String>)> {
        records
            .iter()
            .map(|r| (r.prefix.clone(), site_fields(r)))
            .collect()
    };
    index(a) == index(b)
}

// ---------------------------------------------------------------------------
// Successor side: the app's own import path
// ---------------------------------------------------------------------------

/// Errors from the successor-side writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The delegate did not acknowledge a store request within the bound.
    NoAck(&'static str),
    /// The delegate replied with an error.
    Delegate(String),
    /// The transport failed.
    Transport(String),
    /// The marker store failed.
    Marker(String),
    /// A recovered value was not the shape its key implies.
    Malformed(&'static str),
}

impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoAck(what) => write!(f, "no acknowledgement for {what}"),
            Self::Delegate(e) => write!(f, "delegate error: {e}"),
            Self::Transport(e) => write!(f, "transport error: {e}"),
            Self::Marker(e) => write!(f, "marker store error: {e}"),
            Self::Malformed(what) => write!(f, "malformed recovered {what}"),
        }
    }
}

/// Delta's own import path, as the library's successor writer.
///
/// Every arm is **never-clobber**: it reads the successor's current value first
/// and returns [`ItemWrite::AlreadyAuthoritative`] rather than overwriting. That
/// is not a stylistic choice — under
/// [`SecretSelectionPolicy::UnionAllGenerations`] the library's newest-wins
/// guarantee holds ONLY because this writer declines a key the successor already
/// has. An overwriting writer would install the OLDEST generation's value and
/// still report cleanly.
///
/// Writes are **write-through, not buffered**: each arm awaits the delegate's own
/// acknowledgement (`KeyStored` / `SitesStored` / `SiteStateStored`) before
/// answering [`ItemWrite::Written`]. That is what makes
/// [`SuccessorSecretsIo::flush_predecessor`]'s default no-op honest here, and it
/// sidesteps the buffering-writer hazard the 0.5.0 flush contract exists for.
pub struct DeltaSuccessorIo<'a, C: DeltaDelegateChannel, M: MigrationMarkerStore> {
    channel: &'a C,
    markers: &'a mut M,
    successor: DelegateKey,
    newest_generation: Option<u32>,
}

impl<'a, C: DeltaDelegateChannel, M: MigrationMarkerStore> DeltaSuccessorIo<'a, C, M> {
    /// Write into `successor` over `channel`, recording markers in `markers`.
    ///
    /// `newest_generation` is the highest generation in the lineage — the
    /// delegate immediately preceding the current one. It gates which
    /// generations may contribute LIVE known-sites records; see
    /// [`Self::accepts_site_records_from`].
    pub fn new(
        channel: &'a C,
        markers: &'a mut M,
        successor: DelegateKey,
        newest_generation: Option<u32>,
    ) -> Self {
        Self {
            channel,
            markers,
            successor,
            newest_generation,
        }
    }

    /// Whether LIVE known-sites records from `generation` may be unioned in.
    ///
    /// Only the NEWEST predecessor's live records are, which preserves the rule
    /// the shipped sweep encodes in `super::delegate::is_newest_legacy_delegate`
    /// and its "generation-aware reconciliation" comment. The reason is specific
    /// and load-bearing:
    ///
    /// * The newest predecessor is POST-TOMBSTONE (the convention has existed
    ///   since delegate V3), so a site removed while it was current is recorded
    ///   there as a TOMBSTONE, never as a live record. Unioning it cannot
    ///   resurrect a removed site.
    /// * An OLDER predecessor can hold a FROZEN live record for a site removed
    ///   later, because a pre-tombstone removal deleted the record only from the
    ///   delegate that was current at removal time. Unioning those WOULD
    ///   resurrect a site the user deleted.
    ///
    /// This is the one place [`SecretSelectionPolicy::UnionAllGenerations`] needs
    /// narrowing for Delta. The policy stays Union — it is what stops the walk
    /// halting at a silent predecessor, and it is what recovers SIGNING KEYS from
    /// every generation, which is the data whose loss is unrecoverable. Only the
    /// site LIST, which is the one entry with delete-by-absence semantics, is
    /// restricted. Tombstones are accepted from every generation, since they can
    /// only ever remove.
    fn accepts_site_records_from(&self, generation: u32) -> bool {
        self.newest_generation
            .is_none_or(|newest| generation == newest)
    }

    /// Ask the successor for a value, mapping transport failure to [`WriteError`].
    async fn ask(
        &mut self,
        request: DelegateRequest,
    ) -> Result<Option<DelegateResponse>, WriteError> {
        self.channel
            .request(&self.successor, request)
            .await
            .map_err(|e| WriteError::Transport(format!("{e:?}")))
    }

    /// Whether the successor already holds a signing key for `prefix`.
    async fn has_signing_key(&mut self, prefix: &str) -> Result<bool, WriteError> {
        let reply = self
            .ask(DelegateRequest::GetSigningKeyForPrefix {
                prefix: prefix.to_string(),
            })
            .await?;
        Ok(matches!(
            reply,
            Some(DelegateResponse::SigningKey(ref bytes)) if !bytes.is_empty()
        ))
    }

    /// Store a signing key for `prefix`, awaiting the delegate's ack.
    async fn put_signing_key(
        &mut self,
        prefix: &str,
        key_bytes: Vec<u8>,
    ) -> Result<(), WriteError> {
        match self
            .ask(DelegateRequest::StoreSigningKey {
                key_bytes,
                prefix: Some(prefix.to_string()),
            })
            .await?
        {
            Some(DelegateResponse::KeyStored) => Ok(()),
            Some(DelegateResponse::Error(e)) => Err(WriteError::Delegate(e)),
            _ => Err(WriteError::NoAck("StoreSigningKey")),
        }
    }

    /// The per-site prefix a raw signing key belongs to, derived from its public
    /// half.
    ///
    /// The legacy single slot (`delta:signing_key`) records no prefix, so a raw
    /// pair copy would land it in the successor's legacy slot where it could
    /// later sign ANOTHER site's content (the cross-site mis-sign
    /// `super::delegate::signing_target` exists to prevent). Re-deriving the
    /// prefix routes it to the correct per-site slot instead. This is precisely
    /// the kind of app-level knowledge the 0.5.0 writer seam exists to preserve.
    fn prefix_for_key(key_bytes: &[u8]) -> Option<String> {
        let arr: [u8; 32] = key_bytes.try_into().ok()?;
        let sk = ed25519_dalek::SigningKey::from_bytes(&arr);
        Some(delta_core::pubkey_to_prefix(&sk.verifying_key()))
    }
}

impl<C: DeltaDelegateChannel, M: MigrationMarkerStore> SuccessorSecretsIo
    for DeltaSuccessorIo<'_, C, M>
{
    type Error = WriteError;

    async fn migration_marker(
        &mut self,
        query: &MarkerQuery<'_>,
    ) -> Result<Option<MigrationMarker>, Self::Error> {
        self.markers
            .load(query.predecessor)
            .map_err(|e| WriteError::Marker(format!("{e:?}")))
    }

    async fn record_marker(
        &mut self,
        predecessor: &DelegateKey,
        marker: MigrationMarker,
    ) -> Result<(), Self::Error> {
        // Durable on return: `MigrationMarkerStore::store` is specified to
        // persist synchronously and is NOT routed through the item path.
        self.markers
            .store(predecessor, marker)
            .map_err(|e| WriteError::Marker(format!("{e:?}")))
    }

    async fn write_secret(&mut self, item: &RecoveredSecret<'_>) -> ItemWrite<Self::Error> {
        let Ok(key) = core::str::from_utf8(item.key) else {
            // Not one of Delta's keys at all. Nothing to import, and it is not a
            // failure: the successor's (empty) state stands.
            return ItemWrite::AlreadyAuthoritative;
        };

        // --- the known-sites LIST: read-merge-write, never a verbatim copy ---
        if key == SECRET_KNOWN_SITES {
            let Some(incoming) = decode_known_sites(item.value) else {
                // A value we cannot parse will never become parseable.
                return ItemWrite::permanent(WriteError::Malformed("known_sites"));
            };
            // Generation gate: an OLDER predecessor may contribute only its
            // TOMBSTONES (which can only remove), never its live records (which
            // could resurrect a pre-tombstone-era removal). See
            // `accepts_site_records_from`.
            let contributions: Vec<KnownSiteRecord> =
                if self.accepts_site_records_from(item.generation) {
                    incoming
                } else {
                    incoming.into_iter().filter(|r| r.is_tombstone()).collect()
                };
            let current = match self.ask(DelegateRequest::GetKnownSites).await {
                Ok(Some(DelegateResponse::KnownSites(records))) => records,
                Ok(_) => Vec::new(),
                Err(e) => return ItemWrite::retryable(e),
            };
            let merged = merge_known_sites(&current, &contributions);
            if site_sets_match(&merged, &current) {
                // Every site the predecessor knows is already represented.
                return ItemWrite::AlreadyAuthoritative;
            }
            return match self
                .ask(DelegateRequest::StoreKnownSites { sites: merged })
                .await
            {
                Ok(Some(DelegateResponse::SitesStored)) => ItemWrite::Written,
                Ok(Some(DelegateResponse::Error(e))) => {
                    ItemWrite::retryable(WriteError::Delegate(e))
                }
                Ok(_) => ItemWrite::retryable(WriteError::NoAck("StoreKnownSites")),
                Err(e) => ItemWrite::retryable(e),
            };
        }

        // --- per-site signing keys ---
        if let Some(prefix) = key.strip_prefix(SECRET_SIGNING_KEY_PREFIX) {
            return self
                .import_signing_key(prefix.to_string(), item.value.to_vec())
                .await;
        }

        // --- the legacy single-slot signing key: re-derive its real prefix ---
        if key == SECRET_SIGNING_KEY_LEGACY {
            let Some(prefix) = Self::prefix_for_key(item.value) else {
                return ItemWrite::permanent(WriteError::Malformed("signing key"));
            };
            return self.import_signing_key(prefix, item.value.to_vec()).await;
        }

        // --- per-site state backups ---
        if let Some(prefix) = key.strip_prefix(SECRET_SITE_STATE_PREFIX) {
            let prefix = prefix.to_string();
            let existing = match self
                .ask(DelegateRequest::GetSiteState {
                    prefix: prefix.clone(),
                })
                .await
            {
                Ok(Some(DelegateResponse::SiteState { state_bytes, .. })) => {
                    !state_bytes.is_empty()
                }
                Ok(_) => false,
                Err(e) => return ItemWrite::retryable(e),
            };
            if existing {
                // The successor's own backup is fresher by construction (it is
                // written from live state on every reconcile).
                return ItemWrite::AlreadyAuthoritative;
            }
            return match self
                .ask(DelegateRequest::StoreSiteState {
                    prefix,
                    state_bytes: item.value.to_vec(),
                })
                .await
            {
                Ok(Some(DelegateResponse::SiteStateStored)) => ItemWrite::Written,
                Ok(Some(DelegateResponse::Error(e))) => {
                    ItemWrite::retryable(WriteError::Delegate(e))
                }
                Ok(_) => ItemWrite::retryable(WriteError::NoAck("StoreSiteState")),
                Err(e) => ItemWrite::retryable(e),
            };
        }

        // An unrecognized key: not Delta's to copy. Complete and correct.
        ItemWrite::AlreadyAuthoritative
    }
}

impl<C: DeltaDelegateChannel, M: MigrationMarkerStore> DeltaSuccessorIo<'_, C, M> {
    /// Never-clobber import of one per-site signing key.
    async fn import_signing_key(
        &mut self,
        prefix: String,
        key_bytes: Vec<u8>,
    ) -> ItemWrite<WriteError> {
        if key_bytes.len() != 32 {
            return ItemWrite::permanent(WriteError::Malformed("signing key"));
        }
        match self.has_signing_key(&prefix).await {
            // Never-clobber: the successor's key for this site stands.
            Ok(true) => ItemWrite::AlreadyAuthoritative,
            Ok(false) => match self.put_signing_key(&prefix, key_bytes).await {
                Ok(()) => ItemWrite::Written,
                Err(e) => ItemWrite::retryable(e),
            },
            Err(e) => ItemWrite::retryable(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The policy Delta migrates under.
///
/// [`SecretSelectionPolicy::UnionAllGenerations`], because the default
/// (`NewestSnapshotWins`) halts the walk at the first predecessor that answers
/// silently, and silence is the ORDINARY case here: V6/V7 delegates predate
/// `GetSigningKeyForPrefix` and cannot answer it at all, and any predecessor the
/// reached node never registered is indistinguishable from a broken one. Halting
/// there would strand every older generation's keys — the freenet/river#204
/// failure this policy exists for.
///
/// The [`UnionAck`] acknowledges both hazards: union resurrects delete-by-absence
/// data (bounded for Delta by the tombstone convention — see the module docs),
/// and withholding is run-scoped (freenet-migrate#15, not covered here).
pub fn delta_secret_policy() -> SecretSelectionPolicy {
    SecretSelectionPolicy::UnionAllGenerations(
        UnionAck::i_understand_union_resurrects_deleted_by_absence_secrets(),
    )
}

/// Run the delegate secret migration for `successor` over `predecessors`.
///
/// `predecessors` is the lineage OLDEST-FIRST (the order `legacy_delegates.toml`
/// is authored in); the library walks it newest-first itself.
pub async fn run_delegate_migration<C: DeltaDelegateChannel, M: MigrationMarkerStore>(
    channel: &C,
    markers: &mut M,
    successor: DelegateKey,
    predecessors: &[DelegateLineageEntry],
    extra_prefixes: Vec<String>,
) -> DelegateMigrationReport {
    // `migrate_delegate_secrets` holds the writer and the reader as two
    // simultaneous `&mut` borrows and both need the transport, which is why
    // `DeltaDelegateChannel::request` takes `&self`: the shared `&C` can sit in
    // both without conflict.
    let newest_generation = predecessors.iter().map(|e| e.generation).max();
    let mut reader = DeltaPredecessorIo::new(channel, extra_prefixes);
    let mut writer = DeltaSuccessorIo::new(channel, markers, successor, newest_generation);
    freenet_migrate::migrate_delegate_secrets(
        &mut writer,
        &mut reader,
        predecessors,
        MigrationAuthorization::app_author_ack(),
        delta_secret_policy(),
    )
    .await
}

/// Build the library's lineage from Delta's generated `LEGACY_DELEGATES` table.
///
/// The table is `(delegate_key, code_hash)` OLDEST-FIRST, so the slice index is
/// the generation — the same convention `super::operations::legacy_lineage_newest_first`
/// uses for contracts. `irregular_key` is `false` because Delta's build script
/// already validates `delegate_key == BLAKE3(code_hash)` at compile time for
/// every row (a typo in `legacy_delegates.toml` fails the build), so no row is
/// trusted as-recorded the way River's V1/V2 are.
pub fn delta_delegate_lineage(table: &[([u8; 32], [u8; 32])]) -> Vec<DelegateLineageEntry> {
    table
        .iter()
        .enumerate()
        .map(
            |(generation, (delegate_key, code_hash))| DelegateLineageEntry {
                generation: generation as u32,
                code_hash: *code_hash,
                delegate_key: *delegate_key,
                irregular_key: false,
                note: "delta legacy delegate",
            },
        )
        .collect()
}

// ---------------------------------------------------------------------------
// Production transport (wasm)
// ---------------------------------------------------------------------------

/// The production transport and marker store.
///
/// Delta's WebSocket transport is fire-and-forget — `send_to_delegate_key`
/// pushes a `ClientRequest` and the reply lands later in the global
/// [`super::delegate::handle_delegate_response`] — while the library's traits
/// need an awaitable round-trip. This module closes that gap with a
/// pending-reply registry: a request parks a slot keyed by
/// `(delegate_key, expected reply kind)`, and the response handler resolves it.
///
/// # Correlation is by kind, not by id
///
/// Delta's delegate protocol carries NO request id, and adding one would change
/// the delegate WASM — the exact re-key this migration exists to survive. So
/// replies are matched on `(delegate_key, reply kind)`, oldest waiter first.
/// That is sound here because the migration awaits each round-trip
/// SEQUENTIALLY, so it never has two requests of the same kind outstanding
/// against the same delegate. It is NOT sound for concurrent callers, which is
/// why nothing else uses this channel.
#[cfg(target_arch = "wasm32")]
pub mod wasm_transport {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    /// How long to wait for a reply before treating a predecessor as silent.
    ///
    /// A predecessor that does not answer within this bound is reported
    /// `Unresponsive` rather than silently treated as empty (the
    /// freenet/river#204 gate). Generous, because an old delegate's first
    /// execution on a cold node can be slow, and a false `Unresponsive` costs a
    /// user their signing key.
    const REPLY_TIMEOUT_MS: u32 = 10_000;

    /// The reply shape a request expects.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ReplyKind {
        PublicKey,
        SigningKey,
        KnownSites,
        SiteState,
        KeyStored,
        SitesStored,
        SiteStateStored,
    }

    impl ReplyKind {
        fn of_request(request: &DelegateRequest) -> Option<Self> {
            Some(match request {
                DelegateRequest::GetPublicKey => Self::PublicKey,
                DelegateRequest::GetSigningKey | DelegateRequest::GetSigningKeyForPrefix { .. } => {
                    Self::SigningKey
                }
                DelegateRequest::GetKnownSites => Self::KnownSites,
                DelegateRequest::GetSiteState { .. } => Self::SiteState,
                DelegateRequest::StoreSigningKey { .. } => Self::KeyStored,
                DelegateRequest::StoreKnownSites { .. } => Self::SitesStored,
                DelegateRequest::StoreSiteState { .. } => Self::SiteStateStored,
                _ => return None,
            })
        }

        /// Whether `response` answers this request.
        ///
        /// `Error` answers ANY request: a delegate that replies "no signing key
        /// stored" has demonstrably executed, which is exactly what the
        /// executability preflight needs to distinguish from silence.
        fn matches(self, response: &DelegateResponse) -> bool {
            match (self, response) {
                (_, DelegateResponse::Error(_)) => true,
                (Self::PublicKey, DelegateResponse::PublicKey(_)) => true,
                (Self::SigningKey, DelegateResponse::SigningKey(_)) => true,
                (Self::KnownSites, DelegateResponse::KnownSites(_)) => true,
                (Self::SiteState, DelegateResponse::SiteState { .. }) => true,
                (Self::KeyStored, DelegateResponse::KeyStored) => true,
                (Self::SitesStored, DelegateResponse::SitesStored) => true,
                (Self::SiteStateStored, DelegateResponse::SiteStateStored) => true,
                _ => false,
            }
        }
    }

    type Slot = Rc<RefCell<SlotState>>;

    #[derive(Default)]
    struct SlotState {
        reply: Option<DelegateResponse>,
        timed_out: bool,
        waker: Option<Waker>,
    }

    struct Pending {
        target: Vec<u8>,
        kind: ReplyKind,
        slot: Slot,
    }

    thread_local! {
        static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    }

    /// Offer a delegate response to any waiting migration round-trip.
    ///
    /// Returns `true` if it was consumed by the migration. Called from
    /// [`super::super::delegate::handle_delegate_response`] BEFORE its own
    /// handling, so a reply the migration is awaiting is not also applied to UI
    /// state by the shipped sweep.
    pub fn offer_response(responding_key: &DelegateKey, response: &DelegateResponse) -> bool {
        PENDING.with(|pending| {
            let mut pending = pending.borrow_mut();
            let Some(index) = pending
                .iter()
                .position(|p| p.target == responding_key.bytes() && p.kind.matches(response))
            else {
                return false;
            };
            let entry = pending.remove(index);
            let mut slot = entry.slot.borrow_mut();
            slot.reply = Some(response.clone());
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
            true
        })
    }

    /// The production [`DeltaDelegateChannel`].
    pub struct WasmChannel;

    impl DeltaDelegateChannel for WasmChannel {
        type Error = core::convert::Infallible;

        fn request(
            &self,
            target: &DelegateKey,
            request: DelegateRequest,
        ) -> impl Future<Output = Result<Option<DelegateResponse>, Self::Error>> {
            let slot: Slot = Rc::new(RefCell::new(SlotState::default()));
            if let Some(kind) = ReplyKind::of_request(&request) {
                PENDING.with(|pending| {
                    pending.borrow_mut().push(Pending {
                        target: target.bytes().to_vec(),
                        kind,
                        slot: slot.clone(),
                    })
                });
                super::super::delegate::send_to_delegate_key_pub(&request, target.clone());
                arm_timeout(slot.clone());
            } else {
                // A request the delegate does not understand is never sent.
                slot.borrow_mut().timed_out = true;
            }
            ReplyFuture { slot }
        }
    }

    /// Fire a timer that marks the slot timed-out, so a silent predecessor
    /// resolves as `Ok(None)` (→ `Unresponsive`) instead of hanging the walk.
    fn arm_timeout(slot: Slot) {
        use wasm_bindgen::prelude::*;
        let cb = Closure::<dyn Fn()>::new(move || {
            let mut state = slot.borrow_mut();
            if state.reply.is_some() {
                return;
            }
            state.timed_out = true;
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        });
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                REPLY_TIMEOUT_MS as i32,
            );
        }
        cb.forget();
    }

    struct ReplyFuture {
        slot: Slot,
    }

    impl Future for ReplyFuture {
        type Output = Result<Option<DelegateResponse>, core::convert::Infallible>;

        fn poll(self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut state = self.slot.borrow_mut();
            if let Some(reply) = state.reply.take() {
                return Poll::Ready(Ok(Some(reply)));
            }
            if state.timed_out {
                return Poll::Ready(Ok(None));
            }
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    /// `localStorage`-backed marker store, falling back to page memory.
    ///
    /// See [`MigrationMarkerStore`] for why markers cannot live in the delegate
    /// (no generic secret-set request, and adding one would re-key it) and what
    /// the page-lifetime fallback costs in the gateway iframe.
    #[derive(Default)]
    pub struct BrowserMarkers {
        memory: BTreeMap<Vec<u8>, MigrationMarker>,
    }

    impl BrowserMarkers {
        fn storage_key(predecessor: &DelegateKey) -> String {
            format!(
                "delta_migrate_pred_{}",
                bs58::encode(predecessor.bytes()).into_string()
            )
        }
    }

    impl MigrationMarkerStore for BrowserMarkers {
        type Error = core::convert::Infallible;

        fn load(&self, predecessor: &DelegateKey) -> Result<Option<MigrationMarker>, Self::Error> {
            if let Some(value) = web_sys::window()
                .and_then(|w| w.local_storage().ok().flatten())
                .and_then(|s| s.get_item(&Self::storage_key(predecessor)).ok().flatten())
            {
                return Ok(Some(match value.as_str() {
                    "done_data" => MigrationMarker::Done { had_data: true },
                    "done_empty" => MigrationMarker::Done { had_data: false },
                    "wip_data" => MigrationMarker::InProgress { saw_data: true },
                    _ => MigrationMarker::InProgress { saw_data: false },
                }));
            }
            Ok(self.memory.get(predecessor.bytes()).copied())
        }

        fn store(
            &mut self,
            predecessor: &DelegateKey,
            marker: MigrationMarker,
        ) -> Result<(), Self::Error> {
            let encoded = match marker {
                MigrationMarker::Done { had_data: true } => "done_data",
                MigrationMarker::Done { had_data: false } => "done_empty",
                MigrationMarker::InProgress { saw_data: true } => "wip_data",
                MigrationMarker::InProgress { saw_data: false } => "wip_empty",
            };
            // Durable on return where storage exists; page-lifetime otherwise.
            // Both paths complete synchronously, so the marker is never batched
            // with the item writes.
            if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten())
            {
                let _ = storage.set_item(&Self::storage_key(predecessor), encoded);
            }
            self.memory.insert(predecessor.bytes().to_vec(), marker);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(prefix: &str, name: &str) -> KnownSiteRecord {
        KnownSiteRecord {
            prefix: prefix.to_string(),
            name: name.to_string(),
            is_owner: true,
            contract_key_b58: None,
        }
    }

    /// The two-site case the whole read-merge-write exists for, and which no
    /// earlier Delta test covered: a site created on the OLD delegate and a
    /// DIFFERENT site created on the NEW one must BOTH survive the migration.
    ///
    /// A naive pass-through writer (`StoreKnownSites { sites: predecessor }`)
    /// would REPLACE the successor's whole list and delete site `new`. That is
    /// the mirror image of the index-skipping bug measured in another adopter:
    /// there a never-clobber pair copy HID keys, here an overwriting copy would
    /// DELETE sites.
    #[test]
    fn merge_keeps_sites_from_both_delegates() {
        let successor = vec![site("newsiteaaa", "New")];
        let predecessor = vec![site("oldsitebbb", "Old")];
        let merged = merge_known_sites(&successor, &predecessor);
        let prefixes: Vec<&str> = merged.iter().map(|r| r.prefix.as_str()).collect();
        assert_eq!(prefixes, vec!["newsiteaaa", "oldsitebbb"]);
    }

    /// Never-clobber at record granularity: the successor's own record for a
    /// prefix wins. This is what makes `UnionAllGenerations`' newest-wins
    /// guarantee hold — the library does NOT enforce it.
    #[test]
    fn successor_record_wins_over_predecessor() {
        let successor = vec![site("samesiteaa", "Renamed on new")];
        let predecessor = vec![site("samesiteaa", "Old name")];
        let merged = merge_known_sites(&successor, &predecessor);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "Renamed on new");
    }

    /// A union must not resurrect a site removed in the tombstone era: the
    /// successor's tombstone outranks the predecessor's stale live record.
    #[test]
    fn successor_tombstone_blocks_predecessor_resurrection() {
        let successor = vec![KnownSiteRecord::tombstone("goneaaaaaa")];
        let predecessor = vec![site("goneaaaaaa", "Deleted site")];
        let merged = merge_known_sites(&successor, &predecessor);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].is_tombstone(), "removal must survive the merge");
    }

    /// A predecessor's tombstone for a prefix the successor never saw is carried
    /// forward as a removal, not dropped and not converted into a live site.
    #[test]
    fn predecessor_tombstone_carries_forward_as_removal() {
        let successor: Vec<KnownSiteRecord> = Vec::new();
        let predecessor = vec![KnownSiteRecord::tombstone("goneaaaaaa")];
        let merged = merge_known_sites(&successor, &predecessor);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].is_tombstone());
    }

    /// The merge is idempotent, so a re-walk (which is the ORDINARY case for
    /// Delta, whose markers are page-lifetime in the production iframe) writes
    /// nothing new and the writer answers `AlreadyAuthoritative`.
    #[test]
    fn merge_is_idempotent() {
        let successor = vec![site("newsiteaaa", "New")];
        let predecessor = vec![site("oldsitebbb", "Old")];
        let once = merge_known_sites(&successor, &predecessor);
        let twice = merge_known_sites(&once, &predecessor);
        assert!(site_sets_match(&once, &twice));
    }

    /// The secret-key names this adapter writes must be exactly the ones the
    /// delegate WASM already understands. If a future edit needs a name the
    /// delegate does not have, it needs a NEW delegate request variant — which
    /// re-keys the WASM and destroys the very data this module migrates. Pin the
    /// names so that change cannot happen silently.
    #[test]
    fn delegate_migration_is_ui_side_only() {
        let delegate_src = include_str!("../../../delegates/site-delegate/src/lib.rs");
        assert!(
            delegate_src.contains(&format!("\"{SECRET_SIGNING_KEY_LEGACY}\"")),
            "legacy signing-key slot name drifted from the delegate"
        );
        assert!(
            delegate_src.contains(&format!("\"{SECRET_KNOWN_SITES}\"")),
            "known-sites slot name drifted from the delegate"
        );
        assert!(
            delegate_src.contains("delta:signing_key:{prefix}"),
            "per-prefix signing-key slot name drifted from the delegate"
        );
        assert!(
            delegate_src.contains("delta:site_state:{prefix}"),
            "site-state slot name drifted from the delegate"
        );
    }
}
