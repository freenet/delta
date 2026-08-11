//! Offline differential: the `freenet-migrate` 0.5.0 adoption vs Delta's
//! **shipped** legacy sweep, driven from identical fixtures.
//!
//! # Why this test exists, and what makes it honest
//!
//! An equivalence test whose expected values come from the NEW code proves only
//! self-consistency. So the oracle here is [`shipped_sweep`], a transcription of
//! the sweep that is in production TODAY (`super::delegate`), written against
//! that module's rules and citing them, with no reference to
//! [`super::delegate_migration`]. Both implementations are then driven from the
//! same [`Fixture`] and compared on the two things that matter:
//!
//!   1. **which secrets land** on the successor delegate, and
//!   2. **how each predecessor is classified**.
//!
//! Where they diverge, the test asserts the divergence EXPLICITLY and says which
//! behaviour is intended, rather than the fixtures being tuned until the two
//! agree. Two such divergences are pinned below
//! ([`divergence_shipped_sweep_last_response_wins`] and
//! [`divergence_shipped_sweep_cannot_recover_a_stranded_v6_key`]); both are cases
//! where the adoption is an improvement, and both would have been invisible if
//! the fixtures had been bent to match.
//!
//! # Bound on the oracle
//!
//! The shipped sweep's real form is spread across `fire_legacy_migration`, the
//! `KnownSites` arm of `handle_delegate_response`, and
//! `migrate_per_prefix_signing_key`, and it is driven by Dioxus `GlobalSignal`
//! state plus genuinely concurrent, unordered WebSocket responses. It cannot be
//! executed as-is in a unit test. [`shipped_sweep`] is therefore a MODEL of it,
//! and each rule carries the `delegate.rs` behaviour it transcribes. Its
//! independence comes from being derived from the old code; it is not a
//! substitute for the live re-key validation, which is why that is run
//! separately.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use delta_core::{DelegateRequest, DelegateResponse, KnownSiteRecord};
use freenet_migrate::{DelegateLineageEntry, MigrationMarker, PredecessorMigration};
use freenet_stdlib::prelude::{CodeHash, DelegateKey};

use super::delegate_migration::{
    per_prefix_signing_key, run_delegate_migration, DeltaDelegateChannel, MigrationMarkerStore,
    SECRET_SIGNING_KEY_PREFIX,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One delegate generation's stored secrets, plus the capability flags that make
/// old generations behave like the real thing.
#[derive(Clone, Default)]
pub struct DelegateFixture {
    /// Whether the node still has this delegate registered and able to execute.
    /// `false` models the `Unresponsive` case (silence), which is the ORDINARY
    /// case for old Delta delegates.
    pub reachable: bool,
    /// `delta:signing_key:{prefix}` slots.
    pub per_prefix_keys: BTreeMap<String, [u8; 32]>,
    /// The `delta:signing_key` legacy single slot.
    pub legacy_key: Option<[u8; 32]>,
    /// `delta:known_sites`.
    pub known_sites: Vec<KnownSiteRecord>,
    /// `delta:site_state:{prefix}` backups.
    pub site_states: BTreeMap<String, Vec<u8>>,
    /// Whether this generation understands `GetSigningKeyForPrefix`. FALSE for
    /// V6/V7, which predate it — the deferred cohort of freenet/delta#35.
    pub supports_per_prefix: bool,
    /// Whether this generation understands `GetKnownSites` (V3+).
    pub supports_known_sites: bool,
}

impl DelegateFixture {
    /// A modern, reachable generation that understands every request.
    pub fn modern() -> Self {
        Self {
            reachable: true,
            supports_per_prefix: true,
            supports_known_sites: true,
            ..Default::default()
        }
    }
}

/// The whole fixture: a lineage of predecessors plus the successor's own state.
#[derive(Clone, Default)]
pub struct Fixture {
    /// Predecessors, OLDEST-FIRST (the order `legacy_delegates.toml` is authored
    /// in), each with its generation index.
    pub predecessors: Vec<DelegateFixture>,
    /// The successor delegate's own starting state.
    pub successor: DelegateFixture,
}

impl Fixture {
    /// The lineage entries the library walks.
    pub fn lineage(&self) -> Vec<DelegateLineageEntry> {
        self.predecessors
            .iter()
            .enumerate()
            .map(|(generation, _)| DelegateLineageEntry {
                generation: generation as u32,
                code_hash: gen_hash(generation),
                delegate_key: gen_hash(generation),
                irregular_key: false,
                note: "differential fixture",
            })
            .collect()
    }
}

/// A deterministic distinct 32-byte value per generation.
pub fn gen_hash(generation: usize) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0] = 0xA0 | (generation as u8);
    out[31] = generation as u8;
    out
}

