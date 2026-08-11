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
/// correlates replies via the [`correlation`] registry: by delegate key, reply
/// kind, and whatever identity the reply itself carries — the protocol has no
/// request ids, and the concurrently-running legacy sweep sends the same
/// request kinds to the same delegates, so a same-kind reply is NOT proof it
/// answers this request. Callers must therefore treat a reply as evidence only
/// of what its content proves (see [`DeltaSuccessorIo::has_signing_key`] and
/// [`DeltaPredecessorIo::fetch_secrets`] for the two places that matters).
/// Abstracting the round-trip here is what lets the whole adapter — and the
/// differential against the shipped sweep — run natively in unit tests rather
/// than only in a browser.
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
        //
        // Replies are attributed by their CONTENT, never by the request they
        // are assumed to answer. A `SigningKey` reply names no prefix, and two
        // real senders can put a differently-owned key on this await: the
        // delegate's own legacy-single-slot fallback (a sequential fact of the
        // shipped WASM), and a mis-correlated reply to one of the concurrent
        // sweep's probes. Slotting such bytes under the PROBED prefix wrote
        // another site's key into `delta:signing_key:{probed}` — permanent
        // wrong-key corruption once never-clobber seals it. Deriving the slot
        // from the key bytes makes every genuine key land under its true
        // prefix no matter which request its reply actually answered. (A key
        // whose true owner was thereby not probed loses nothing either: the
        // same reply falls through to the sweep's `SigningKey` arm, which
        // re-stores it content-addressed as well.)
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
                if let Some(derived) = signing_key_prefix(&bytes) {
                    let slot = per_prefix_signing_key(&derived).into_bytes();
                    // Distinct probes can surface the same key (the fallback
                    // serves it for every keyless prefix); emit it once.
                    if !pairs.iter().any(|(key, _)| *key == slot) {
                        pairs.push((slot, bytes));
                    }
                }
            }

            // A `SiteState` reply DOES echo its prefix; trust the echo, not
            // the request. (The wasm registry already rejects a mismatched
            // echo, so there the two always agree; a non-wasm channel gets the
            // same attribution safety from this.)
            if let Some(DelegateResponse::SiteState {
                prefix: echoed,
                state_bytes,
            }) = self
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
                    let slot = site_state_key(&echoed).into_bytes();
                    if !pairs.iter().any(|(key, _)| *key == slot) {
                        pairs.push((slot, state_bytes));
                    }
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