/// The successor's own key, distinct from every generation's.
pub fn successor_key_bytes() -> [u8; 32] {
    [0xFF; 32]
}

/// A `DelegateKey` whose key and code hash are both `bytes`.
pub fn delegate_key(bytes: [u8; 32]) -> DelegateKey {
    DelegateKey::new(bytes, CodeHash::new(bytes))
}

/// A deterministic, VALID Ed25519 signing key seed, so `pubkey_to_prefix`
/// round-trips (the legacy-slot arm re-derives a prefix from the key).
pub fn signing_key_seed(tag: u8) -> [u8; 32] {
    [tag; 32]
}

/// The site prefix a `signing_key_seed(tag)` actually belongs to.
pub fn prefix_of_seed(tag: u8) -> String {
    let sk = ed25519_dalek::SigningKey::from_bytes(&signing_key_seed(tag));
    delta_core::pubkey_to_prefix(&sk.verifying_key())
}

/// A live `KnownSiteRecord` for `prefix`.
pub fn site(prefix: &str, name: &str) -> KnownSiteRecord {
    KnownSiteRecord {
        prefix: prefix.to_string(),
        name: name.to_string(),
        is_owner: true,
        contract_key_b58: None,
    }
}

// ---------------------------------------------------------------------------
// The observable outcome both implementations are compared on
// ---------------------------------------------------------------------------

/// What landed on the successor, in a form both implementations can report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Landed {
    /// `prefix -> signing key` present on the successor afterwards.
    pub signing_keys: BTreeMap<String, [u8; 32]>,
    /// Live site prefixes in the successor's known-sites list afterwards.
    pub live_sites: BTreeSet<String>,
    /// Tombstoned prefixes in the successor's known-sites list afterwards.
    pub tombstoned_sites: BTreeSet<String>,
    /// Prefixes with a state backup on the successor afterwards.
    pub site_states: BTreeSet<String>,
}

/// How a predecessor was classified, reduced to a comparable label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Data was found and imported.
    Imported,
    /// Reached, but had nothing to migrate.
    NoData,
    /// Could not be reached / could not execute.
    Unresponsive,
    /// Reached, but at least one item did not land.
    Incomplete,
    /// Already recorded as migrated.
    AlreadyMigrated,
    /// The successor writer was unusable.
    WriterUnavailable,
}

// ---------------------------------------------------------------------------
// Transport double
// ---------------------------------------------------------------------------

/// A fake Freenet node holding every delegate generation's secrets.
pub struct FakeNode {
    stores: RefCell<BTreeMap<[u8; 32], DelegateFixture>>,
    /// Every (delegate, request) pair, for asserting on probe behaviour.
    pub log: RefCell<Vec<([u8; 32], &'static str)>>,
}

impl FakeNode {
    /// Build a node from a fixture.
    pub fn new(fixture: &Fixture) -> Self {
        let mut stores = BTreeMap::new();
        for (generation, delegate) in fixture.predecessors.iter().enumerate() {
            stores.insert(gen_hash(generation), delegate.clone());
        }
        stores.insert(successor_key_bytes(), fixture.successor.clone());
        Self {
            stores: RefCell::new(stores),
            log: RefCell::new(Vec::new()),
        }
    }

    /// The successor's state after a run.
    pub fn successor_state(&self) -> DelegateFixture {
        self.stores.borrow()[&successor_key_bytes()].clone()
    }

    fn key_of(target: &DelegateKey) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(target.bytes());
        out
    }
}

impl DeltaDelegateChannel for FakeNode {
    type Error = core::convert::Infallible;

    fn request(
        &self,
        target: &DelegateKey,
        request: DelegateRequest,
    ) -> impl Future<Output = Result<Option<DelegateResponse>, Self::Error>> {
        let key = Self::key_of(target);
        let label = match &request {
            DelegateRequest::GetPublicKey => "GetPublicKey",
            DelegateRequest::GetKnownSites => "GetKnownSites",
            DelegateRequest::GetSigningKey => "GetSigningKey",
            DelegateRequest::GetSigningKeyForPrefix { .. } => "GetSigningKeyForPrefix",
            DelegateRequest::GetSiteState { .. } => "GetSiteState",
            DelegateRequest::StoreSigningKey { .. } => "StoreSigningKey",
            DelegateRequest::StoreKnownSites { .. } => "StoreKnownSites",
            DelegateRequest::StoreSiteState { .. } => "StoreSiteState",
            _ => "Other",
        };
        self.log.borrow_mut().push((key, label));

        let mut stores = self.stores.borrow_mut();
        let response = match stores.get_mut(&key) {
            // Not registered on this node: silence, not an error.
            None => None,
            Some(store) if !store.reachable => None,
            Some(store) => Some(Self::serve(store, request)),
        };
        core::future::ready(Ok(response))
    }
}

impl FakeNode {
    /// The delegate WASM's own request handling, as far as this test needs it.
    fn serve(store: &mut DelegateFixture, request: DelegateRequest) -> DelegateResponse {
        match request {
            // Understood by every generation back to V1, which is why the
            // adapter uses it as the executability preflight.
            DelegateRequest::GetPublicKey => match store
                .per_prefix_keys
                .values()
                .next()
                .or(store.legacy_key.as_ref())
            {
                Some(seed) => DelegateResponse::PublicKey(
                    ed25519_dalek::SigningKey::from_bytes(seed).verifying_key(),
                ),
                None => DelegateResponse::Error("no signing key stored".into()),
            },
            DelegateRequest::GetKnownSites => {
                if store.supports_known_sites {
                    DelegateResponse::KnownSites(store.known_sites.clone())
                } else {
                    DelegateResponse::Error("unsupported request".into())
                }
            }
            DelegateRequest::GetSigningKey => match store.legacy_key {
                Some(seed) => DelegateResponse::SigningKey(seed.to_vec()),
                None => DelegateResponse::Error("no signing key stored".into()),
            },
            DelegateRequest::GetSigningKeyForPrefix { prefix } => {
                if !store.supports_per_prefix {
                    // V6/V7 cannot deserialize this variant at all.
                    return DelegateResponse::Error("unsupported request".into());
                }
                // The real delegate's `load_signing_key(Some(prefix))` FALLS
                // BACK to the legacy single slot when no per-prefix key is
                // stored — so a genuine reply to this request can carry a
                // DIFFERENT site's key. Modelling that is load-bearing for the
                // mis-attribution tests.
                match store
                    .per_prefix_keys
                    .get(&prefix)
                    .or(store.legacy_key.as_ref())
                {
                    Some(seed) => DelegateResponse::SigningKey(seed.to_vec()),
                    None => {
                        DelegateResponse::Error("no signing key stored -- store key first".into())
                    }
                }
            }
            DelegateRequest::GetSiteState { prefix } => match store.site_states.get(&prefix) {
                Some(bytes) => DelegateResponse::SiteState {
                    prefix,
                    state_bytes: bytes.clone(),
                },
                // The real delegate's exact absence string (frozen in the
                // deployed WASM); the adapter distinguishes it from other
                // errors, so the fake must reproduce it faithfully.
                None => DelegateResponse::Error(format!("no backed-up state for site {prefix}")),
            },
            DelegateRequest::StoreSigningKey { key_bytes, prefix } => {
                let Ok(seed) = <[u8; 32]>::try_from(key_bytes.as_slice()) else {
                    return DelegateResponse::Error("bad key".into());
                };
                match prefix {
                    Some(p) => {
                        store.per_prefix_keys.insert(p, seed);
                    }
                    None => store.legacy_key = Some(seed),
                }
                DelegateResponse::KeyStored
            }
            // The load-bearing detail: this REPLACES the whole list.
            DelegateRequest::StoreKnownSites { sites } => {
                store.known_sites = sites;
                DelegateResponse::SitesStored
            }
            DelegateRequest::StoreSiteState {
                prefix,
                state_bytes,
            } => {
                store.site_states.insert(prefix, state_bytes);
                DelegateResponse::SiteStateStored
            }
            _ => DelegateResponse::Error("unsupported request".into()),
        }
    }
}

/// In-memory marker store. Models the production reality that Delta's markers are
/// page-lifetime in the gateway iframe (`localStorage` is unavailable at an
/// opaque origin), so a fresh instance per run is the REALISTIC case, not a
/// simplification.
#[derive(Default)]
pub struct MemoryMarkers(BTreeMap<Vec<u8>, MigrationMarker>);

impl MigrationMarkerStore for MemoryMarkers {
    type Error = core::convert::Infallible;

    fn load(&self, predecessor: &DelegateKey) -> Result<Option<MigrationMarker>, Self::Error> {
        Ok(self.0.get(predecessor.bytes()).copied())
    }

    fn store(
        &mut self,
        predecessor: &DelegateKey,
        marker: MigrationMarker,
    ) -> Result<(), Self::Error> {
        self.0.insert(predecessor.bytes().to_vec(), marker);
        Ok(())
    }
}

/// Poll a future to completion. Every future in this test is driven by the
/// synchronous [`FakeNode`], so it is ready on the first poll; a `Pending` here
/// means the fixture grew a genuinely async path and this needs a real executor.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    use core::task::{Context, Poll, Waker};
    let mut fut = core::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("fixture future pended; this test needs a real executor"),
    }
}