/// The site prefix `key_bytes` ACTUALLY belongs to, derived from its public
/// half. `None` if the bytes are not a 32-byte Ed25519 seed.
///
/// A site's identity IS its keypair (`prefix = base58(pubkey)[..10]`), so a
/// signing key carries its own attribution. That is the module's defence
/// against every form of mis-labelled key: the delegate's `SigningKey` reply
/// names no prefix, the reply stream carries no request ids, and the real
/// delegate's `load_signing_key` even FALLS BACK to the legacy single slot —
/// so the reply to `GetSigningKeyForPrefix { p }` can legitimately carry a
/// different site's key. Trusting the request's prefix would write that key
/// under `delta:signing_key:p`, permanently (never-clobber) breaking the site;
/// deriving the prefix from the key bytes makes a wrong-slot write impossible
/// regardless of which request a reply actually answered.
pub fn signing_key_prefix(key_bytes: &[u8]) -> Option<String> {
    let arr: [u8; 32] = key_bytes.try_into().ok()?;
    let sk = ed25519_dalek::SigningKey::from_bytes(&arr);
    Some(delta_core::pubkey_to_prefix(&sk.verifying_key()))
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
    /// Every known-sites contribution this RUN has already merged toward the
    /// successor, accumulated across predecessors.
    ///
    /// The read half of the read-merge-write cannot be trusted to be fresh:
    /// its `GetKnownSites` await can be resolved by a mis-correlated reply
    /// whose snapshot predates this run's own previous `StoreKnownSites` (the
    /// concurrent sweep reads the same delegate). Since `StoreKnownSites`
    /// REPLACES the whole list, merging against such a stale base would roll
    /// an earlier generation's sites back off the delegate. Folding this
    /// accumulator into every base makes the run's merges MONOTONE: a stale
    /// read can no longer unwind them. (What it does NOT defend: a stale base
    /// missing a concurrent USER change — that read-modify-write race predates
    /// this module, sits outside the migration's own writes, and tombstone
    /// precedence in `merge_known_sites` bounds it for removals.)
    contributed_sites: Vec<KnownSiteRecord>,
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
            contributed_sites: Vec::new(),
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

    /// Whether the successor already holds **`prefix`'s own** signing key.
    ///
    /// `true` requires the reply's key bytes to DERIVE `prefix` (see
    /// [`signing_key_prefix`]). A non-empty `SigningKey` reply carrying some
    /// other site's key is answered `false`, because it is one of two things,
    /// and "the successor holds p's key" is neither:
    ///
    /// * the delegate's legacy-single-slot FALLBACK (no per-prefix key is
    ///   stored, so importing one is exactly right — per-prefix wins over
    ///   legacy in the delegate's own `select_key_bytes`); or
    /// * a MIS-CORRELATED reply to a concurrent sweep probe for another
    ///   prefix (`SigningKey` carries no prefix, so the reply registry cannot
    ///   reject it). Answering `true` here was the false-`AlreadyAuthoritative`
    ///   data loss: p's key never imported, report clean.
    ///
    /// Answering `false` on a mis-correlated reply is safe even when the
    /// successor DOES hold p's key: the import that follows writes bytes that
    /// [`Self::import_signing_key`] has validated derive to p, and p's key is
    /// the same keypair wherever it is held, so the write is an idempotent
    /// re-store — or a repair, if a wrong key ever landed under p.
    async fn has_signing_key(&mut self, prefix: &str) -> Result<bool, WriteError> {
        let reply = self
            .ask(DelegateRequest::GetSigningKeyForPrefix {
                prefix: prefix.to_string(),
            })
            .await?;
        match reply {
            Some(DelegateResponse::SigningKey(ref bytes)) => {
                Ok(signing_key_prefix(bytes).is_some_and(|derived| derived == prefix))
            }
            // The delegate reports a missing key as an error, so this is the
            // legitimate "successor lacks it, import it" answer.
            Some(DelegateResponse::Error(_)) => Ok(false),
            // Silence is NOT absence. Answering `false` here would import over
            // a key we simply failed to read; never-clobber then seals the
            // wrong key in permanently, and the report still reads `Imported`.
            None => Err(WriteError::NoAck("GetSigningKeyForPrefix")),
            // Any other reply kind means the correlation matched something
            // that was not our answer -- never act on it.
            Some(_) => Err(WriteError::NoAck("GetSigningKeyForPrefix")),
        }
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
            // A genuinely empty successor answers `KnownSites([])` -- the
            // delegate returns an empty vec when the secret is absent, and
            // `Error` only when stored bytes fail to deserialize. So silence
            // and errors NEVER legitimately mean "no sites", and must not be
            // read as an empty base: the merge below feeds `StoreKnownSites`,
            // which REPLACES the whole list, so one 10s timeout would drop
            // every site the user created on the successor. Retry instead.
            let current = match self.ask(DelegateRequest::GetKnownSites).await {
                Ok(Some(DelegateResponse::KnownSites(records))) => records,
                Ok(Some(DelegateResponse::Error(e))) => {
                    return ItemWrite::retryable(WriteError::Delegate(e))
                }
                Ok(_) => return ItemWrite::retryable(WriteError::NoAck("GetKnownSites")),
                Err(e) => return ItemWrite::retryable(e),
            };
            // Fold in what this run already merged, so a stale read cannot
            // roll an earlier generation's sites back off the delegate (see
            // `contributed_sites`). `current` keeps precedence: a genuinely
            // newer successor record or tombstone still wins.
            let base = merge_known_sites(&current, &self.contributed_sites);
            let merged = merge_known_sites(&base, &contributions);
            // Record BEFORE the store: newer generations were merged first, so
            // first-wins accumulation preserves newest-wins, and remembering a
            // contribution whose store then fails only means a later merge
            // re-asserts it — the write is what a retry re-runs anyway.
            self.contributed_sites = merge_known_sites(&self.contributed_sites, &contributions);
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
        //
        // The legacy slot records no prefix, so a raw pair copy would land it
        // in the successor's legacy slot where it could later sign ANOTHER
        // site's content (the cross-site mis-sign
        // `super::delegate::signing_target` exists to prevent). Re-deriving
        // the prefix routes it to the correct per-site slot instead. This is
        // precisely the kind of app-level knowledge the 0.5.0 writer seam
        // exists to preserve.
        if key == SECRET_SIGNING_KEY_LEGACY {
            let Some(prefix) = signing_key_prefix(item.value) else {
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
                // Only a reply that ECHOES our prefix says anything about our
                // backup; one for another prefix is a mis-correlated answer to
                // a concurrent sweep probe. Reading it as "we already have a
                // backup" silently skipped importing the predecessor's — which,
                // if the network copy is gone, was the user's only one.
                Ok(Some(DelegateResponse::SiteState {
                    prefix: ref echoed,
                    ref state_bytes,
                })) if *echoed == prefix => !state_bytes.is_empty(),
                Ok(Some(DelegateResponse::SiteState { .. })) => {
                    return ItemWrite::retryable(WriteError::NoAck("GetSiteState"))
                }
                // Unlike GetKnownSites, absence here IS reported as an error
                // ("no backed-up state for site {prefix}"), so THAT error is
                // the legitimate "successor has no backup" answer and must let
                // the import proceed. It names its prefix — frozen in the
                // deployed WASM, pinned to the delegate source by test — so
                // absence is attributable; any OTHER error is not an answer
                // about this prefix's backup, and treating it as absence would
                // let an older predecessor backup replace a fresher successor
                // one. Silence must not pass either: a timeout is not absence.
                Ok(Some(DelegateResponse::Error(ref e)))
                    if correlation::is_no_backup_error_for(e, &prefix) =>
                {
                    false
                }
                Ok(Some(DelegateResponse::Error(e))) => {
                    return ItemWrite::retryable(WriteError::Delegate(e))
                }
                Ok(None) => return ItemWrite::retryable(WriteError::NoAck("GetSiteState")),
                Ok(_) => return ItemWrite::retryable(WriteError::NoAck("GetSiteState")),
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
        // The last write gate: a key may only ever be stored under the prefix
        // it derives (see `signing_key_prefix`). Every current caller supplies
        // a derived prefix, so this is defence in depth against a future
        // caller trusting a request-side label again; the verdict is stable
        // across versions (the keypair IS the site identity), hence permanent.
        match signing_key_prefix(&key_bytes) {
            Some(derived) if derived == prefix => {}
            _ => return ItemWrite::permanent(WriteError::Malformed("signing key")),
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
// Reply correlation (the host-testable core of the production transport)
// ---------------------------------------------------------------------------

/// Correlates delegate replies with in-flight migration round-trips.
///
/// Delta's delegate protocol carries NO request id, and adding one would change
/// the delegate WASM — the exact re-key this migration exists to survive. So a
/// reply can be matched to a request only by (delegate key, reply shape, and
/// whatever identity the reply itself carries).
///
/// The migration awaits its own round-trips sequentially, so it never has more
/// than ONE request outstanding and the registry holds at most one slot. That
/// does NOT make kind-matching sound on its own: the shipped sweep in
/// `super::delegate` runs CONCURRENTLY from the same trigger and sends the
/// SAME request kinds to the SAME delegates (`fire_legacy_migration`,
/// `migrate_per_prefix_signing_key`, `request_site_state_backup`, plus the
/// stores its response arms issue). Mutual exclusion is not achievable either:
/// the migration's own replies deliberately FALL THROUGH into those sweep arms
/// (see `handle_delegate_response`), and the arms respond by sending more
/// same-kind requests, so sweep traffic during the migration is guaranteed by
/// design. The registry therefore accepts a reply for the slot only when it is
/// consistent with the request, and every decision the adapter derives from a
/// reply that carries no identity of its own is made safe under
/// mis-correlation at the point of decision (see
/// [`DeltaSuccessorIo::has_signing_key`] and
/// [`DeltaPredecessorIo::fetch_secrets`]).
///
/// This module is compiled on every target so those rules are testable on the
/// host; only the browser glue (timers, sends) lives in `wasm_transport`.
///
/// [`DeltaSuccessorIo::has_signing_key`]: super::DeltaSuccessorIo
/// [`DeltaPredecessorIo::fetch_secrets`]: super::DeltaPredecessorIo
pub mod correlation {
    use super::{DelegateRequest, DelegateResponse};
    use freenet_stdlib::prelude::DelegateKey;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    /// The reply shape a request expects.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ReplyKind {
        /// `GetPublicKey` → `PublicKey`.
        PublicKey,
        /// `GetSigningKey` / `GetSigningKeyForPrefix` → `SigningKey`.
        SigningKey,
        /// `GetKnownSites` → `KnownSites`.
        KnownSites,
        /// `GetSiteState` → `SiteState`.
        SiteState,
        /// `StoreSigningKey` → `KeyStored`.
        KeyStored,
        /// `StoreKnownSites` → `SitesStored`.
        SitesStored,
        /// `StoreSiteState` → `SiteStateStored`.
        SiteStateStored,
    }

    /// What a pending request will accept as its answer.
    #[derive(Clone, Debug)]
    pub struct Expectation {
        kind: ReplyKind,
        /// The prefix the request named, for requests that name one. Used to
        /// validate the echo in a `SiteState` reply and the prefix embedded in
        /// the delegate's site-state absence error. Deliberately NOT used to
        /// gate `SigningKey` replies: the delegate legitimately answers
        /// `GetSigningKeyForPrefix { p }` with its LEGACY single-slot key (a
        /// different site's key) when no per-prefix key is stored, so a
        /// prefix-mismatched `SigningKey` reply can be a genuine answer. Those
        /// are validated where the DECISION is made instead.
        prefix: Option<String>,
    }

    impl Expectation {
        /// The expectation `request` establishes, or `None` for a request the
        /// delegate protocol does not cover.
        pub fn of_request(request: &DelegateRequest) -> Option<Self> {
            let kind = match request {
                DelegateRequest::GetPublicKey => ReplyKind::PublicKey,
                DelegateRequest::GetSigningKey | DelegateRequest::GetSigningKeyForPrefix { .. } => {
                    ReplyKind::SigningKey
                }
                DelegateRequest::GetKnownSites => ReplyKind::KnownSites,
                DelegateRequest::GetSiteState { .. } => ReplyKind::SiteState,
                DelegateRequest::StoreSigningKey { .. } => ReplyKind::KeyStored,
                DelegateRequest::StoreKnownSites { .. } => ReplyKind::SitesStored,
                DelegateRequest::StoreSiteState { .. } => ReplyKind::SiteStateStored,
                _ => return None,
            };
            let prefix = match request {
                DelegateRequest::GetSiteState { prefix }
                | DelegateRequest::GetSigningKeyForPrefix { prefix } => Some(prefix.clone()),
                _ => None,
            };
            Some(Self { kind, prefix })
        }

        /// Whether `response` answers this request.
        ///
        /// A reply is accepted only when it is CONSISTENT with the request:
        ///
        /// * its shape answers the request kind;
        /// * a `SiteState` reply must echo the requested prefix — one for
        ///   another prefix is provably an answer to some other (concurrent
        ///   sweep) request, and rejecting it lets OUR reply, which is still
        ///   queued behind it, resolve the await instead;
        /// * an `Error` whose text is one of the current delegate's FROZEN
        ///   strings (the deployed WASM cannot change without the re-key this
        ///   migration exists to survive, so the strings are stable; pinned to
        ///   the delegate source by `error_classification_matches_the_shipped_delegate`)
        ///   must belong to the awaited kind — and, for the site-state absence
        ///   error, name the awaited prefix. An error a concurrent sweep probe
        ///   provoked for a different kind is thereby left to fall through.
        ///   Unrecognized error text (an older generation's wording) matches
        ///   any kind, preserving "an error proves execution" for the
        ///   executability preflight.
        pub fn matches(&self, response: &DelegateResponse) -> bool {
            match (self.kind, response) {
                (kind, DelegateResponse::Error(msg)) => {
                    error_may_answer(kind, self.prefix.as_deref(), msg)
                }
                (ReplyKind::PublicKey, DelegateResponse::PublicKey(_)) => true,
                (ReplyKind::SigningKey, DelegateResponse::SigningKey(_)) => true,
                (ReplyKind::KnownSites, DelegateResponse::KnownSites(_)) => true,
                (ReplyKind::SiteState, DelegateResponse::SiteState { prefix, .. }) => self
                    .prefix
                    .as_deref()
                    .is_none_or(|expected| expected == prefix),
                (ReplyKind::KeyStored, DelegateResponse::KeyStored) => true,
                (ReplyKind::SitesStored, DelegateResponse::SitesStored) => true,
                (ReplyKind::SiteStateStored, DelegateResponse::SiteStateStored) => true,
                _ => false,
            }
        }
    }

    /// The current delegate's signing-key absence answer (`load_signing_key`).
    pub const ERR_NO_SIGNING_KEY: &str = "no signing key stored -- store key first";
    /// The current delegate's corrupt-stored-key answer (`parse_signing_key`).
    pub const ERR_STORED_KEY_BAD_LEN: &str = "stored key is not 32 bytes";
    /// The current delegate's `StoreSigningKey` length rejection.
    pub const ERR_STORE_KEY_BAD_LEN: &str = "signing key must be 32 bytes";
    /// Prefix of the current delegate's `GetKnownSites` decode failure.
    pub const ERR_KNOWN_SITES_DECODE_PREFIX: &str = "deserialize known sites: ";
    /// Prefix of the current delegate's site-state absence answer; the rest of
    /// the string is the site prefix, which is what makes site-state absence
    /// ATTRIBUTABLE where signing-key absence is not.
    pub const ERR_NO_SITE_BACKUP_PREFIX: &str = "no backed-up state for site ";

    /// Whether `msg` is the current delegate's "no backup" answer **about
    /// `prefix`** — as opposed to about some other site's backup, or an
    /// unrelated error entirely.
    pub fn is_no_backup_error_for(msg: &str, prefix: &str) -> bool {
        msg.strip_prefix(ERR_NO_SITE_BACKUP_PREFIX) == Some(prefix)
    }

    /// Whether an in-band `Error` with text `msg` can be the answer to an
    /// outstanding request of `kind` (about `expected_prefix`, where the
    /// request named one). See [`Expectation::matches`] for the contract.
    fn error_may_answer(kind: ReplyKind, expected_prefix: Option<&str>, msg: &str) -> bool {
        if let Some(err_prefix) = msg.strip_prefix(ERR_NO_SITE_BACKUP_PREFIX) {
            return kind == ReplyKind::SiteState
                && expected_prefix.is_none_or(|expected| expected == err_prefix);
        }
        if msg == ERR_NO_SIGNING_KEY || msg == ERR_STORED_KEY_BAD_LEN {
            // The signing-key load path answers GetPublicKey, GetSigningKey
            // and GetSigningKeyForPrefix (and the Sign* requests, which the
            // migration never awaits).
            return matches!(kind, ReplyKind::PublicKey | ReplyKind::SigningKey);
        }
        if msg == ERR_STORE_KEY_BAD_LEN {
            return kind == ReplyKind::KeyStored;
        }
        if msg.starts_with(ERR_KNOWN_SITES_DECODE_PREFIX) {
            return kind == ReplyKind::KnownSites;
        }
        // Unknown text: an older delegate generation's in-band error. Match
        // any kind — the pre-narrowing behaviour — so a legacy delegate's
        // ordinary absence answer is never turned into a 10 s timeout and a
        // false `Unresponsive` (the freenet/river#204 failure).
        true
    }

    /// The resolution state of one awaited reply.
    #[derive(Default)]
    pub struct SlotState {
        reply: Option<DelegateResponse>,
        timed_out: bool,
        waker: Option<Waker>,
    }

    /// A handle to one awaited reply, shared between the registry (which
    /// resolves it), the timeout (which expires it), and the [`ReplyFuture`]
    /// (which awaits it).
    pub type Slot = Rc<RefCell<SlotState>>;

    /// Expire `slot` if it has not been resolved, waking its awaiter so the
    /// round-trip resolves as `None` (silence) instead of hanging the walk.
    pub fn mark_timed_out(slot: &Slot) {
        let mut state = slot.borrow_mut();
        if state.reply.is_some() {
            return;
        }
        state.timed_out = true;
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }

    struct Pending {
        target: Vec<u8>,
        expectation: Expectation,
        slot: Slot,
    }

    /// The pending-reply registry: a request parks a slot, and the response
    /// handler offers every arriving reply to it.
    pub struct PendingRegistry {
        pending: RefCell<Vec<Pending>>,
    }

    impl PendingRegistry {
        /// An empty registry.
        pub const fn new() -> Self {
            Self {
                pending: RefCell::new(Vec::new()),
            }
        }

        /// Park a slot awaiting the reply to `request` from `target`, or
        /// `None` for a request outside the delegate protocol (the caller must
        /// not send it).
        pub fn register(&self, target: &DelegateKey, request: &DelegateRequest) -> Option<Slot> {
            let expectation = Expectation::of_request(request)?;
            let slot: Slot = Rc::new(RefCell::new(SlotState::default()));
            self.pending.borrow_mut().push(Pending {
                target: target.bytes().to_vec(),
                expectation,
                slot: slot.clone(),
            });
            Some(slot)
        }

        /// Offer a delegate response to any waiting round-trip, oldest waiter
        /// first. Returns `true` if it was consumed.
        pub fn offer(&self, responding_key: &DelegateKey, response: &DelegateResponse) -> bool {
            let mut pending = self.pending.borrow_mut();
            let Some(index) = pending.iter().position(|p| {
                p.target == responding_key.bytes() && p.expectation.matches(response)
            }) else {
                return false;
            };
            let entry = pending.remove(index);
            let mut slot = entry.slot.borrow_mut();
            slot.reply = Some(response.clone());
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
            true
        }

        /// How many awaits are parked. Test observability.
        pub fn pending_len(&self) -> usize {
            self.pending.borrow().len()
        }
    }

    impl Default for PendingRegistry {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Awaits a slot's resolution: `Some(reply)` or `None` on timeout.
    pub struct ReplyFuture {
        /// The slot being awaited.
        pub slot: Slot,
    }

    impl core::future::Future for ReplyFuture {
        type Output = Option<DelegateResponse>;

        fn poll(self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut state = self.slot.borrow_mut();
            if let Some(reply) = state.reply.take() {
                return Poll::Ready(Some(reply));
            }
            if state.timed_out {
                return Poll::Ready(None);
            }
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// Production transport (wasm)
// ---------------------------------------------------------------------------

/// The production transport and marker store.
///
/// Delta's WebSocket transport is fire-and-forget — `send_to_delegate_key`
/// pushes a `ClientRequest` and the reply lands later in the global
/// [`super::delegate::handle_delegate_response`] — while the library's traits
/// need an awaitable round-trip. The gap is closed by the
/// [`correlation`] registry; this module contributes only the browser glue:
/// the thread-local registry instance, the send, and the reply timeout.
#[cfg(target_arch = "wasm32")]
pub mod wasm_transport {
    use super::correlation::{self, PendingRegistry, Slot};
    use super::*;

    /// How long to wait for a reply before treating a predecessor as silent.
    ///
    /// A predecessor that does not answer within this bound is reported
    /// `Unresponsive` rather than silently treated as empty (the
    /// freenet/river#204 gate). Generous, because an old delegate's first
    /// execution on a cold node can be slow, and a false `Unresponsive` costs a
    /// user their signing key.
    const REPLY_TIMEOUT_MS: u32 = 10_000;

    thread_local! {
        static REGISTRY: PendingRegistry = const { PendingRegistry::new() };
    }

    /// Offer a delegate response to any waiting migration round-trip.
    ///
    /// Returns `true` if it was consumed by the migration. Called from
    /// [`super::super::delegate::handle_delegate_response`] BEFORE its own
    /// handling, so a reply the migration is awaiting is not also applied to UI
    /// state by the shipped sweep.
    pub fn offer_response(responding_key: &DelegateKey, response: &DelegateResponse) -> bool {
        REGISTRY.with(|registry| registry.offer(responding_key, response))
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
            let slot = match REGISTRY.with(|registry| registry.register(target, &request)) {
                Some(slot) => {
                    super::super::delegate::send_to_delegate_key_pub(&request, target.clone());
                    arm_timeout(slot.clone());
                    slot
                }
                None => {
                    // A request the delegate does not understand is never sent.
                    let slot: Slot = Slot::default();
                    correlation::mark_timed_out(&slot);
                    slot
                }
            };
            async move { Ok(correlation::ReplyFuture { slot }.await) }
        }
    }

    /// Fire a timer that marks the slot timed-out, so a silent predecessor
    /// resolves as `Ok(None)` (→ `Unresponsive`) instead of hanging the walk.
    fn arm_timeout(slot: Slot) {
        use wasm_bindgen::prelude::*;
        let cb = Closure::<dyn Fn()>::new(move || {
            correlation::mark_timed_out(&slot);
        });
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                REPLY_TIMEOUT_MS as i32,
            );
        }
        cb.forget();
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

    // -----------------------------------------------------------------------
    // The correlation boundary.
    //
    // The migration shares one reply stream, with no request ids, with the
    // shipped sweep — which sends the SAME request kinds to the SAME delegates
    // concurrently (and is FED by the migration's own replies falling through
    // to it, so the concurrency cannot be gated away). These tests drive that
    // boundary with hostile interleavings: replies that are the right kind
    // from the right delegate but answer a DIFFERENT request. The dangerous
    // property under test is that no such reply can cause a wrong write or a
    // false already-have-it skip.
    // -----------------------------------------------------------------------

    use super::super::delegate_migration_differential as diff;
    use super::correlation;

    /// Wraps the differential [`diff::FakeNode`], substituting a forged reply
    /// for requests the interceptor claims — the host-side stand-in for a
    /// mis-correlated reply picked out of the shared reply stream.
    struct HostileChannel<'a, F>
    where
        F: Fn(&DelegateKey, &DelegateRequest) -> Option<DelegateResponse>,
    {
        inner: &'a diff::FakeNode,
        intercept: F,
    }

    impl<F> DeltaDelegateChannel for HostileChannel<'_, F>
    where
        F: Fn(&DelegateKey, &DelegateRequest) -> Option<DelegateResponse>,
    {
        type Error = core::convert::Infallible;

        async fn request(
            &self,
            target: &DelegateKey,
            request: DelegateRequest,
        ) -> Result<Option<DelegateResponse>, Self::Error> {
            if let Some(forged) = (self.intercept)(target, &request) {
                return Ok(Some(forged));
            }
            self.inner.request(target, request).await
        }
    }

    // --- registry: which replies may resolve an await at all ---

    /// The error classifier narrows `Error` matching by recognizing the
    /// CURRENT delegate's error strings. That is sound only because the
    /// deployed delegate WASM cannot change without the re-key this migration
    /// exists to survive — so pin every classified string to the delegate
    /// source, exactly as `delegate_migration_is_ui_side_only` pins the slot
    /// names. If this fails, the classifier and the delegate have drifted and
    /// the narrowing is misclassifying real answers.
    #[test]
    fn error_classification_matches_the_shipped_delegate() {
        let delegate_src = include_str!("../../../delegates/site-delegate/src/lib.rs");
        for (needle, what) in [
            (
                format!("\"{}\"", correlation::ERR_NO_SIGNING_KEY),
                "signing-key absence",
            ),
            (
                format!("\"{}\"", correlation::ERR_STORED_KEY_BAD_LEN),
                "corrupt stored key",
            ),
            (
                format!("\"{}\"", correlation::ERR_STORE_KEY_BAD_LEN),
                "store-key length rejection",
            ),
            (
                format!("\"{}{{e}}\"", correlation::ERR_KNOWN_SITES_DECODE_PREFIX),
                "known-sites decode failure",
            ),
            (
                format!("\"{}{{prefix}}\"", correlation::ERR_NO_SITE_BACKUP_PREFIX),
                "site-state absence",
            ),
        ] {
            assert!(
                delegate_src.contains(&needle),
                "{what} error string drifted from the delegate: {needle} not found"
            );
        }
    }

    /// Defect 4: `Error` used to match ANY pending kind, so an error provoked
    /// by a concurrent sweep probe (post-re-key, the current delegate answers
    /// "no signing key stored" to every per-prefix probe — these are COMMON,
    /// not rare) could resolve the migration's await for a totally different
    /// kind, e.g. its known-sites read.
    #[test]
    fn a_signing_key_error_does_not_resolve_a_known_sites_await() {
        let registry = correlation::PendingRegistry::new();
        let target = diff::delegate_key([0x11; 32]);
        let slot = registry
            .register(&target, &DelegateRequest::GetKnownSites)
            .expect("GetKnownSites is a protocol request");

        let sweep_error =
            DelegateResponse::Error("no signing key stored -- store key first".into());
        assert!(
            !registry.offer(&target, &sweep_error),
            "a signing-key absence error must not resolve a KnownSites await"
        );
        assert_eq!(registry.pending_len(), 1, "the await must still be parked");

        // The genuine reply, arriving later, still resolves it.
        let genuine = DelegateResponse::KnownSites(Vec::new());
        assert!(registry.offer(&target, &genuine));
        let got = diff::block_on(correlation::ReplyFuture { slot });
        assert!(matches!(got, Some(DelegateResponse::KnownSites(_))));
    }

    /// Defect 2, transport half: a `SiteState` reply echoes its prefix, so a
    /// reply for another site's backup is detectably not ours and must fall
    /// through to the sweep (which is its real addressee) instead of resolving
    /// our await.
    #[test]
    fn a_site_state_reply_for_the_wrong_prefix_is_rejected() {
        let registry = correlation::PendingRegistry::new();
        let target = diff::delegate_key([0x22; 32]);
        let p1 = diff::prefix_of_seed(1);
        let p2 = diff::prefix_of_seed(2);
        let slot = registry
            .register(
                &target,
                &DelegateRequest::GetSiteState { prefix: p1.clone() },
            )
            .expect("GetSiteState is a protocol request");

        let other_sites_backup = DelegateResponse::SiteState {
            prefix: p2,
            state_bytes: vec![1, 2, 3],
        };
        assert!(
            !registry.offer(&target, &other_sites_backup),
            "a SiteState reply for another prefix must not resolve this await"
        );
        assert_eq!(registry.pending_len(), 1);

        let ours = DelegateResponse::SiteState {
            prefix: p1,
            state_bytes: vec![9],
        };
        assert!(registry.offer(&target, &ours));
        assert!(diff::block_on(correlation::ReplyFuture { slot }).is_some());
    }

    /// The delegate's site-state absence error carries the prefix in its text
    /// ("no backed-up state for site {prefix}", frozen in the deployed WASM),
    /// so absence is attributable: an absence error about ANOTHER prefix must
    /// not stand in for an answer about ours.
    #[test]
    fn a_site_state_absence_error_is_scoped_to_its_prefix() {
        let registry = correlation::PendingRegistry::new();
        let target = diff::delegate_key([0x33; 32]);
        let p1 = diff::prefix_of_seed(1);
        let p2 = diff::prefix_of_seed(2);
        let slot = registry
            .register(
                &target,
                &DelegateRequest::GetSiteState { prefix: p1.clone() },
            )
            .expect("GetSiteState is a protocol request");

        let other = DelegateResponse::Error(format!("no backed-up state for site {p2}"));
        assert!(
            !registry.offer(&target, &other),
            "an absence error about another prefix must not resolve this await"
        );

        let ours = DelegateResponse::Error(format!("no backed-up state for site {p1}"));
        assert!(
            registry.offer(&target, &ours),
            "the absence error about OUR prefix is the genuine answer"
        );
        assert!(matches!(
            diff::block_on(correlation::ReplyFuture { slot }),
            Some(DelegateResponse::Error(_))
        ));
    }

    /// Compat pin: an error string this module does not recognize (an older
    /// delegate generation's wording) must still match any kind — otherwise a
    /// legacy delegate's ordinary absence answer becomes a 10 s timeout and a
    /// false `Unresponsive`, the freenet/river#204 failure.
    #[test]
    fn an_unrecognized_error_string_still_resolves_a_probe() {
        let registry = correlation::PendingRegistry::new();
        let target = diff::delegate_key([0x44; 32]);
        let slot = registry
            .register(&target, &DelegateRequest::GetPublicKey)
            .expect("GetPublicKey is a protocol request");
        let vintage = DelegateResponse::Error("no key".into());
        assert!(
            registry.offer(&target, &vintage),
            "an unknown error string must conservatively answer any await"
        );
        assert!(diff::block_on(correlation::ReplyFuture { slot }).is_some());
    }

    /// Compat pin: the current delegate's signing-key absence error must keep
    /// answering a signing-key await — it is the ordinary "not stored" answer
    /// `has_signing_key` depends on. Narrowing `Error` matching must never
    /// narrow this away.
    #[test]
    fn the_signing_key_absence_error_still_answers_a_signing_key_await() {
        let registry = correlation::PendingRegistry::new();
        let target = diff::delegate_key([0x55; 32]);
        let slot = registry
            .register(
                &target,
                &DelegateRequest::GetSigningKeyForPrefix {
                    prefix: diff::prefix_of_seed(1),
                },
            )
            .expect("GetSigningKeyForPrefix is a protocol request");
        let absence = DelegateResponse::Error("no signing key stored -- store key first".into());
        assert!(registry.offer(&target, &absence));
        assert!(diff::block_on(correlation::ReplyFuture { slot }).is_some());
    }

    /// A reply from a different delegate never resolves an await, whatever its
    /// kind. (Held by the shipped code too; pinned now that it is host-testable.)
    #[test]
    fn a_reply_from_a_different_delegate_is_never_consumed() {
        let registry = correlation::PendingRegistry::new();
        let awaited = diff::delegate_key([0x66; 32]);
        let other = diff::delegate_key([0x77; 32]);
        let _slot = registry
            .register(&awaited, &DelegateRequest::GetKnownSites)
            .expect("GetKnownSites is a protocol request");
        assert!(!registry.offer(&other, &DelegateResponse::KnownSites(Vec::new())));
        assert_eq!(registry.pending_len(), 1);
    }

    // --- adapter: decisions must be safe under mis-correlation ---

    /// The last-write gate in `import_signing_key`, pinned directly: bytes
    /// that do not derive the supplied prefix are refused BEFORE any delegate
    /// round-trip, permanently (the keypair IS the site identity, so the
    /// verdict is stable across versions — never retryable). Every current
    /// caller derives the prefix from the bytes first, so this gate is
    /// defence in depth against a future caller trusting a request-side label
    /// again — and an unpinned defence-in-depth guard is worth nothing,
    /// because the change it exists to catch would delete or bypass it
    /// silently. Deleting the gate makes this test fail.
    #[test]
    fn a_key_is_never_written_under_a_prefix_it_does_not_derive() {
        use freenet_migrate::RetryAdvice;

        let fixture = diff::Fixture {
            predecessors: Vec::new(),
            successor: diff::DelegateFixture::modern(),
        };
        let node = diff::FakeNode::new(&fixture);
        let mut markers = diff::MemoryMarkers::default();
        let mut io = DeltaSuccessorIo::new(
            &node,
            &mut markers,
            diff::delegate_key(diff::successor_key_bytes()),
            None,
        );

        // Bytes that derive prefix_of_seed(2), offered under p1's slot.
        let p1 = diff::prefix_of_seed(1);
        let foreign = diff::signing_key_seed(2).to_vec();
        let result = diff::block_on(io.import_signing_key(p1.clone(), foreign));
        assert!(
            matches!(
                &result,
                ItemWrite::Failed {
                    error: WriteError::Malformed("signing key"),
                    retry: RetryAdvice::Permanent,
                }
            ),
            "a foreign key under p1 must be refused permanently, got {result:?}"
        );
        assert!(
            node.log.borrow().is_empty(),
            "the gate must refuse before ANY delegate round-trip — it may not \
             depend on what the (possibly mis-correlated) channel answers"
        );
        assert!(
            node.successor_state().per_prefix_keys.is_empty(),
            "nothing may be written to the slot"
        );

        // Control: the gate must not over-block — the same bytes under the
        // prefix they DO derive import normally.
        let own = diff::prefix_of_seed(2);
        let result =
            diff::block_on(io.import_signing_key(own.clone(), diff::signing_key_seed(2).to_vec()));
        assert!(matches!(result, ItemWrite::Written), "got {result:?}");
        assert_eq!(
            node.successor_state().per_prefix_keys.get(&own),
            Some(&diff::signing_key_seed(2)),
        );
    }

    /// Defect 3, the successor half — the data-loss headline. While the
    /// migration imports p1's key, its "does the successor already hold p1's
    /// key?" probe is answered by a reply carrying ANOTHER site's key (a
    /// concurrent sweep probe's reply; `SigningKey` carries no prefix, so the
    /// registry cannot tell). Treating that as "already have it" reports
    /// `AlreadyAuthoritative` while p1's key is never imported — silent,
    /// permanent key loss once a durable Done marker seals it.
    #[test]
    fn a_mis_correlated_signing_key_reply_cannot_fake_already_authoritative() {
        let p1 = diff::prefix_of_seed(1);
        let mut predecessor = diff::DelegateFixture::modern();
        predecessor.known_sites = vec![diff::site(&p1, "Mine")];
        predecessor
            .per_prefix_keys
            .insert(p1.clone(), diff::signing_key_seed(1));
        let fixture = diff::Fixture {
            predecessors: vec![predecessor],
            successor: diff::DelegateFixture::modern(),
        };
        let node = diff::FakeNode::new(&fixture);

        let successor_bytes = diff::successor_key_bytes();
        let p1_for_intercept = p1.clone();
        let channel = HostileChannel {
            inner: &node,
            intercept: move |target: &DelegateKey, request: &DelegateRequest| match request {
                DelegateRequest::GetSigningKeyForPrefix { prefix }
                    if target.bytes() == successor_bytes.as_slice()
                        && *prefix == p1_for_intercept =>
                {
                    // p2's key, as a sweep probe for p2 would elicit.
                    Some(DelegateResponse::SigningKey(
                        diff::signing_key_seed(2).to_vec(),
                    ))
                }
                _ => None,
            },
        };

        let mut markers = diff::MemoryMarkers::default();
        let report = diff::block_on(run_delegate_migration(
            &channel,
            &mut markers,
            diff::delegate_key(diff::successor_key_bytes()),
            &fixture.lineage(),
            Vec::new(),
        ));

        let landed = node.successor_state();
        assert_eq!(
            landed.per_prefix_keys.get(&p1),
            Some(&diff::signing_key_seed(1)),
            "p1's key must be imported: another site's key satisfying the \
             has-key probe is a mis-correlation, not evidence the successor \
             holds p1's key"
        );
        assert!(
            report.is_complete(),
            "the import went through, so the report should be clean"
        );
    }

    /// Defect 3, the predecessor half. The reply to the migration's per-prefix
    /// probe of p1 carries a DIFFERENT site's key. This is not only the
    /// concurrent case: the real delegate's `load_signing_key` FALLS BACK to
    /// the legacy single slot, so `GetSigningKeyForPrefix { p1 }` legitimately
    /// returns another site's key even sequentially. Attributing those bytes
    /// to p1 writes the wrong key under `delta:signing_key:p1`, sealed in by
    /// never-clobber — the site becomes permanently unusable while the report
    /// reads `Imported`.
    #[test]
    fn a_fallback_signing_key_reply_is_never_written_under_the_probed_prefix() {
        let p1 = diff::prefix_of_seed(1);
        let q = diff::prefix_of_seed(9);
        let mut predecessor = diff::DelegateFixture::modern();
        // p1 is listed but its per-prefix key is stored nowhere; the legacy
        // single slot holds site q's key, which the fallback serves.
        predecessor.known_sites = vec![diff::site(&p1, "Keyless")];
        predecessor.legacy_key = Some(diff::signing_key_seed(9));
        let fixture = diff::Fixture {
            predecessors: vec![predecessor],
            successor: diff::DelegateFixture::modern(),
        };
        let node = diff::FakeNode::new(&fixture);

        let mut markers = diff::MemoryMarkers::default();
        let _report = diff::block_on(run_delegate_migration(
            &node,
            &mut markers,
            diff::delegate_key(diff::successor_key_bytes()),
            &fixture.lineage(),
            Vec::new(),
        ));

        let landed = node.successor_state();
        assert_ne!(
            landed.per_prefix_keys.get(&p1),
            Some(&diff::signing_key_seed(9)),
            "q's key must never be written under p1's slot — that is the \
             permanent wrong-key corruption"
        );
        assert_eq!(
            landed.per_prefix_keys.get(&q),
            Some(&diff::signing_key_seed(9)),
            "the recovered key must land under the prefix it actually derives"
        );
    }

    /// The same gate reached the way it will actually be reached in the field,
    /// rather than by a direct call (see
    /// `a_key_is_never_written_under_a_prefix_it_does_not_derive` for the unit
    /// half): a predecessor whose `delta:signing_key:{p1}` slot ALREADY holds a
    /// different site's key.
    ///
    /// That is not hypothetical — it is precisely the wreckage a pre-fix build
    /// could have written, since the old probe path attributed a
    /// legacy-fallback reply to the probed prefix. So the first thing this
    /// migration must not do is faithfully copy that corruption forward, where
    /// never-clobber would then refuse to correct it forever.
    ///
    /// Mutation evidence, and the reason this test is worth keeping alongside
    /// the unit one: TWO independent layers stop this, and each covers for the
    /// other, so no single mutation trips this test. Breaking the enumeration
    /// re-homing alone routes the pair to `import_signing_key`, where the
    /// last-write gate refuses it; deleting the gate alone leaves the
    /// re-homing to place the key correctly. Only removing BOTH lands q's key
    /// under p1, and then this test fails. It therefore pins the *property*
    /// rather than either mechanism — which is exactly what should survive a
    /// future refactor that legitimately replaces one of the two layers.
    #[test]
    fn a_mis_slotted_predecessor_key_is_not_copied_forward() {
        let p1 = diff::prefix_of_seed(1);
        let q = diff::prefix_of_seed(9);
        let mut predecessor = diff::DelegateFixture::modern();
        // Mis-slotted: p1's slot holds q's key.
        predecessor
            .per_prefix_keys
            .insert(p1.clone(), diff::signing_key_seed(9));
        predecessor.known_sites = vec![diff::site(&p1, "Mis-slotted")];
        let fixture = diff::Fixture {
            predecessors: vec![predecessor],
            successor: diff::DelegateFixture::modern(),
        };
        let node = diff::FakeNode::new(&fixture);

        let mut markers = diff::MemoryMarkers::default();
        let _report = diff::block_on(run_delegate_migration(
            &node,
            &mut markers,
            diff::delegate_key(diff::successor_key_bytes()),
            &fixture.lineage(),
            Vec::new(),
        ));

        let landed = node.successor_state();
        assert_ne!(
            landed.per_prefix_keys.get(&p1),
            Some(&diff::signing_key_seed(9)),
            "a key that does not derive p1 must never be written under p1, \
             however it reached the writer — copying a predecessor's existing \
             mis-slotting forward makes the corruption permanent, because \
             never-clobber then refuses to correct it"
        );
        // Whatever the writer decides to do with such an item, the one thing it
        // may not do is honour the slot it was labelled with.
        if let Some(landed_under_q) = landed.per_prefix_keys.get(&q) {
            assert_eq!(
                landed_under_q,
                &diff::signing_key_seed(9),
                "if the key is re-homed at all, it goes to the prefix it derives"
            );
        }
    }

    /// Defect 1's residual (the lead analysis called `GetKnownSites`
    /// cross-matching harmless; this is the one case it is not): the
    /// read-merge-write's READ can be satisfied by a mis-correlated reply
    /// snapshotted BEFORE the migration's own previous merge was stored. The
    /// merge then rebuilds from the stale base and the follow-up
    /// `StoreKnownSites` — a whole-list REPLACE — rolls the earlier
    /// generation's contribution back off the delegate.
    ///
    /// The second write here is an OLDER generation's tombstone contribution
    /// (the only known-sites content the generation gate accepts from older
    /// generations), landing after the newest generation's site was merged; a
    /// stale-empty base for it must not unwind that site.
    #[test]
    fn a_stale_known_sites_read_cannot_roll_back_an_earlier_generations_merge() {
        let a = diff::prefix_of_seed(1);
        let gone = diff::prefix_of_seed(3);
        let mut older = diff::DelegateFixture::modern();
        older.known_sites = vec![delta_core::KnownSiteRecord::tombstone(&gone)];
        let mut newer = diff::DelegateFixture::modern();
        newer.known_sites = vec![diff::site(&a, "On the newer delegate")];
        let fixture = diff::Fixture {
            predecessors: vec![older, newer],
            successor: diff::DelegateFixture::modern(),
        };
        let node = diff::FakeNode::new(&fixture);

        // EVERY read of the successor's list is answered stale-empty — the
        // worst case for a mis-correlated snapshot.
        let successor_bytes = diff::successor_key_bytes();
        let channel = HostileChannel {
            inner: &node,
            intercept: move |target: &DelegateKey, request: &DelegateRequest| match request {
                DelegateRequest::GetKnownSites if target.bytes() == successor_bytes.as_slice() => {
                    Some(DelegateResponse::KnownSites(Vec::new()))
                }
                _ => None,
            },
        };

        let mut markers = diff::MemoryMarkers::default();
        let _report = diff::block_on(run_delegate_migration(
            &channel,
            &mut markers,
            diff::delegate_key(diff::successor_key_bytes()),
            &fixture.lineage(),
            Vec::new(),
        ));

        let landed = node.successor_state();
        let live: Vec<&str> = landed
            .known_sites
            .iter()
            .filter(|r| !r.is_tombstone())
            .map(|r| r.prefix.as_str())
            .collect();
        assert!(
            live.contains(&a.as_str()),
            "the newest generation's site must survive the older generation's \
             tombstone merge even when that merge's read base is stale; got {live:?}"
        );
        assert!(
            landed
                .known_sites
                .iter()
                .any(|r| r.is_tombstone() && r.prefix == gone),
            "and the older generation's tombstone must still be carried forward"
        );
    }

    /// Defect 2, the adapter half. The successor-side "do we already hold a
    /// backup for p1?" probe is answered by a NON-EMPTY `SiteState` reply for
    /// p2. Reading it as "yes" skips importing p1's backup with a clean
    /// report — if the network copy is gone, that backup was the user's only
    /// copy. The reply must instead be rejected as not-our-answer, leaving the
    /// item retryable (report NOT complete), never silently skipped.
    #[test]
    fn a_wrong_prefix_site_state_reply_does_not_pass_the_exists_check() {
        let p1 = diff::prefix_of_seed(1);
        let p2 = diff::prefix_of_seed(2);
        let mut predecessor = diff::DelegateFixture::modern();
        predecessor.known_sites = vec![diff::site(&p1, "Backed up")];
        predecessor.site_states.insert(p1.clone(), vec![0xAB; 16]);
        let fixture = diff::Fixture {
            predecessors: vec![predecessor],
            successor: diff::DelegateFixture::modern(),
        };
        let node = diff::FakeNode::new(&fixture);

        let successor_bytes = diff::successor_key_bytes();
        let p1_for_intercept = p1.clone();
        let channel = HostileChannel {
            inner: &node,
            intercept: move |target: &DelegateKey, request: &DelegateRequest| match request {
                DelegateRequest::GetSiteState { prefix }
                    if target.bytes() == successor_bytes.as_slice()
                        && *prefix == p1_for_intercept =>
                {
                    Some(DelegateResponse::SiteState {
                        prefix: p2.clone(),
                        state_bytes: vec![0xCD; 8],
                    })
                }
                _ => None,
            },
        };

        let mut markers = diff::MemoryMarkers::default();
        let report = diff::block_on(run_delegate_migration(
            &channel,
            &mut markers,
            diff::delegate_key(diff::successor_key_bytes()),
            &fixture.lineage(),
            Vec::new(),
        ));

        assert!(
            !report.is_complete(),
            "a mis-correlated exists-check answer must leave the backup item \
             retryable, never silently skipped with a clean report"
        );
        assert!(
            node.successor_state().site_states.is_empty(),
            "and nothing may be written from an unverified answer"
        );
    }
}