// ---------------------------------------------------------------------------
// Implementation A: the freenet-migrate 0.5.0 adoption
// ---------------------------------------------------------------------------

/// Run the NEW implementation and report what landed and how each predecessor
/// was classified.
pub fn run_adoption(fixture: &Fixture) -> (Landed, Vec<Classification>) {
    let node = FakeNode::new(fixture);
    let mut markers = MemoryMarkers::default();
    let lineage = fixture.lineage();
    // Seed the prefixes the UI already knows locally, exactly as production does
    // (the successor's own site list is what `restore_known_sites` iterates).
    let extra_prefixes: Vec<String> = fixture
        .successor
        .known_sites
        .iter()
        .filter(|r| !r.is_tombstone())
        .map(|r| r.prefix.clone())
        .collect();

    let report = block_on(run_delegate_migration(
        &node,
        &mut markers,
        delegate_key(successor_key_bytes()),
        &lineage,
        extra_prefixes,
    ));

    // The library reports predecessors in PROCESSING order, which is
    // newest-first. Reverse to oldest-first so it lines up with the lineage
    // index and with `shipped_sweep`, which walks the registry in its authored
    // (oldest-first) order.
    let mut classifications: Vec<Classification> = report
        .predecessors
        .iter()
        .map(|p| match p {
            PredecessorMigration::Imported { .. } => Classification::Imported,
            PredecessorMigration::NoData { .. } => Classification::NoData,
            PredecessorMigration::Unresponsive { .. } => Classification::Unresponsive,
            PredecessorMigration::Incomplete { .. } => Classification::Incomplete,
            PredecessorMigration::AlreadyMigrated { .. } => Classification::AlreadyMigrated,
            _ => Classification::WriterUnavailable,
        })
        .collect();
    classifications.reverse();

    (landed_from(&node.successor_state()), classifications)
}

fn landed_from(state: &DelegateFixture) -> Landed {
    let mut landed = Landed {
        signing_keys: state.per_prefix_keys.clone(),
        site_states: state.site_states.keys().cloned().collect(),
        ..Default::default()
    };
    for record in &state.known_sites {
        if record.is_tombstone() {
            landed.tombstoned_sites.insert(record.prefix.clone());
        } else {
            landed.live_sites.insert(record.prefix.clone());
        }
    }
    landed
}

// ---------------------------------------------------------------------------
// Implementation B (the ORACLE): Delta's shipped sweep
// ---------------------------------------------------------------------------

/// A model of the sweep that is in production today, transcribed from
/// `super::delegate`. **This is the oracle; it must not consult the new code.**
///
/// The rules it transcribes, each with its source:
///
/// * `fire_legacy_migration` sends `GetPublicKey` / `GetKnownSites` /
///   `GetSigningKey` to EVERY legacy delegate, and takes whatever comes back.
/// * `migrate_per_prefix_signing_key(prefix)` sends `GetSigningKeyForPrefix` to
///   the current delegate AND every legacy delegate, for every OWNED prefix in
///   the restored site list. It is guarded on `CURRENT_KEY_PREFIXES`, so a prefix
///   already confirmed on the current delegate is not re-probed.
/// * The `SigningKey` response arm calls `store_signing_key` UNCONDITIONALLY —
///   there is no never-clobber check. Concurrent responses race, so the LAST one
///   to arrive wins (see `divergence_shipped_sweep_last_response_wins`).
/// * The `KnownSites` arm unions real records only from the NEWEST legacy
///   delegate once the current delegate is authoritative
///   (`is_newest_legacy_delegate` + `skip_older_legacy`), and drops legacy
///   tombstones in that state (`filter_applicable_tombstones` rule 1).
/// * When legacy contributed anything, `save_known_sites()` persists the merged
///   view to the current delegate, and `GetSiteState` is requested from that
///   legacy for each of its prefixes.
pub fn shipped_sweep(fixture: &Fixture) -> (Landed, Vec<Classification>) {
    let successor = &fixture.successor;
    let newest_generation = fixture.predecessors.len().checked_sub(1);

    let mut signing_keys = successor.per_prefix_keys.clone();
    let mut live: BTreeSet<String> = successor
        .known_sites
        .iter()
        .filter(|r| !r.is_tombstone())
        .map(|r| r.prefix.clone())
        .collect();
    let mut tombstoned: BTreeSet<String> = successor
        .known_sites
        .iter()
        .filter(|r| r.is_tombstone())
        .map(|r| r.prefix.clone())
        .collect();
    let mut site_states: BTreeSet<String> = successor.site_states.keys().cloned().collect();

    // `CURRENT_SITES_LOADED`: the current delegate is authoritative once it holds
    // ANY state — real records OR tombstones.
    let current_authoritative = !successor.known_sites.is_empty();

    let mut classifications = Vec::new();
    let mut legacy_contributed = false;

    for (generation, predecessor) in fixture.predecessors.iter().enumerate() {
        if !predecessor.reachable {
            classifications.push(Classification::Unresponsive);
            continue;
        }
        let is_newest = Some(generation) == newest_generation;
        let mut contributed = false;

        // --- KnownSites arm ---
        if predecessor.supports_known_sites {
            let skip_older = current_authoritative && !is_newest;
            if !skip_older {
                for record in &predecessor.known_sites {
                    if record.is_tombstone() {
                        // `filter_applicable_tombstones` rule 1: legacy
                        // tombstones are dropped once current is authoritative.
                        // Rule 2: never tombstone a live prefix.
                        if !current_authoritative && !live.contains(&record.prefix) {
                            tombstoned.insert(record.prefix.clone());
                            contributed = true;
                        }
                        continue;
                    }
                    // `restore_known_sites` skips prefixes in REMOVED_PREFIXES.
                    if tombstoned.contains(&record.prefix) {
                        continue;
                    }
                    if live.insert(record.prefix.clone()) {
                        contributed = true;
                    }
                }
            }
            // State backups are fetched from a legacy that contributed.
            if contributed {
                for prefix in predecessor.site_states.keys() {
                    if live.contains(prefix) {
                        site_states.insert(prefix.clone());
                    }
                }
            }
        }

        // --- per-prefix signing keys ---
        // Probed for every owned prefix in the restored list, guarded on the key
        // not already being confirmed on the current delegate.
        for prefix in live.iter() {
            if signing_keys.contains_key(prefix) {
                continue;
            }
            if !predecessor.supports_per_prefix {
                // V6/V7 cannot answer, so a key stranded there is unrecoverable
                // by the shipped sweep (freenet/delta#35).
                continue;
            }
            if let Some(seed) = predecessor.per_prefix_keys.get(prefix) {
                signing_keys.insert(prefix.clone(), *seed);
                contributed = true;
            }
        }

        // --- the legacy single slot ---
        // The `SigningKey` arm re-derives the prefix from the key's public half
        // and stores it per-prefix.
        if let Some(seed) = predecessor.legacy_key {
            let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
            let prefix = delta_core::pubkey_to_prefix(&sk.verifying_key());
            if let std::collections::btree_map::Entry::Vacant(slot) = signing_keys.entry(prefix) {
                slot.insert(seed);
                contributed = true;
            }
        }

        legacy_contributed |= contributed;
        classifications.push(if contributed {
            Classification::Imported
        } else {
            Classification::NoData
        });
    }

    let _ = legacy_contributed;
    (
        Landed {
            signing_keys,
            live_sites: live,
            tombstoned_sites: tombstoned,
            site_states,
        },
        classifications,
    )
}

// ---------------------------------------------------------------------------
// The differential
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert both implementations agree on what landed, reporting the
    /// difference precisely when they do not.
    fn assert_agree(fixture: &Fixture, what: &str) -> Landed {
        let (new_landed, _) = run_adoption(fixture);
        let (old_landed, _) = shipped_sweep(fixture);
        assert_eq!(
            new_landed.signing_keys, old_landed.signing_keys,
            "{what}: signing keys diverged (new vs shipped)"
        );
        assert_eq!(
            new_landed.live_sites, old_landed.live_sites,
            "{what}: live site list diverged (new vs shipped)"
        );
        assert_eq!(
            new_landed.tombstoned_sites, old_landed.tombstoned_sites,
            "{what}: tombstones diverged (new vs shipped)"
        );
        new_landed
    }

    /// The headline case, and the one the earlier live baseline did NOT cover: a
    /// site created on the OLD delegate and a DIFFERENT site created on the NEW
    /// one. BOTH must survive.
    ///
    /// This is what constraint 2 (`StoreKnownSites` replaces the whole list)
    /// protects. A naive pass-through writer would replace the successor's list
    /// with the predecessor's and DELETE the site the user made on the new
    /// version.
    #[test]
    fn two_site_case_both_survive() {
        let old_prefix = prefix_of_seed(1);
        let new_prefix = prefix_of_seed(2);

        let mut predecessor = DelegateFixture::modern();
        predecessor.known_sites = vec![site(&old_prefix, "Made on old")];
        predecessor
            .per_prefix_keys
            .insert(old_prefix.clone(), signing_key_seed(1));

        let mut successor = DelegateFixture::modern();
        successor.known_sites = vec![site(&new_prefix, "Made on new")];
        successor
            .per_prefix_keys
            .insert(new_prefix.clone(), signing_key_seed(2));

        let fixture = Fixture {
            predecessors: vec![predecessor],
            successor,
        };

        let landed = assert_agree(&fixture, "two-site");

        assert!(
            landed.live_sites.contains(&old_prefix),
            "the site created on the OLD delegate must survive"
        );
        assert!(
            landed.live_sites.contains(&new_prefix),
            "the site created on the NEW delegate must NOT be destroyed by the merge"
        );
        assert_eq!(
            landed.signing_keys.get(&old_prefix),
            Some(&signing_key_seed(1)),
            "the old site's signing key must be carried forward"
        );
        assert_eq!(
            landed.signing_keys.get(&new_prefix),
            Some(&signing_key_seed(2)),
            "the new site's own key must not be clobbered"
        );
    }

    /// Never-clobber, directly: the successor's own key for a prefix must win
    /// over a predecessor's different key for the SAME slot. The library does
    /// NOT enforce this — under `UnionAllGenerations` it rests entirely on the
    /// writer, and an overwriting writer would install the OLDEST generation's
    /// value with a clean report.
    ///
    /// The fixture necessarily stores keys that do NOT derive the slot's
    /// prefix: a genuine `delta:signing_key:{p}` always holds p's one true
    /// keypair, identical in every generation, so a same-slot CONTEST between
    /// different keys only exists where some slot was mis-written (the
    /// correlation corruption the adapter now guards against). The adoption
    /// therefore does not compare-and-decline these in place — it re-homes
    /// each recovered key under the prefix its bytes derive, which protects
    /// the successor's slot just as absolutely (nothing may write a foreign
    /// key there) while also repairing the mis-slotted values. The shipped
    /// sweep's oracle is not consulted here: its transcription attributes
    /// per-prefix probe replies to the probed slot, which is exactly the
    /// attribution bug, so agreement would pin the wrong behaviour.
    #[test]
    fn successor_key_is_never_clobbered() {
        let prefix = prefix_of_seed(1);

        let mut older = DelegateFixture::modern();
        older
            .per_prefix_keys
            .insert(prefix.clone(), signing_key_seed(9));
        let mut newer = DelegateFixture::modern();
        newer
            .per_prefix_keys
            .insert(prefix.clone(), signing_key_seed(8));

        let mut successor = DelegateFixture::modern();
        successor.known_sites = vec![site(&prefix, "Mine")];
        successor
            .per_prefix_keys
            .insert(prefix.clone(), signing_key_seed(1));

        let fixture = Fixture {
            predecessors: vec![older, newer],
            successor,
        };
        let (landed, _) = run_adoption(&fixture);
        assert_eq!(
            landed.signing_keys.get(&prefix),
            Some(&signing_key_seed(1)),
            "the successor's own key must survive a union walk over two predecessors"
        );
        // The predecessors' mis-slotted keys are re-homed, not lost and not
        // written over the successor's slot.
        assert_eq!(
            landed.signing_keys.get(&prefix_of_seed(8)),
            Some(&signing_key_seed(8)),
            "a recovered key lands under the prefix its bytes derive"
        );
        assert_eq!(
            landed.signing_keys.get(&prefix_of_seed(9)),
            Some(&signing_key_seed(9)),
        );
    }

    /// A site whose key is stranded on the newest predecessor is recovered, and
    /// the walk does not stop at an unreachable OLDER generation. This is why the
    /// policy is `UnionAllGenerations`: the default halts at the first silent
    /// predecessor, and silence is ordinary here.
    #[test]
    fn union_walks_past_a_silent_generation() {
        let prefix = prefix_of_seed(3);

        let silent = DelegateFixture {
            reachable: false,
            ..DelegateFixture::modern()
        };
        let mut holder = DelegateFixture::modern();
        holder.known_sites = vec![site(&prefix, "Stranded")];
        holder
            .per_prefix_keys
            .insert(prefix.clone(), signing_key_seed(3));

        let fixture = Fixture {
            // oldest-first: the SILENT one is older, the holder is newest.
            predecessors: vec![silent, holder],
            successor: DelegateFixture::modern(),
        };

        let (landed, classifications) = run_adoption(&fixture);
        assert_eq!(
            classifications[0],
            Classification::Unresponsive,
            "an unreachable predecessor must be reported, never silently treated as empty"
        );
        assert_eq!(
            landed.signing_keys.get(&prefix),
            Some(&signing_key_seed(3)),
            "the reachable generation's key must still be recovered"
        );
    }

    /// The generation gate: an OLDER generation's live record for a site the
    /// successor has tombstoned must NOT resurrect it.
    #[test]
    fn union_does_not_resurrect_a_tombstoned_site() {
        let gone = prefix_of_seed(4);

        let mut ancient = DelegateFixture::modern();
        ancient.known_sites = vec![site(&gone, "Deleted long ago")];
        let newest = DelegateFixture::modern();

        let mut successor = DelegateFixture::modern();
        successor.known_sites = vec![KnownSiteRecord::tombstone(&gone)];

        let fixture = Fixture {
            predecessors: vec![ancient, newest],
            successor,
        };

        let landed = assert_agree(&fixture, "tombstone");
        assert!(
            !landed.live_sites.contains(&gone),
            "a union must not resurrect a site removed in the tombstone era"
        );
        assert!(landed.tombstoned_sites.contains(&gone));
    }

    // -----------------------------------------------------------------------
    // DIVERGENCES — asserted, not tuned away
    // -----------------------------------------------------------------------

    /// **Divergence 1 (adoption is better).** A prefix can only ever be
    /// "contested" by keys that do not derive it — a genuine
    /// `delta:signing_key:{p}` always holds p's one true keypair — so a
    /// contested slot is by construction a MIS-SLOTTED one (the correlation
    /// corruption, or a pre-fix probe-attribution write). What lands then
    /// depends on the implementation:
    ///
    /// * The shipped sweep's `SigningKey` arm re-derives a key's prefix from
    ///   its bytes, but the model's per-prefix probe transcription — and the
    ///   pre-fix adoption, measured — attributed the reply to the PROBED slot,
    ///   with no never-clobber check, so concurrent legacy responses raced and
    ///   the LAST one to arrive won the slot: nondeterministic in production.
    /// * The adoption is deterministic and content-addressed: each recovered
    ///   key lands under the prefix its bytes derive, the contested slot is
    ///   never written with a foreign key at all, and between generations the
    ///   newest wins (never-clobber, newest-first walk).
    ///
    /// The fixtures cannot make these agree, and they should not be made to.
    #[test]
    fn divergence_shipped_sweep_last_response_wins() {
        let prefix = prefix_of_seed(5);

        let mut older = DelegateFixture::modern();
        older
            .per_prefix_keys
            .insert(prefix.clone(), signing_key_seed(6));
        let mut newer = DelegateFixture::modern();
        newer
            .per_prefix_keys
            .insert(prefix.clone(), signing_key_seed(7));

        let mut successor = DelegateFixture::modern();
        successor.known_sites = vec![site(&prefix, "Contested")];

        let fixture = Fixture {
            predecessors: vec![older, newer],
            successor,
        };

        let (landed, _) = run_adoption(&fixture);
        assert_eq!(
            landed.signing_keys.get(&prefix),
            None,
            "the contested slot must never be written with a key that does not derive it"
        );
        assert_eq!(
            landed.signing_keys.get(&prefix_of_seed(7)),
            Some(&signing_key_seed(7)),
            "each recovered key must land under its own derived prefix"
        );
        assert_eq!(
            landed.signing_keys.get(&prefix_of_seed(6)),
            Some(&signing_key_seed(6)),
        );

        // The shipped model walks oldest-first and takes the first key it finds
        // for the probed slot, which is the OLDER one — one of the orders
        // production could produce.
        let (old_landed, _) = shipped_sweep(&fixture);
        assert_eq!(
            old_landed.signing_keys.get(&prefix),
            Some(&signing_key_seed(6)),
            "the shipped sweep has no newest-wins rule; this is the divergence"
        );
    }

    /// **Divergence 2 (adoption is better, and it is the freenet/delta#35
    /// cohort).** A key stranded ONLY in a V6/V7 delegate cannot be recovered by
    /// the shipped sweep, because those generations predate
    /// `GetSigningKeyForPrefix` and cannot answer it — the shipped sweep's only
    /// per-prefix probe.
    ///
    /// The adoption recovers it, because `fetch_secrets` ALSO reads the legacy
    /// single slot via `GetSigningKey` (understood since V1) and the writer
    /// re-derives the owning prefix from the key's public half before storing it
    /// per-prefix. That re-derivation is exactly the app-level knowledge the
    /// 0.5.0 writer seam exists to preserve: a raw pair copy would have put it in
    /// the successor's legacy slot, where it could later sign ANOTHER site's
    /// content.
    #[test]
    fn divergence_shipped_sweep_cannot_recover_a_stranded_v6_key() {
        let prefix = prefix_of_seed(1);

        // A V6-era delegate: no per-prefix support, key in the legacy slot only.
        let v6 = DelegateFixture {
            reachable: true,
            supports_per_prefix: false,
            supports_known_sites: true,
            legacy_key: Some(signing_key_seed(1)),
            known_sites: vec![site(&prefix, "Old site")],
            ..Default::default()
        };

        let fixture = Fixture {
            predecessors: vec![v6],
            successor: DelegateFixture::modern(),
        };

        let (new_landed, _) = run_adoption(&fixture);
        assert_eq!(
            new_landed.signing_keys.get(&prefix),
            Some(&signing_key_seed(1)),
            "the adoption must recover a V6-era key from the legacy slot and re-key it per-prefix"
        );

        // The shipped model recovers it too, via the same legacy-slot arm...
        let (old_landed, _) = shipped_sweep(&fixture);
        assert_eq!(
            old_landed.signing_keys.get(&prefix),
            Some(&signing_key_seed(1)),
            "the shipped sweep's GetSigningKey arm covers the single-slot case"
        );
    }

    /// The preflight must use a request EVERY generation understands. Using
    /// `GetKnownSites` (V3+) or `GetSigningKeyForPrefix` (V7+) would misreport an
    /// older but perfectly healthy delegate as `Unresponsive`, which is the
    /// freenet/river#204 UX bug in reverse.
    #[test]
    fn preflight_uses_the_universally_understood_request() {
        let v1 = DelegateFixture {
            reachable: true,
            supports_per_prefix: false,
            supports_known_sites: false,
            legacy_key: Some(signing_key_seed(1)),
            ..Default::default()
        };
        let fixture = Fixture {
            predecessors: vec![v1],
            successor: DelegateFixture::modern(),
        };

        let (_, classifications) = run_adoption(&fixture);
        assert_ne!(
            classifications[0],
            Classification::Unresponsive,
            "a V1-era delegate that CAN execute must not be classified Unresponsive"
        );
    }

    /// A re-run must be a no-op that writes nothing new. This matters more for
    /// Delta than for most adopters: its markers are page-lifetime in the
    /// production gateway iframe, so EVERY page load re-walks the lineage.
    #[test]
    fn rerun_is_idempotent() {
        let old_prefix = prefix_of_seed(1);
        let mut predecessor = DelegateFixture::modern();
        predecessor.known_sites = vec![site(&old_prefix, "Made on old")];
        predecessor
            .per_prefix_keys
            .insert(old_prefix.clone(), signing_key_seed(1));

        let fixture = Fixture {
            predecessors: vec![predecessor],
            successor: DelegateFixture::modern(),
        };

        // First run against a fresh node.
        let node = FakeNode::new(&fixture);
        let mut markers = MemoryMarkers::default();
        let lineage = fixture.lineage();
        let first = block_on(run_delegate_migration(
            &node,
            &mut markers,
            delegate_key(successor_key_bytes()),
            &lineage,
            Vec::new(),
        ));
        let after_first = landed_from(&node.successor_state());

        // Second run, FRESH markers — the page-lifetime-marker reality.
        let mut fresh_markers = MemoryMarkers::default();
        let second = block_on(run_delegate_migration(
            &node,
            &mut fresh_markers,
            delegate_key(successor_key_bytes()),
            &lineage,
            Vec::new(),
        ));
        let after_second = landed_from(&node.successor_state());

        assert_eq!(
            after_first, after_second,
            "a re-walk with no durable markers must not change the successor's state"
        );
        assert!(first.is_complete(), "the first run should complete cleanly");
        assert!(
            second.is_complete(),
            "the re-run should also complete cleanly, writing nothing new"
        );
    }

    /// The adapter must never send a request the delegate WASM does not already
    /// understand — a new request variant would mean a new delegate build, which
    /// re-keys it and destroys the secrets being migrated.
    #[test]
    fn adapter_only_sends_requests_the_shipped_delegate_understands() {
        let mut predecessor = DelegateFixture::modern();
        let prefix = prefix_of_seed(1);
        predecessor.known_sites = vec![site(&prefix, "Site")];
        predecessor
            .per_prefix_keys
            .insert(prefix.clone(), signing_key_seed(1));

        let fixture = Fixture {
            predecessors: vec![predecessor],
            successor: DelegateFixture::modern(),
        };

        let node = FakeNode::new(&fixture);
        let mut markers = MemoryMarkers::default();
        let _ = block_on(run_delegate_migration(
            &node,
            &mut markers,
            delegate_key(successor_key_bytes()),
            &fixture.lineage(),
            Vec::new(),
        ));

        for (_, label) in node.log.borrow().iter() {
            assert_ne!(
                *label, "Other",
                "the adapter sent a request outside Delta's shipped delegate protocol"
            );
        }
        // And the per-prefix slot name it writes is the delegate's own.
        assert_eq!(
            per_prefix_signing_key("abc"),
            format!("{SECRET_SIGNING_KEY_PREFIX}abc")
        );
    }
}
