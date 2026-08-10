use ciborium::de::from_reader;
use delta_core::SiteState;
use dioxus::prelude::ReadableExt;
use freenet_migrate::{
    ContractLineageEntry, FoldAllAck, NewestFirst, Outcome, ProbeDriver, ProbeStateOps,
    SelectionPolicy, Step, DEFAULT_MAX_PROBE_HOPS,
};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use std::sync::Arc;

use crate::state::{self, KnownSite, SiteRole};
use dioxus::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Site contract WASM (embedded at build time).
const SITE_CONTRACT_WASM: &[u8] = include_bytes!("../../public/contracts/site_contract.wasm");

// BLAKE3 hashes of every previous `site_contract.wasm` shipped by Delta,
// generated from `legacy_contracts.toml` by `ui/build.rs`.
//
// Every release commit that changes `site_contract.wasm` (including
// incidental rebuilds caused by touching `common/`) must first record
// the committed WASM hash via `./scripts/add-contract-migration.sh`
// and commit the updated `legacy_contracts.toml`. Without this, users
// of the previous release whose delegate-stored `contract_key_b58`
// is missing or stale are unable to reach their existing site state
// and see a permanent "Loading..." screen.
include!(concat!(env!("OUT_DIR"), "/legacy_contracts.rs"));

/// Pending migrations: maps old contract key (base58) -> site prefix.
/// When a GET response arrives for an old key, we PUT the state to the new key.
///
/// Multiple old keys may be registered for the same prefix when the UI is
/// probing several historical contract WASM hashes at startup. Late arrivals
/// are NOT dropped: every generation's state flows through the tombstone-aware
/// merge in `handle_site_state` / `reconcile_into`, which keeps the newest data
/// and can never be clobbered by an older generation regardless of arrival
/// order. Each entry is removed when its own key's GET resolves (state or
/// NotFound).
static PENDING_MIGRATIONS: GlobalSignal<BTreeMap<String, String>> =
    GlobalSignal::new(BTreeMap::new);

/// Prefixes whose initial state capture is in progress.
///
/// Populated by `restore_known_sites` for each site it is restoring. Its only
/// remaining job is to classify a current-key GET as `InitialCurrentKey` (so an
/// empty current-key response defers to the legacy probes) vs a later
/// `LiveUpdate`. It does NOT gate which state wins: every candidate generation
/// is reconciled via the tombstone-aware merge in `handle_site_state`
/// (`reconcile_into`), which keeps the newest data and preserves deletions
/// regardless of arrival order — there is no "first wins / drop late arrival"
/// behavior.
static MIGRATING_PREFIXES: GlobalSignal<BTreeSet<String>> = GlobalSignal::new(BTreeSet::new);

/// Register a prefix as "currently being captured from the network".
/// Called by `restore_known_sites` before firing any GETs for it.
pub fn mark_prefix_migrating(prefix: &str) {
    MIGRATING_PREFIXES.with_mut(|set| {
        set.insert(prefix.to_string());
    });
}

/// Classification of an incoming GET response as determined by the
/// (prefix, key, pending-migrations, migrating-prefixes) tuple. Exposed
/// for unit testing the state machine in isolation from Dioxus signals
/// and the WebSocket runtime.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GetClassification {
    /// GET for a legacy/migration key that is currently pending. The
    /// caller should process the state (if non-empty), PUT-migrate it
    /// to the current key, cancel sibling probes for the prefix, and
    /// remove this prefix from `MIGRATING_PREFIXES`.
    PendingMigration { prefix: String },
    /// GET for the current contract key belonging to a prefix that is
    /// still being captured. The caller should process the state (if
    /// non-empty), cancel sibling legacy probes for the prefix, and
    /// remove the prefix from `MIGRATING_PREFIXES`. No migration PUT
    /// is needed because the state is already under the current key.
    InitialCurrentKey { prefix: String },
    /// GET for a prefix that has already completed its initial capture
    /// (or was never in the migration window). The response should be
    /// treated as a live update — processed only if non-empty, no
    /// migration bookkeeping.
    LiveUpdate,
    /// GET for a key we do not recognize at all (pending-migrations
    /// lookup missed and the prefix cannot be resolved from the key).
    /// Fall through to the delegate-backup path on NotFound; otherwise
    /// treat as a live update.
    Unknown,
}

/// Pure classifier used by `handle_contract_response::GetResponse`.
/// Takes the key and three reads of global state so it can be tested
/// with mocked inputs.
pub(crate) fn classify_get_response(
    key_b58: &str,
    pending_migrations: &BTreeMap<String, String>,
    migrating_prefixes: &BTreeSet<String>,
    prefix_for_current_key: Option<&str>,
) -> GetClassification {
    if let Some(prefix) = pending_migrations.get(key_b58) {
        return GetClassification::PendingMigration {
            prefix: prefix.clone(),
        };
    }
    match prefix_for_current_key {
        Some(prefix) if migrating_prefixes.contains(prefix) => {
            GetClassification::InitialCurrentKey {
                prefix: prefix.to_string(),
            }
        }
        Some(_) => GetClassification::LiveUpdate,
        None => GetClassification::Unknown,
    }
}

/// Handle an incoming response from the Freenet node.
pub fn handle_response(response: HostResponse) {
    // Receiving a response means the WebSocket is still alive.
    // Self-clear a stuck `ConnectionStatus::Error` so the bottom-left
    // indicator doesn't stay red forever after a transient transport
    // blip — the WebApi error callback that originally set it has no
    // counterpart that resets the status. (Ivvor, 2026-05-03)
    let mut status = super::CONNECTION_STATUS.write();
    if !matches!(&*status, super::ConnectionStatus::Connected) {
        *status = super::ConnectionStatus::Connected;
    }
    drop(status);

    match response {
        HostResponse::ContractResponse(contract_response) => {
            handle_contract_response(contract_response);
        }
        HostResponse::DelegateResponse { key, values } => {
            super::delegate::handle_delegate_response(key, values);
        }
        HostResponse::Ok => {}
        other => {
            log(&format!("Delta: unhandled response: {other:?}"));
        }
    }
}

fn handle_contract_response(response: ContractResponse) {
    match response {
        ContractResponse::GetResponse { key, state, .. } => {
            let key_b58 = key.encoded_contract_id();
            log(&format!("Delta: GET response for {key}"));

            let prefix_for_key = find_prefix_for_contract_key(&key);
            let classification = classify_get_response(
                &key_b58,
                &PENDING_MIGRATIONS.read(),
                &MIGRATING_PREFIXES.read(),
                prefix_for_key.as_deref(),
            );

            let state_bytes = state.to_vec();

            // Current-key resolution — DECOUPLED from classification (a nonempty
            // LEGACY response arriving first removes the prefix from
            // MIGRATING_PREFIXES, so the current-key response then classifies as
            // LiveUpdate). A GET whose key resolves to a known site's current key
            // (legacy keys never do) drives two independent things:
            //
            //   * The unload-window backup request, fired ONCE per prefix per
            //     session, UNCONDITIONALLY on any current-key resolution —
            //     nonempty, empty, or undecodable — and independent of the sweep
            //     and its lifetime. A one-candidate sweep can finalize (and be
            //     removed) before the current-key response, and a site may have
            //     no legacy candidates at all; a sweep-gated request would be
            //     skipped in exactly those orderings. Firing after the current
            //     key resolves preserves option-B's no-write-amplification: by
            //     the time the (local) backup response is reconciled, SITES holds
            //     the current-key state, so the backup only PUTs when it exceeds
            //     it.
            //   * The sweep's current-key BASELINE recording (for the finalize
            //     gate), which naturally needs a decodable nonempty state and a
            //     live sweep, so it stays gated on those.
            if let Some(prefix) = prefix_for_key.as_deref() {
                request_backup_once(prefix);
                if !state_bytes.is_empty() {
                    if let Ok(current_state) = from_reader::<SiteState, _>(state_bytes.as_slice()) {
                        record_sweep_current_key_state(prefix, current_state);
                    }
                }
            }

            match classification {
                GetClassification::PendingMigration { prefix } => {
                    // Clear this one entry up front so the classifier
                    // doesn't re-fire for the same key on a duplicate
                    // delivery. Sibling probes for the same prefix are
                    // left in PENDING_MIGRATIONS on purpose (see below).
                    PENDING_MIGRATIONS.write().remove(&key_b58);

                    if state_bytes.is_empty() {
                        log(&format!(
                            "Delta: migration GET returned empty for site {prefix}"
                        ));
                        // An empty legacy generation is a resolved candidate
                        // (a miss for the fold). Record it so the sweep can
                        // complete; preserve the pre-driver early return, which
                        // deliberately does NOT clear MIGRATING_PREFIXES for an
                        // empty response.
                        record_sweep_resolution(&prefix, &key_b58, None, false);
                        return;
                    }
                    // Reconcile this legacy generation into local state via a
                    // tombstone-aware merge (see `reconcile_into`): only genuinely
                    // new data is adopted, deletions on either side are preserved,
                    // and an older/dominated generation is a no-op. We do NOT drop
                    // late responses and do NOT cancel sibling probes, so a
                    // still-newer generation can also contribute.
                    //
                    // The forward PUT of the reconciled state is deferred to
                    // sweep completion (`finalize_migration_sweep`) — the driver
                    // folds all generations to one outcome and PUTs once, instead
                    // of re-PUTting per response. Record this resolved candidate
                    // and whether it changed local state (the exact condition the
                    // pre-driver code PUT on).
                    let new_key = state::contract_key_from_prefix(&prefix);
                    let changed = handle_site_state(new_key, &state_bytes);
                    if changed {
                        log(&format!(
                            "Delta: merged newer data for site {prefix} from a legacy generation"
                        ));
                    } else {
                        log(&format!(
                            "Delta: legacy generation for site {prefix} added no new \
                             data (dominated by current); ignored"
                        ));
                    }
                    record_sweep_resolution(&prefix, &key_b58, Some(state_bytes), changed);
                    // Initial capture has progressed for this prefix; a
                    // subsequent current-key GET now flows through the
                    // LiveUpdate path (still merge-guarded).
                    MIGRATING_PREFIXES.with_mut(|set| {
                        set.remove(&prefix);
                    });
                }
                GetClassification::InitialCurrentKey { prefix } => {
                    // Note: the current-key state recording + unload-window backup
                    // request happen in the classification-independent block above
                    // (a legacy response arriving first would reclassify this as
                    // LiveUpdate, so it cannot live here).
                    if state_bytes.is_empty() {
                        // Empty state for the current key during the
                        // initial capture window doesn't tell us
                        // anything useful; let legacy probes resolve.
                        log(&format!(
                            "Delta: current-key GET returned empty for site {prefix} \
                             during initial capture; awaiting legacy probes"
                        ));
                        return;
                    }
                    if handle_site_state(key, &state_bytes) {
                        log(&format!(
                            "Delta: captured state for site {prefix} from current contract key"
                        ));
                    }
                    // Do NOT cancel the legacy-generation probes: one of
                    // them may hold a NEWER generation that must still be
                    // able to win via the recency guard (self-heal for
                    // users whose current key holds a stale generation from
                    // a prior broken migration).
                    MIGRATING_PREFIXES.with_mut(|set| {
                        set.remove(&prefix);
                    });
                    subscribe_to_site_by_id(&key.id().clone());
                }
                GetClassification::LiveUpdate | GetClassification::Unknown => {
                    if !state_bytes.is_empty() {
                        handle_site_state(key, &state_bytes);
                    }
                    subscribe_to_site(&key);
                }
            }
        }
        ContractResponse::UpdateNotification { key, update } => {
            log(&format!("Delta: update notification for {key}"));
            match update {
                UpdateData::State(s) => {
                    handle_site_state(key, s.as_ref());
                }
                UpdateData::Delta(d) => {
                    handle_site_delta(key, d.as_ref());
                }
                _ => {}
            }
        }
        ContractResponse::PutResponse { key } => {
            log(&format!("Delta: PUT succeeded for {key}"));
            // Subscribe to our own site after successful PUT
            log(&format!("Delta: subscribing to {key}"));
            subscribe_to_site(&key);
        }
        ContractResponse::UpdateResponse { key, .. } => {
            log(&format!("Delta: UPDATE succeeded for {key}"));
        }
        ContractResponse::NotFound { instance_id } => {
            let key_b58 = instance_id.encode();
            log(&format!("Delta: contract not found: {key_b58}"));
            // Clean up any pending migration for this key. The caller
            // in `restore_known_sites` always issues a GET for the
            // current contract key alongside the legacy-hash probes,
            // so a NotFound on one legacy hash does not need to retry
            // the current key here — that GET is already in flight.
            let pending_prefix = PENDING_MIGRATIONS.write().remove(&key_b58);
            if let Some(prefix) = pending_prefix {
                log(&format!(
                    "Delta: legacy contract key {key_b58} has no state; \
                     another probe may still succeed"
                ));
                // Resolve this candidate as a miss so the sweep can complete.
                record_sweep_resolution(&prefix, &key_b58, None, false);
            } else if let Some(prefix) = find_prefix_for_contract_key_b58(&key_b58) {
                // Network doesn't have this contract under its current
                // key either — try restoring from a delegate backup.
                // Only runs for sites whose current key is unknown to
                // the network; legacy-hash probes never reach here.
                log(&format!(
                    "Delta: contract not found on network, trying delegate backup for {prefix}"
                ));
                super::delegate::request_site_state_backup(&prefix);
            }
        }
        other => {
            log(&format!("Delta: unhandled contract response: {other:?}"));
        }
    }
}

/// Reconcile an incoming full state into `existing` WITHOUT losing data.
///
/// This replaces the earlier scalar-"recency" wholesale replace, which was
/// itself a data-loss bug of the SAME class as the incident: deleting the
/// newest page REMOVES it from `pages`, which LOWERS `max(updated_at)`, so a
/// pre-deletion snapshot out-ranked the true post-deletion state and the
/// self-heal probe resurrected the deleted page. Instead we use delta-core's
/// commutative, tombstone-aware [`SiteState::merge`] — the SAME merge the
/// contract applies — which honors `deleted_pages` on BOTH sides and keeps
/// the newer of each page by `updated_at`. A delete can therefore never be
/// lost, regardless of which generation arrives or in what order.
///
/// First capture (existing is the empty placeholder) adopts `incoming`
/// wholesale: there is nothing to merge into, and the old code accepted the
/// first state without a signature check, so we preserve that. For a
/// non-empty existing state we merge, which verifies `incoming` against the
/// site params and rejects a state that fails verification (keeping what we
/// have). Returns whether `existing` changed.
pub(crate) fn reconcile_into(existing: &mut SiteState, incoming: &SiteState) -> bool {
    if *incoming == SiteState::default() {
        return false; // nothing real to adopt
    }
    if *existing == SiteState::default() {
        // First capture. TODO(follow-up): this adopts `incoming` without a
        // signature check (pre-existing behavior — the old `handle_site_state`
        // never verified either). Consider verifying `incoming.verify(&params)`
        // here too so a corrupt/forged first state can't seed a placeholder.
        // Left as a separate change per review (not part of these fixes).
        *existing = incoming.clone();
        return true;
    }
    // Never blend two different owners (a prefix collision, or an incoming
    // state resolved to this entry via a contract key): merging would corrupt
    // both sites. The merge below also rejects an owner/params mismatch, but
    // guard explicitly so the intent is clear.
    if existing.owner != incoming.owner {
        return false;
    }
    let params = delta_core::SiteParameters::from_owner(&incoming.owner);
    let before = existing.clone();
    if existing.merge(&params, incoming).is_err() {
        // incoming failed verification — keep what we have.
        return false;
    }
    // F3: preserve the highest id counter across the merge. `SiteState::merge`
    // advances `next_page_id` only when it INSERTS a live page, so an incoming
    // generation whose highest ids were created-then-deleted — possibly
    // leaving NO tombstone (a pre-tombstone-era deletion) — would otherwise
    // lose its counter, letting `create_page` reuse a since-deleted id.
    existing.next_page_id = existing.next_page_id.max(incoming.next_page_id);
    *existing != before
}

/// Process a full site state received from GET or full state update.
///
/// Returns `true` iff local state actually changed (new pages, a newer
/// config, or a newly-applied deletion). Callers use this to decide whether
/// to PUT the reconciled state forward to the current contract key, so an
/// unchanged (dominated) generation triggers no write.
fn handle_site_state(key: ContractKey, state_bytes: &[u8]) -> bool {
    if state_bytes.is_empty() {
        return false;
    }

    let site_state: SiteState = match from_reader(state_bytes) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("Delta: failed to deserialize site state: {e}"));
            return false;
        }
    };

    let name = site_state.config.config.name.clone();
    let owner_pubkey = site_state.owner.to_bytes();
    // Derive prefix from owner pubkey
    let prefix_from_pubkey = delta_core::pubkey_to_prefix(&site_state.owner);

    // Try to find existing entry: first by pubkey-derived prefix, then by contract key
    let prefix = if state::SITES.read().contains_key(&prefix_from_pubkey) {
        prefix_from_pubkey
    } else if let Some(p) = find_prefix_for_contract_key(&key) {
        p
    } else {
        prefix_from_pubkey
    };

    // Don't re-add sites the user explicitly removed.
    if state::REMOVED_PREFIXES.read().contains(&prefix) {
        log(&format!(
            "Delta: blocked re-add of removed site {prefix} from network response"
        ));
        return false;
    }

    let changed = {
        let mut sites = state::SITES.write();
        if let Some(existing) = sites.get_mut(&prefix) {
            // Tombstone-aware merge: an older generation / stale backup can
            // never overwrite newer data, and a deletion is never lost.
            let c = reconcile_into(&mut existing.state, &site_state);
            if c {
                existing.name = existing.state.config.config.name.clone();
                existing.owner_pubkey = existing.state.owner.to_bytes();
                if existing.contract_key.is_none() {
                    existing.contract_key = Some(key);
                }
            }
            c
        } else {
            sites.insert(
                prefix.clone(),
                KnownSite {
                    name,
                    prefix: prefix.clone(),
                    role: SiteRole::Visitor,
                    state: site_state,
                    owner_pubkey,
                    contract_key: Some(key),
                },
            );
            true
        }
    };

    if !changed {
        log(&format!(
            "Delta: state for {prefix} had no new data after merge; keeping current"
        ));
        return false;
    }

    // Back up state to delegate for resilience
    if let Some(site) = state::SITES.read().get(&prefix) {
        super::delegate::backup_site_state(&prefix, &site.state);
    }

    // If this is the currently selected site, re-select to pick up
    // pending page from hash route and update title
    if state::CURRENT_SITE.read().as_deref() == Some(&prefix) {
        #[cfg(target_arch = "wasm32")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                state::select_site(&prefix);
            });
        }
    }
    true
}

/// Apply a delta to a site's state via the single verified implementation
/// (`SiteState::apply_delta`, also used by the contract itself), rather than
/// hand-rolling a second applier here. `handle_site_delta` used to have its
/// own inline logic that never verified page/deletion/config signatures and
/// never recorded a tombstone for an applied deletion, so a deleted page
/// just came back on the next GET, delegate-backup restore, or
/// legacy-contract sweep (`merge`/the tombstone-aware reconcile had nothing
/// to skip on, since no tombstone was ever recorded). That was effectively
/// dead code as long as `compute_delta` never populated `page_deletions`;
/// this PR makes that path live, which made both gaps live too. That is a
/// genuine authentication bypass, not just a missed optimization, so it is
/// fixed by delegating rather than patching the second implementation to
/// match the first.
///
/// A free function (not touching `state::SITES` or any other global) so it
/// is directly unit-testable, matching this file's existing pattern for
/// logic that used to be inlined into a signal-touching caller (see
/// `reconcile_into`, `classify_get_response`).
///
/// Returns the resulting site name on success (for the caller to sync onto
/// `KnownSite::name`), or the verification error on rejection.
///
/// Applies to a CLONE and only swaps it in on success. `SiteState::apply_delta`
/// mutates its fields incrementally (config, then page_updates, then
/// page_deletions) and returns early via `?` on the first failure, so an
/// in-place `site_state.apply_delta(...)` would commit whichever fields
/// verified before the one that didn't — only reachable from a delta whose
/// earlier fields are validly signed by the real owner but a later field is
/// forged by a different key, but a "rejected" delta should never leave a
/// partial mutation behind. The clone keeps this call site atomic; deliberately
/// NOT changed in `SiteState::apply_delta` itself, since the contract's
/// `update_state` already discards its `site_state` on `Err` (see
/// `ContractInterface::update_state`) and the clone belongs where the
/// in-place, reused `SiteState` actually lives.
fn apply_delta_to_site_state(
    site_state: &mut SiteState,
    delta: &delta_core::SiteStateDelta,
) -> Result<String, String> {
    let mut next = site_state.clone();
    let params = delta_core::SiteParameters::from_owner(&next.owner);
    next.apply_delta(delta, &params)?;
    let name = next.config.config.name.clone();
    *site_state = next;
    Ok(name)
}

/// Process a delta update for a site.
fn handle_site_delta(key: ContractKey, delta_bytes: &[u8]) {
    if delta_bytes.is_empty() {
        return;
    }

    let delta: delta_core::SiteStateDelta = match from_reader(delta_bytes) {
        Ok(d) => d,
        Err(e) => {
            log(&format!("Delta: failed to deserialize delta: {e}"));
            return;
        }
    };

    let prefix = find_prefix_for_contract_key(&key);
    let Some(prefix) = prefix else {
        log(&format!("Delta: delta for unknown contract key {key}"));
        return;
    };
    let mut sites = state::SITES.write();

    if let Some(site) = sites.get_mut(&prefix) {
        match apply_delta_to_site_state(&mut site.state, &delta) {
            Ok(name) => site.name = name,
            Err(e) => {
                log(&format!(
                    "Delta: rejected delta for {prefix} (failed verification): {e}"
                ));
                return;
            }
        }
    }

    // Back up updated state to delegate
    if let Some(site) = sites.get(&prefix) {
        super::delegate::backup_site_state(&prefix, &site.state);
    }
}

/// Subscribe to a site contract to receive live updates.
#[allow(dead_code)]
pub fn subscribe_to_site(contract_key: &ContractKey) {
    subscribe_to_site_by_id(contract_key.id());
}

/// Subscribe by ContractInstanceId directly.
#[allow(dead_code)]
pub fn subscribe_to_site_by_id(id: &ContractInstanceId) {
    let key = *id;
    send(move |api| {
        Box::pin(async move {
            let request =
                ClientRequest::ContractOp(ContractRequest::Subscribe { key, summary: None });
            api.send(request).await
        })
    });
}

/// GET a site contract's current state.
#[allow(dead_code)]
pub fn get_site(contract_key: &ContractKey) {
    get_site_by_id(contract_key.id());
}

/// GET by ContractInstanceId directly.
#[allow(dead_code)]
pub fn get_site_by_id(id: &ContractInstanceId) {
    let key = *id;
    send(move |api| {
        Box::pin(async move {
            let request = ClientRequest::ContractOp(ContractRequest::Get {
                key,
                return_contract_code: true,
                subscribe: false,
                blocking_subscribe: false,
            });
            api.send(request).await
        })
    });
}

/// Compute the contract instance ID for a given `prefix` under a
/// specific contract WASM hash. `ContractInstanceId =
/// BLAKE3(BLAKE3(wasm) || CBOR(params))`, where BLAKE3(wasm) is already
/// `wasm_code_hash`.
fn contract_id_for_prefix_with_hash(prefix: &str, wasm_code_hash: &[u8; 32]) -> String {
    let params_buf = cbor_site_params(prefix);

    let mut hasher = blake3::Hasher::new();
    hasher.update(wasm_code_hash);
    hasher.update(&params_buf);
    bs58::encode(hasher.finalize().as_bytes()).into_string()
}

/// Legacy contract instance IDs for a given site prefix — one per
/// historical `site_contract.wasm` hash recorded in
/// `legacy_contracts.toml`. The caller fires a migration GET for each
/// so that sites whose on-network state lives under any prior contract
/// key can be rescued, not just the immediately-preceding release.
///
/// The current contract key is excluded: callers should issue a normal
/// `get_site` for the current key separately.
pub fn legacy_contract_ids_for_prefix(prefix: &str, current_id_b58: &str) -> Vec<String> {
    LEGACY_CONTRACT_HASHES
        .iter()
        .map(|hash| contract_id_for_prefix_with_hash(prefix, hash))
        .filter(|id| id != current_id_b58)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// GET from an old contract key for migration purposes.
/// Registers the old key -> prefix mapping so the response handler
/// knows to PUT the state to the new key.
pub fn get_for_migration(old_key_b58: &str, prefix: &str) {
    let old_id: ContractInstanceId = match old_key_b58.parse() {
        Ok(id) => id,
        Err(_) => {
            log(&format!(
                "Delta: can't parse old contract key for migration: {old_key_b58}"
            ));
            return;
        }
    };

    PENDING_MIGRATIONS
        .write()
        .insert(old_key_b58.to_string(), prefix.to_string());
    // Track this candidate in the prefix's sweep so the driver's single forward
    // PUT fires once every candidate has resolved. Registered synchronously
    // here, before any response can arrive. The returned generation tags the
    // timeout so a stale fire cannot disturb a later same-prefix sweep.
    let sweep_gen = register_sweep_candidate(prefix, old_key_b58);

    // Arm a per-candidate timeout. NEW behavior vs Delta's previously-untimed
    // sweep: without it, a candidate whose GET neither responds nor NotFounds
    // would leave the sweep incomplete forever, so the driver's single forward
    // PUT would never fire this session (recovered state would still reach local
    // SITES via the per-response reconcile below, but would not propagate to the
    // network's current key until the next reload). The timeout delivers a miss
    // to the sweep — the driver's own model for a lost candidate — bounding a
    // previously-unbounded wait so the forward PUT always lands. A real response
    // arriving after the timeout is single-shot-dropped from the sweep (the
    // idempotent `resolve`) but is still reconciled into SITES. The generation
    // tag makes a stale timeout a no-op against a superseded sweep.
    #[cfg(target_arch = "wasm32")]
    {
        let prefix = prefix.to_string();
        let key_b58 = old_key_b58.to_string();
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::sleep(std::time::Duration::from_millis(
                SWEEP_CANDIDATE_TIMEOUT_MS.into(),
            ))
            .await;
            record_sweep_timeout(&prefix, &key_b58, sweep_gen);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = sweep_gen;

    log(&format!(
        "Delta: GET from old contract for migration: {old_key_b58}"
    ));

    let key = old_id;
    send(move |api| {
        Box::pin(async move {
            let request = ClientRequest::ContractOp(ContractRequest::Get {
                key,
                return_contract_code: true,
                subscribe: false,
                blocking_subscribe: false,
            });
            api.send(request).await
        })
    });
}

/// The instance ID of the NEWEST legacy contract generation (the WASM hash
/// immediately preceding the current one) for `prefix`, or `None` when the
/// only legacy hash is the current key itself. `legacy_contracts.toml` is
/// ordered oldest→newest, so the newest legacy hash is the last entry.
fn newest_legacy_contract_id_for_prefix(prefix: &str, current_id_b58: &str) -> Option<String> {
    LEGACY_CONTRACT_HASHES
        .iter()
        .rev()
        .map(|hash| contract_id_for_prefix_with_hash(prefix, hash))
        .find(|id| id != current_id_b58)
}

/// Self-heal probe: re-GET the NEWEST legacy contract generation for a site
/// even when the delegate-stored `contract_key_b58` matches the current WASM
/// (so the generic legacy sweep is skipped).
///
/// A prior BROKEN migration can leave a user's real current content stranded
/// under the immediately-preceding contract generation while a stale/empty
/// state sits under the current key AND the delegate-stored key already
/// points at the current WASM. Such users would never re-probe the old
/// generation and would be permanently "rolled back". Always re-probing the
/// newest legacy generation, combined with the recency guard in
/// `handle_site_state`, lets them self-heal on reload: if the old generation
/// is newer it wins and is migrated forward; if it is not, it is ignored.
pub fn fire_newest_legacy_contract_migration(prefix: &str, current_key_b58: &str) {
    if let Some(newest) = newest_legacy_contract_id_for_prefix(prefix, current_key_b58) {
        log(&format!(
            "Delta: self-heal re-probing newest legacy contract generation for site {prefix}"
        ));
        get_for_migration(&newest, prefix);
    }
}

/// Fire migration GETs for every historical contract WASM hash recorded
/// in `legacy_contracts.toml`. Called for sites with a missing or stale
/// `contract_key_b58` so that state stored under any past contract key
/// can be rescued, not just the immediately-preceding release.
pub fn fire_legacy_contract_migrations(prefix: &str, current_key_b58: &str) {
    let legacy_ids = legacy_contract_ids_for_prefix(prefix, current_key_b58);
    if legacy_ids.is_empty() {
        return;
    }
    log(&format!(
        "Delta: probing {} legacy contract hash(es) for site {prefix}",
        legacy_ids.len()
    ));
    for old_b58 in legacy_ids {
        get_for_migration(&old_b58, prefix);
    }
}

// ---------------------------------------------------------------------------
// freenet-migrate decision driver
// ---------------------------------------------------------------------------
//
// Delta's initial-capture sweep is adopted onto freenet-migrate's sans-IO
// `ProbeDriver` (the same decision machinery River and riverctl ship). The
// binding below is deliberately thin: every state decision delegates to the
// incident-hardened `reconcile_into` (delta#33/#34), so the driver's fold IS
// Delta's tombstone-aware, commutative merge — there is no second merge
// implementation that could silently drift from the one the existing tests
// pin.
//
// Policy is `FoldAll`: Delta already folds *every* legacy generation into
// local state (its concurrent sweep is a FoldAll realized concurrently). The
// `policy_check` property tests mechanically confirm the merge is commutative,
// idempotent, and fold-order-invariant over the tombstone-era generations, and
// the `FoldAllAck` is the loud acknowledgement of the preconditions. Two honest
// bounds on that soundness — BOTH pre-existing, shared identically by today's
// concurrent per-response sweep, so adopting the driver introduces neither:
//
//   * The tombstone precondition holds only for the TOMBSTONE-ERA lineage. The
//     oldest registry generation (C1, "Pre-tombstone WASM") predates tombstones,
//     so a page deleted before tombstones existed can resurrect through the fold
//     from C1. This is the pre-existing accepted residual from the delta#34-era
//     analysis (the fix that added tombstones); floor-vs-no-floor is tracked in
//     delta#38.
//   * The merge is order-INDEPENDENT only where writes do not conflict at an
//     equal (page `updated_at` / config `version`), and where a page is not
//     deleted twice at different `deleted_at`. Same-timestamp conflicting writes
//     (e.g. concurrent tabs) resolve keep-accumulator, and conflicting tombstones
//     (`SiteState::merge`'s `deleted_pages` or_insert) likewise keep the
//     accumulator's deletion — both order-dependent. Harmless: for the tombstone
//     conflict the page stays deleted either way, only the recorded deletion
//     timestamp differs; and the PUT payload is the canonical concurrently-merged
//     SITES value, so the fold matches production arrival-order behavior exactly.

/// `ProbeStateOps` binding Delta's `SiteState` into the decision driver.
struct DeltaProbeOps;

impl ProbeStateOps for DeltaProbeOps {
    type State = SiteState;

    /// Same defensive deserialization as `handle_site_state`: an undecodable
    /// (corrupt / ancient-format) or empty generation is a **miss**, never a
    /// panic and never adopted.
    fn decode(&self, bytes: &[u8]) -> Option<SiteState> {
        if bytes.is_empty() {
            return None;
        }
        from_reader(bytes).ok()
    }

    /// Mirrors `reconcile_into`'s `incoming == SiteState::default()` miss: the
    /// zeroed placeholder is not real state.
    ///
    /// Deliberately does **not** verify the signature. `reconcile_into` adopts
    /// a FIRST capture (empty existing) without a signature check — the pre-#34
    /// behavior it explicitly preserves (see its TODO). Adding a verify here
    /// would diverge from that; if first-capture verification is ever added it
    /// must be added in `reconcile_into` and here together.
    fn is_real(&self, state: &SiteState) -> bool {
        *state != SiteState::default()
    }

    /// Fold an older generation into the newer accumulator. `reconcile_into`
    /// is commutative + tombstone-aware, so this is exactly the fold Delta's
    /// concurrent SITES merge performs: owner-equality guard (mismatch keeps
    /// the accumulator, logging a warn), then `SiteState::merge` (fail-closed:
    /// keep the accumulator on a verification failure), then the F3
    /// `next_page_id`-max.
    fn merge_generations(&self, mut newer: SiteState, older: SiteState) -> SiteState {
        reconcile_into(&mut newer, &older);
        newer
    }

    /// Fold the recovered generation into the device's local snapshot, keeping
    /// local-only writes. Same reconcile: a first capture (empty local) adopts
    /// `recovered` wholesale, an empty `recovered` is a no-op, and a
    /// different-owner `recovered` keeps the local snapshot unchanged.
    ///
    /// Note the fail-closed direction differs from the driver's advisory hint
    /// ("prefer returning recovered"): Delta keeps the LOCAL snapshot if
    /// `recovered` fails verification, because adopting an unverified state
    /// over a verified local one is exactly the class of bug #34 hardened
    /// against. `reconcile_into` is commutative, so the adopted page/tombstone
    /// set is identical regardless of which side is the base.
    fn merge_with_local(&self, recovered: SiteState, local: &SiteState) -> SiteState {
        let mut base = local.clone();
        reconcile_into(&mut base, &recovered);
        base
    }

    // prepare_forward: identity (the default). Delta carries no key-relative
    // upgrade pointer inside its state (unlike freenet/river#427), so there is
    // nothing to strip before the forward PUT.
}

/// Build the newest-first candidate ordering for a site `prefix` from the
/// legacy-contract registry. `legacy_contracts.toml` is oldest→newest, so
/// generation = index; `NewestFirst::from_lineage` sorts descending by that
/// field (robust even if the registry were ever authored out of order).
///
/// The `Parameters` are the CBOR-encoded `SiteParameters { prefix }` — the
/// same bytes Delta hashes into the contract key, so
/// `contract_id_from_code_hash` reproduces exactly the ids
/// `contract_id_for_prefix_with_hash` derives (pinned by
/// `driver_candidate_ids_match_delta_derivation`).
fn legacy_lineage_newest_first(prefix: &str) -> NewestFirst {
    let params_bytes = cbor_site_params(prefix);
    let params = Parameters::from(params_bytes);
    let lineage: Vec<ContractLineageEntry> = LEGACY_CONTRACT_HASHES
        .iter()
        .enumerate()
        .map(|(generation, code_hash)| ContractLineageEntry {
            generation: generation as u32,
            code_hash: *code_hash,
            note: "delta legacy contract",
        })
        .collect();
    NewestFirst::from_lineage(&params, &lineage)
}

/// CBOR encoding of `SiteParameters { prefix }` — the contract's parameter
/// bytes. Shared by the driver candidate derivation so it stays byte-identical
/// to `contract_id_for_prefix_with_hash`.
fn cbor_site_params(prefix: &str) -> Vec<u8> {
    let params = delta_core::SiteParameters {
        prefix: prefix.to_string(),
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&params, &mut buf).expect("CBOR params");
    buf
}

/// Drive a `FoldAll` probe over `candidates` (newest-first) to completion using
/// already-fetched `responses` (`Some(bytes)` = GET state, `None` = a
/// NotFound / miss for that candidate), folding onto `local`.
///
/// Pure and deterministic: the driver always asks newest-first and drains the
/// map, so the returned [`Outcome`] does not depend on the order responses
/// were inserted — the fold-order-invariance the `policy_check` tests prove for
/// the merge is what makes that safe. A candidate with no map entry is treated
/// as a miss (the finalize caller only runs this once every fired candidate has
/// resolved, so that path is defensive).
fn drive_fold_probe(
    candidates: NewestFirst,
    responses: &HashMap<ContractInstanceId, Option<Vec<u8>>>,
    local: SiteState,
    max_hops: usize,
) -> Outcome<SiteState> {
    let mut driver = ProbeDriver::new(
        DeltaProbeOps,
        local,
        candidates,
        SelectionPolicy::FoldAll(FoldAllAck::i_understand_fold_all_resurrects_without_tombstones()),
    )
    .with_max_hops(max_hops);
    while let Step::Get(id) = driver.next_action() {
        match responses.get(&id) {
            Some(Some(bytes)) => driver.on_response(id, bytes),
            // NotFound, or a candidate that never resolved: a miss.
            Some(None) | None => driver.on_timeout(id),
        }
    }
    driver
        .take_outcome()
        .expect("Step::Done implies an untaken outcome")
}

// ---------------------------------------------------------------------------
// Per-prefix migration sweep: batched single forward PUT
// ---------------------------------------------------------------------------
//
// Honest framing: `reconcile_into` still owns the production data decisions —
// every legacy / stale-key / self-heal candidate GET is fired concurrently (the
// concurrent fan-out is Delta's strength, unchanged) and each response is STILL
// reconciled into SITES + backed up to the delegate immediately (per-response
// visibility preserved). The freenet-migrate driver's role here is to VALIDATE
// and PIN the fold (via `DeltaProbeOps` + the property tests, which prove the
// merge is a sound `FoldAll`) and to BOUND the sweep into ONE decision, not to
// re-implement the merge.
//
// What changes vs the pre-driver code: the forward PUT. It used to re-PUT the
// reconciled state to the current contract key on EVERY legacy response that
// changed local state; now the fired candidates' responses are accumulated and,
// once every candidate has resolved (a GET response, a NotFound, or a
// per-candidate timeout), a single forward PUT of the reconciled state fires —
// gated on a legacy generation having actually changed SITES, so a dominated
// sweep never write-amplifies. This is the "one-final-PUT" adoption
// (freenet-migrate#6). The state PUT is the canonical reconciled SITES value
// (`forward_put_current_site`), which the per-response merges (the same
// `reconcile_into`) already produced — never the driver's `Outcome::merged`.
//
// Divergences vs the pre-driver per-response PUT (both bounded):
//   1. Benign: with no lost candidate, the same reconciled state is PUT once at
//      completion instead of once per changing generation. End state identical.
//   2. Real, and specifically handled: a candidate whose GET is slow enough to
//      lose the race to its 30s timeout is first recorded as a miss (possibly
//      COMPLETING and finalizing the sweep); its real response then arrives,
//      reconciles newer data into SITES, and — because the sweep is already
//      gone — would NOT propagate to the current key. That is caught by
//      `plan_late_response_action` -> `LateResponseAction::ForwardPutNow`, a
//      one-off forward PUT for the late change. (If the sweep is still live, the
//      late change instead re-arms the eventual finalize via
//      `MigrationSweep::resolve`'s changed_local OR.)

/// Accumulates one prefix's in-flight initial-capture migration sweep.
#[derive(Default)]
struct MigrationSweep {
    /// Monotonic tag identifying this sweep instance, so a stale per-candidate
    /// timeout closure from a SUPERSEDED same-prefix sweep (created within the
    /// 30s window after this one finalized) delivers no spurious miss. River's
    /// per-fire route-token pattern. 0 = unassigned (`Default`).
    generation: u64,
    /// Candidate keys (base58) fired via `get_for_migration` for this prefix.
    candidates: Vec<String>,
    /// Resolved responses so far, keyed by candidate base58 key:
    /// `Some(bytes)` = GET state, `None` = a NotFound / empty / miss.
    responses: HashMap<String, Option<Vec<u8>>>,
    /// The state the network's CURRENT contract key returned during this sweep
    /// (decoded, as the network holds it), or `None` if the current key never
    /// returned real state. The forward-PUT gate is "reconciled SITES differs
    /// from this", so a legacy generation that merely re-supplies state the
    /// current key already has never triggers a redundant PUT (the owned-site
    /// self-heal write-amplification Codex flagged). Falls back to
    /// `changed_local` only when this is `None`.
    current_key_state: Option<SiteState>,
    /// Whether any legacy generation changed local SITES state. Used only as
    /// the fallback gate when `current_key_state` is `None` (the current key
    /// never responded), since it cannot tell "recovered data the network
    /// lacks" from "re-supplied identical data".
    changed_local: bool,
}

impl MigrationSweep {
    /// Track a fired candidate key. Idempotent — a key fired twice is one
    /// candidate.
    fn register(&mut self, key_b58: &str) {
        if !self.candidates.iter().any(|c| c == key_b58) {
            self.candidates.push(key_b58.to_string());
        }
    }

    /// Record a resolved candidate (`Some(bytes)` GET state or `None` miss) and
    /// OR in whether it changed local state. Ignores keys that were never a
    /// candidate. Returns whether the sweep is now complete (every fired
    /// candidate resolved).
    ///
    /// The `changed_local` OR runs even for an ALREADY-resolved candidate: a
    /// real GET response arriving after that candidate's timeout-miss (while the
    /// sweep is still tracked) carries data the per-response reconcile has put
    /// into SITES, and it must still gate the eventual forward PUT. Such a late
    /// response does not re-count toward completion and does not overwrite the
    /// first recorded response. (The distinct case where the sweep has ALREADY
    /// finalized is handled by `plan_late_response_action` /
    /// `LateResponseAction::ForwardPutNow`, not here.)
    fn resolve(&mut self, key_b58: &str, response: Option<Vec<u8>>, changed_local: bool) -> bool {
        if !self.candidates.iter().any(|c| c == key_b58) {
            return false;
        }
        self.changed_local |= changed_local;
        if self.responses.contains_key(key_b58) {
            return false;
        }
        self.responses.insert(key_b58.to_string(), response);
        self.is_complete()
    }

    /// Whether every fired candidate has resolved. A sweep with no candidates
    /// is never "complete" (there is nothing to finalize).
    fn is_complete(&self) -> bool {
        !self.candidates.is_empty()
            && self
                .candidates
                .iter()
                .all(|c| self.responses.contains_key(c))
    }

    /// Record the network current-key state the FIRST time it is seen, returning
    /// whether this was that first recording. The caller uses the `true` return
    /// to request the unload-window backup exactly once — regardless of how many
    /// current-key responses arrive, and independent of whether the response was
    /// classified InitialCurrentKey or LiveUpdate (a legacy response arriving
    /// first removes MIGRATING_PREFIXES and reclassifies the current-key response
    /// as LiveUpdate).
    fn record_current_key_state(&mut self, state: SiteState) -> bool {
        if self.current_key_state.is_some() {
            return false;
        }
        self.current_key_state = Some(state);
        true
    }

    /// Overwrite the recorded current-key baseline with the state a forward PUT
    /// just wrote to the network (a restored-backup PUT, or a late-response PUT).
    /// After the PUT the network current key holds `state`, so a subsequent
    /// finalize's "SITES != current-key" gate must compare against it — otherwise
    /// it fires a second, identical PUT.
    fn set_current_key_baseline(&mut self, state: SiteState) {
        self.current_key_state = Some(state);
    }
}

/// Per-candidate migration-probe timeout. Generous (River's UI probe ships 12s;
/// this is looser still): the sweep must complete even if a candidate GET is
/// genuinely lost, and over-waiting only delays the forward PUT, whereas
/// under-waiting could drop a slow-but-live generation from the fold.
const SWEEP_CANDIDATE_TIMEOUT_MS: u32 = 30_000;

/// In-flight migration sweeps, keyed by site prefix. An entry is created when
/// the first candidate GET for a prefix is fired and removed when the sweep
/// finalizes.
static MIGRATION_SWEEPS: GlobalSignal<BTreeMap<String, MigrationSweep>> =
    GlobalSignal::new(BTreeMap::new);

/// Monotonic source of `MigrationSweep::generation` tags. Starts at 1 so a tag
/// is always distinguishable from the `Default` (0 = unassigned).
static NEXT_SWEEP_GENERATION: GlobalSignal<u64> = GlobalSignal::new(|| 1);

/// Register a fired migration-candidate key under its prefix's sweep, returning
/// the sweep's generation (for the per-candidate timeout to capture). Called
/// synchronously from `get_for_migration`, before any response can arrive, so
/// the full candidate set is known by the time the first response lands.
fn register_sweep_candidate(prefix: &str, key_b58: &str) -> u64 {
    // Reserve a generation up front (a cheap monotonic tag) so the borrow of
    // NEXT_SWEEP_GENERATION never nests inside the MIGRATION_SWEEPS borrow;
    // it is only consumed if this call creates the sweep.
    let reserved = NEXT_SWEEP_GENERATION.with_mut(|g| {
        let v = *g;
        *g += 1;
        v
    });
    MIGRATION_SWEEPS.with_mut(|sweeps| {
        let sweep = sweeps.entry(prefix.to_string()).or_default();
        if sweep.generation == 0 {
            sweep.generation = reserved;
        }
        sweep.register(key_b58);
        sweep.generation
    })
}

/// Whether a per-candidate timeout that armed at `armed_gen` should still fire,
/// given the prefix's current sweep generation (`None` if the sweep is gone). A
/// mismatch means a superseded / different sweep now owns the prefix, so the
/// stale timeout must be a no-op.
fn sweep_timeout_is_current(current_gen: Option<u64>, armed_gen: u64) -> bool {
    current_gen == Some(armed_gen)
}

/// Deliver a per-candidate timeout as a miss, but only if the prefix's sweep is
/// still the generation that armed the timer (see `sweep_timeout_is_current`).
fn record_sweep_timeout(prefix: &str, key_b58: &str, armed_gen: u64) {
    let current_gen = MIGRATION_SWEEPS.read().get(prefix).map(|s| s.generation);
    if sweep_timeout_is_current(current_gen, armed_gen) {
        record_sweep_resolution(prefix, key_b58, None, false);
    }
}

/// Record the network current-key state for a prefix's in-flight sweep (once),
/// so the finalize gate can PUT only when the reconciled SITES differs from
/// what the current key already holds. Baseline recording ONLY: the
/// unload-window backup request is sweep-independent (`request_backup_once` /
/// `BACKUP_REQUESTED_PREFIXES`), so a missing or already-finalized sweep does
/// not affect it. Returns whether this was the first recording (informational;
/// `false` when there is no live sweep to record into).
fn record_sweep_current_key_state(prefix: &str, state: SiteState) -> bool {
    MIGRATION_SWEEPS.with_mut(|sweeps| {
        sweeps
            .get_mut(prefix)
            .map(|sweep| sweep.record_current_key_state(state))
            .unwrap_or(false)
    })
}

/// After a forward PUT fires OUTSIDE finalize (a restored-backup PUT, or a
/// late-response PUT), update the prefix's sweep baseline to the PUT payload —
/// that state is now what the network current key holds, so a later finalize's
/// "SITES != current-key" gate will not fire a second, identical PUT. No-op if
/// there is no sweep for the prefix (e.g. the late-response path, which only
/// runs once the sweep has finalized and been removed, so there is nothing left
/// to double-PUT against).
pub(crate) fn note_forward_put_baseline(prefix: &str, payload: &SiteState) {
    MIGRATION_SWEEPS.with_mut(|sweeps| {
        if let Some(sweep) = sweeps.get_mut(prefix) {
            sweep.set_current_key_baseline(payload.clone());
        }
    });
}

/// Prefixes whose unload-window state backup has already been requested this
/// session. Tracked INDEPENDENTLY of `MIGRATION_SWEEPS` and its lifetime: the
/// backup request must fire on ANY current-key resolution for a known site, even
/// when a one-candidate sweep finalized (and was removed) before the current-key
/// response arrives, when the current-key response is empty/undecodable, or when
/// the site has no legacy candidates at all — orderings where a sweep-gated
/// request would be skipped. Resets each session (page reload), which is exactly
/// the reload-recovery cadence.
static BACKUP_REQUESTED_PREFIXES: GlobalSignal<BTreeSet<String>> = GlobalSignal::new(BTreeSet::new);

/// Whether the unload-window backup should be requested for `prefix` now, given
/// the prefixes already requested this session. Pure; the once-per-prefix
/// decision independent of sweep lifetime and response content.
fn should_request_backup(already_requested: &BTreeSet<String>, prefix: &str) -> bool {
    !already_requested.contains(prefix)
}

/// Request the delegate state backup for `prefix` exactly once per session, on
/// ANY current-key resolution (nonempty, empty, or undecodable alike). Fired
/// AFTER the current key resolves, so `handle_restored_site_state` reconciles
/// against a SITES that already holds the current-key state and PUTs only when
/// the backup exceeds it (no write-amplification). Independent of the sweep, so
/// it still fires in the orderings a sweep-gated request would miss.
fn request_backup_once(prefix: &str) {
    let fire = BACKUP_REQUESTED_PREFIXES.with_mut(|set| {
        if should_request_backup(set, prefix) {
            set.insert(prefix.to_string());
            true
        } else {
            false
        }
    });
    if fire {
        super::delegate::request_site_state_backup(prefix);
    }
}

/// What to do with a resolved candidate for a prefix, given whether that
/// prefix's sweep is still tracked. Pure, so the finalize -> late-response
/// orchestration is unit-testable off the Dioxus globals.
#[derive(Debug, PartialEq, Eq)]
enum LateResponseAction {
    /// The sweep is still tracked: record the resolution into it (it will
    /// finalize when complete, PUTting if a legacy generation changed SITES).
    Resolve,
    /// The sweep already finalized (or never existed) and this response changed
    /// SITES: fire a one-off forward PUT now so the recovered data reaches the
    /// current contract key. The single-PUT common case already ran at finalize;
    /// this covers a slow generation whose real response lost the race to its
    /// own 30s timeout (which had completed the sweep as a miss). Without it the
    /// recovered state would sit local-only + delegate-backed-up but never
    /// propagate to the network this session, and — once the current key holds
    /// the partial migration — reload would not NotFound, so the backup fallback
    /// would not fire either (the BLOCKING data-propagation bug two reviewers
    /// independently caught).
    ForwardPutNow,
    /// The sweep already finalized and this response changed nothing: nothing to
    /// do (no write-amplification).
    Ignore,
}

/// Decide what a resolved candidate should trigger. See [`LateResponseAction`].
fn plan_late_response_action(sweep_tracked: bool, changed_local: bool) -> LateResponseAction {
    match (sweep_tracked, changed_local) {
        (true, _) => LateResponseAction::Resolve,
        (false, true) => LateResponseAction::ForwardPutNow,
        (false, false) => LateResponseAction::Ignore,
    }
}

/// Record a resolved candidate for a prefix's sweep. `changed_local` is whether
/// this response's reconcile changed SITES. Routes via
/// [`plan_late_response_action`]: a normal resolution into a tracked sweep
/// (finalizing when complete), or — for a response arriving after the sweep has
/// already finalized — a one-off forward PUT when it changed SITES.
fn record_sweep_resolution(
    prefix: &str,
    key_b58: &str,
    response: Option<Vec<u8>>,
    changed_local: bool,
) {
    let sweep_tracked = MIGRATION_SWEEPS.read().contains_key(prefix);
    match plan_late_response_action(sweep_tracked, changed_local) {
        LateResponseAction::Resolve => {
            let ready = MIGRATION_SWEEPS.with_mut(|sweeps| {
                sweeps
                    .get_mut(prefix)
                    .is_some_and(|sweep| sweep.resolve(key_b58, response, changed_local))
            });
            if ready {
                finalize_migration_sweep(prefix);
            }
        }
        LateResponseAction::ForwardPutNow => {
            log(&format!(
                "Delta: late migration response for {prefix} arrived after the sweep \
                 finalized; propagating recovered state with a one-off forward PUT"
            ));
            forward_put_current_site(prefix);
        }
        LateResponseAction::Ignore => {}
    }
}

/// PUT the current reconciled SITES state for `prefix` forward to the current
/// contract key and persist known sites. Shared by the sweep finalize and the
/// late-response-after-finalize one-off PUT.
///
/// The payload is the canonical reconciled SITES state, NOT the driver's
/// `Outcome::merged`. Do NOT switch the payload to `outcome.merged` without
/// revisiting `DeltaProbeOps::merge_with_local` (which is fail-closed toward the
/// LOCAL snapshot on a verification failure, the opposite of the driver's
/// advisory hint) and non-registry stale keys (a fired stale stored key that is
/// not a registry hash contributes to SITES but is not a driver candidate, so
/// it would be missing from `outcome.merged`).
fn forward_put_current_site(prefix: &str) {
    if let Some(merged) = state::SITES.read().get(prefix).map(|s| s.state.clone()) {
        let params = delta_core::SiteParameters {
            prefix: prefix.to_string(),
        };
        put_site(&params, &merged);
        super::delegate::save_known_sites();
        // Keep the sweep's current-key baseline in step with what we just wrote,
        // so a later finalize doesn't re-PUT identical state. A no-op today for
        // both callers (finalize has already removed the sweep; the
        // late-response path only runs once the sweep is gone), kept so any
        // future caller that PUTs while a sweep is still live stays double-PUT
        // safe.
        note_forward_put_baseline(prefix, &merged);
    }
}

/// The forward-PUT decision for a completed sweep, computed purely (no globals)
/// so the write gate is unit-testable.
struct SweepPlan {
    /// Whether to PUT the reconciled state forward to the current contract key.
    /// Gated on the reconciled SITES differing from what the network's CURRENT
    /// key holds (`current_key_state`), so a legacy generation that merely
    /// re-supplies state the current key already has never write-amplifies (the
    /// owned-site self-heal redundant-write Codex flagged). Falls back to
    /// `changed_local` only when the current key never returned real state.
    forward_put: bool,
    /// Whether the fold was cut short by the FIXED 64-hop cap (missing the
    /// oldest generations). Delta's registry is 5, far under 64, so this never
    /// fires today — but the cap is fixed (not `len().max(64)`, which could
    /// never truncate) so the warning becomes real if the lineage ever exceeds
    /// the cap.
    truncated_fold: bool,
}

/// Fold a completed sweep through the driver and decide whether to forward-PUT.
///
/// Pure: takes the sweep and the current local snapshot, touches no globals, so
/// the Bug-B write-forward gate is unit-testable. The fold goes newest-first by
/// the registry generation order (FoldAll's fold is order-independent — proven
/// by the `policy_check` tests — so ordering is only for the anti-rollback probe
/// intent). `local` is the current reconciled SITES state, which the
/// per-response merges have already folded these generations into, so the fold
/// is the idempotent confirmation; the value we actually PUT is that canonical
/// SITES state. (A fired stale stored key that is not itself a registry hash —
/// rare — is folded into SITES per-response but not probed by the driver; the
/// forward PUT of SITES still carries it.)
fn plan_sweep_forward_put(prefix: &str, sweep: &MigrationSweep, local: SiteState) -> SweepPlan {
    let candidates = legacy_lineage_newest_first(prefix);
    let responses: HashMap<ContractInstanceId, Option<Vec<u8>>> = sweep
        .candidates
        .iter()
        .filter_map(|b58| {
            b58.parse::<ContractInstanceId>()
                .ok()
                .map(|id| (id, sweep.responses.get(b58).cloned().unwrap_or(None)))
        })
        .collect();
    // The write gate is "reconciled SITES differs from what the network current
    // key holds" — NOT the driver outcome (which would over-PUT a real-but-
    // dominated generation), and NOT bare changed_local (which cannot tell a
    // recovered-something generation from one that re-supplied identical state).
    // Computed before `local` is moved into the fold. The PUT payload is
    // canonical SITES, never `outcome.merged` (see `forward_put_current_site`).
    let forward_put = match &sweep.current_key_state {
        Some(current_key) => local != *current_key,
        None => sweep.changed_local,
    };
    // FIXED hop cap (not `len().max(64)`): a lineage larger than the cap must
    // actually truncate so the `truncated_fold` warning is real signal.
    let outcome = drive_fold_probe(candidates, &responses, local, DEFAULT_MAX_PROBE_HOPS);
    SweepPlan {
        forward_put,
        truncated_fold: matches!(
            outcome,
            Outcome::Recovered {
                truncated_fold: true,
                ..
            }
        ),
    }
}

/// Fold a completed sweep through the driver and, if a legacy generation
/// contributed new data, PUT the reconciled state forward to the current
/// contract key exactly once.
fn finalize_migration_sweep(prefix: &str) {
    let Some(sweep) = MIGRATION_SWEEPS.with_mut(|sweeps| sweeps.remove(prefix)) else {
        return;
    };
    let local = state::SITES
        .read()
        .get(prefix)
        .map(|s| s.state.clone())
        .unwrap_or_default();
    let plan = plan_sweep_forward_put(prefix, &sweep, local);

    if plan.truncated_fold {
        log(&format!(
            "Delta: WARNING migration fold for {prefix} was truncated by the hop cap"
        ));
    }

    if !plan.forward_put {
        // No legacy generation added data the current key lacked — nothing to
        // migrate forward (matches the pre-driver "dominated by current;
        // ignored" no-write path).
        return;
    }

    log(&format!(
        "Delta: migration sweep for {prefix} complete; reconciled state PUT \
         to the current contract key"
    ));
    forward_put_current_site(prefix);
}

/// PUT (create) a site contract with full state.
#[allow(dead_code)]
pub fn put_site(params: &delta_core::SiteParameters, site_state: &SiteState) {
    let mut state_buf = Vec::new();
    ciborium::ser::into_writer(site_state, &mut state_buf).expect("CBOR serialization");

    let mut params_buf = Vec::new();
    ciborium::ser::into_writer(params, &mut params_buf).expect("CBOR params serialization");

    send(move |api| {
        Box::pin(async move {
            let contract_code = ContractCode::from(SITE_CONTRACT_WASM);
            let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
                WrappedContract::new(Arc::new(contract_code), Parameters::from(params_buf)),
            ));
            let wrapped_state = WrappedState::new(state_buf);

            let request = ClientRequest::ContractOp(ContractRequest::Put {
                contract: contract_container,
                state: wrapped_state,
                related_contracts: Default::default(),
                subscribe: true,
                blocking_subscribe: false,
            });
            api.send(request).await
        })
    });
}

/// Send a delta update to a site contract.
#[allow(dead_code)]
pub fn update_site(contract_key: &ContractKey, delta: &delta_core::SiteStateDelta) {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(delta, &mut buf).expect("CBOR serialization");

    let key = *contract_key;
    send(move |api| {
        Box::pin(async move {
            let request = ClientRequest::ContractOp(ContractRequest::Update {
                key,
                data: UpdateData::Delta(StateDelta::from(buf)),
            });
            api.send(request).await
        })
    });
}

/// Send a request via the WebApi. The closure receives a mutable reference
/// to the WebApi and must construct the ClientRequest inside (to avoid
/// lifetime issues with ClientRequest's borrowed data).
fn send<F>(f: F)
where
    F: FnOnce(
            &mut freenet_stdlib::client_api::WebApi,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), freenet_stdlib::client_api::Error>>
                    + '_,
            >,
        > + 'static,
{
    #[cfg(target_arch = "wasm32")]
    {
        wasm_bindgen_futures::spawn_local(async move {
            let mut api = super::connection::WEB_API.write();
            if let Some(web_api) = api.as_mut() {
                if let Err(e) = f(web_api).await {
                    log(&format!("Delta: send failed: {e:?}"));
                }
            } else {
                log("Delta: not connected, request dropped");
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = f;
    }
}

/// Find the site prefix that corresponds to a contract key by checking existing sites.
fn find_prefix_for_contract_key(key: &ContractKey) -> Option<String> {
    find_prefix_for_contract_key_b58(&key.encoded_contract_id())
}

fn find_prefix_for_contract_key_b58(key_b58: &str) -> Option<String> {
    let sites = state::SITES.read();
    for (prefix, site) in sites.iter() {
        if let Some(ck) = &site.contract_key {
            if ck.encoded_contract_id() == key_b58 {
                return Some(prefix.clone());
            }
        }
    }
    None
}

/// Handle a GET timeout -- try restoring from delegate backup.
pub fn handle_get_timeout(_error_msg: &str) {
    // Try backup for any site that still has default (empty) state
    let sites = state::SITES.read();
    for (prefix, site) in sites.iter() {
        if site.state == delta_core::SiteState::default() {
            log(&format!(
                "Delta: GET timed out, trying delegate backup for site {prefix}"
            ));
            super::delegate::request_site_state_backup(prefix);
        }
    }
}

fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(s: &str) -> [u8; 32] {
        let bytes: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        arr
    }

    fn pending(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn migrating(prefixes: &[&str]) -> BTreeSet<String> {
        prefixes.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn classifier_routes_pending_migration_key_even_if_prefix_not_migrating() {
        // A legacy-hash probe response must route through the
        // migration branch because the key itself is in
        // PENDING_MIGRATIONS, regardless of whether the prefix is
        // still in MIGRATING_PREFIXES. The caller then reconciles the
        // state via the tombstone-aware merge (newest wins, deletions
        // preserved) — late arrivals are merged, never dropped.
        let pending = pending(&[("legacy_key_b58", "abcdef1234")]);
        let migrating = migrating(&[]);
        let c = classify_get_response("legacy_key_b58", &pending, &migrating, None);
        assert_eq!(
            c,
            GetClassification::PendingMigration {
                prefix: "abcdef1234".to_string()
            }
        );
    }

    #[test]
    fn classifier_routes_current_key_during_initial_capture() {
        // GET for the current contract key (not in PENDING_MIGRATIONS)
        // while its prefix is still in the initial-capture window must
        // be treated as `InitialCurrentKey` so the caller can cancel
        // sibling legacy probes once this one succeeds.
        let pending = pending(&[]);
        let migrating = migrating(&["abcdef1234"]);
        let c = classify_get_response("current_key_b58", &pending, &migrating, Some("abcdef1234"));
        assert_eq!(
            c,
            GetClassification::InitialCurrentKey {
                prefix: "abcdef1234".to_string()
            }
        );
    }

    #[test]
    fn classifier_routes_post_capture_to_live_update() {
        // Same current-key GET *after* the prefix has been removed
        // from MIGRATING_PREFIXES must route to LiveUpdate so normal
        // UpdateNotification merging continues to work in steady state.
        let pending = pending(&[]);
        let migrating = migrating(&[]);
        let c = classify_get_response("current_key_b58", &pending, &migrating, Some("abcdef1234"));
        assert_eq!(c, GetClassification::LiveUpdate);
    }

    #[test]
    fn classifier_routes_unknown_key_to_unknown() {
        let pending = pending(&[]);
        let migrating = migrating(&[]);
        let c = classify_get_response("mystery_key", &pending, &migrating, None);
        assert_eq!(c, GetClassification::Unknown);
    }

    #[test]
    fn classifier_prefers_pending_over_migrating_set() {
        // If a key is BOTH in PENDING_MIGRATIONS and its prefix is in
        // MIGRATING_PREFIXES, the pending branch must win — we need
        // to run the migration PUT, which the InitialCurrentKey
        // branch would skip.
        let pending = pending(&[("legacy_key", "abcdef1234")]);
        let migrating = migrating(&["abcdef1234"]);
        let c = classify_get_response("legacy_key", &pending, &migrating, Some("abcdef1234"));
        assert_eq!(
            c,
            GetClassification::PendingMigration {
                prefix: "abcdef1234".to_string()
            }
        );
    }

    #[test]
    fn contract_id_is_deterministic_and_depends_on_both_hash_and_prefix() {
        // Different hashes must produce different IDs for the same prefix.
        let h1 = hex32("1188d108180a4143e6e4107b193cb90d5c08644e3830499f46186f141f182e81");
        let h2 = hex32("b92da83dae278fcdc237d976ec926ee2fdca20e817662ae8a3aeaf09aaf47fa4");
        let id1 = contract_id_for_prefix_with_hash("abcdef1234", &h1);
        let id2 = contract_id_for_prefix_with_hash("abcdef1234", &h2);
        assert_ne!(id1, id2, "different WASM hashes must yield different keys");

        // Same inputs must be deterministic.
        assert_eq!(id1, contract_id_for_prefix_with_hash("abcdef1234", &h1));

        // Different prefixes under the same hash must differ.
        let id_other = contract_id_for_prefix_with_hash("wxyz123456", &h1);
        assert_ne!(id1, id_other);
    }

    #[test]
    fn legacy_ids_are_deduplicated_and_exclude_current() {
        // Pretend the "current" key matches one of the legacy hashes by
        // passing its base58 as current_id_b58; that hash must drop out
        // of the probe set so the UI doesn't redundantly GET its own key.
        let legacy = hex32("1188d108180a4143e6e4107b193cb90d5c08644e3830499f46186f141f182e81");
        let pretend_current = contract_id_for_prefix_with_hash("abcdef1234", &legacy);

        let ids = legacy_contract_ids_for_prefix("abcdef1234", &pretend_current);
        assert!(
            !ids.contains(&pretend_current),
            "current key must be filtered out of the legacy probe set"
        );

        // Whatever comes back must be a unique set — the test doesn't
        // know the exact count (depends on legacy_contracts.toml) but
        // it must match the de-duplicated expectation.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "legacy ids must be unique");
    }

    #[test]
    fn contract_id_matches_state_key_derivation_for_current_wasm() {
        // Cross-consistency guard: `contract_id_for_prefix_with_hash`
        // (used for legacy probes) and `state::contract_key_from_prefix`
        // (used for the current key) must agree when called with the
        // current WASM's hash. If freenet-stdlib ever changes its
        // `ContractKey::from_params_and_code` internals in a
        // backwards-incompatible way, this test catches it before a
        // release strands users.
        let current_wasm_hash: [u8; 32] = blake3::hash(SITE_CONTRACT_WASM).into();
        let prefix = "abcdef1234";
        let ours = contract_id_for_prefix_with_hash(prefix, &current_wasm_hash);
        let theirs = state::contract_key_from_prefix(prefix).encoded_contract_id();
        assert_eq!(
            ours, theirs,
            "legacy-probe key derivation must match state::contract_key_from_prefix; \
             freenet-stdlib may have changed ContractKey::from_params_and_code"
        );
    }

    fn key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    /// Build a real, fully-signed `SiteState` with the given pages
    /// (`(page_id, content, updated_at)`). Real signatures are required
    /// because `reconcile_into`'s merge verifies the incoming state.
    fn signed_state(owner: &ed25519_dalek::SigningKey, pages: &[(u64, &str, u64)]) -> SiteState {
        let mut s = SiteState::new(delta_core::SiteConfig::default(), owner);
        for (id, content, ts) in pages {
            let p = delta_core::Page::new(*id, "t".into(), content.to_string(), *ts, owner);
            s.upsert_page(*id, p, &owner.verifying_key()).unwrap();
        }
        s
    }

    /// Review follow-up: `handle_site_delta` used to apply page updates and
    /// deletions inline, without verifying signatures or recording a
    /// tombstone. `apply_delta_to_site_state` now delegates to
    /// `SiteState::apply_delta`, so this must reject a deletion signed by
    /// someone other than the site's owner and leave the page untouched.
    /// This is the client-side half of the authentication #40 added on the
    /// contract side: without it, a delta arriving over the network could
    /// delete a page with no valid proof the owner authorized it.
    #[test]
    fn apply_delta_to_site_state_rejects_a_deletion_not_signed_by_the_owner() {
        let owner = key(1);
        let attacker = key(2);
        let mut site_state = signed_state(&owner, &[(1, "home", 1_000)]);

        let forged_deletion = delta_core::SignedPageDeletion::new(1, 2_000, &attacker);
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates: BTreeMap::new(),
            page_deletions: vec![forged_deletion],
        };

        let result = apply_delta_to_site_state(&mut site_state, &delta);
        assert!(
            result.is_err(),
            "a deletion not signed by the site owner must be rejected"
        );
        assert!(
            site_state.pages.contains_key(&1),
            "the page must survive an unauthenticated deletion attempt"
        );
        assert!(
            !site_state.deleted_pages.contains_key(&1),
            "no tombstone should be recorded for a rejected deletion"
        );
    }

    /// The matching positive case: a genuinely owner-signed deletion is
    /// applied AND recorded as a tombstone (the second gap the old inline
    /// logic had: it removed the page from `pages` but never touched
    /// `deleted_pages`, so the delete was silently undone by the next GET,
    /// delegate-backup restore, or legacy-contract sweep).
    #[test]
    fn apply_delta_to_site_state_applies_and_records_a_valid_deletion() {
        let owner = key(1);
        let mut site_state = signed_state(&owner, &[(1, "home", 1_000)]);

        let deletion = delta_core::SignedPageDeletion::new(1, 2_000, &owner);
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates: BTreeMap::new(),
            page_deletions: vec![deletion],
        };

        let name = apply_delta_to_site_state(&mut site_state, &delta)
            .expect("an owner-signed deletion must be accepted");
        assert_eq!(name, site_state.config.config.name);
        assert!(
            !site_state.pages.contains_key(&1),
            "the page must be removed"
        );
        assert!(
            site_state.deleted_pages.contains_key(&1),
            "the tombstone must be recorded so a later merge/reconcile doesn't resurrect the page"
        );
    }

    /// Review follow-up: the two tests above exercise `apply_delta_to_site_state`
    /// directly, but nothing pinned that `handle_site_delta` actually CALLS
    /// it. Reverting `handle_site_delta`'s body to the pre-fix inline logic
    /// (no verification, no tombstone) while leaving the helper untouched
    /// leaves those two tests green and the whole workspace green — this
    /// source-scrape pin is what catches that. The length bound guards
    /// against the `include_str!` self-match trap (this very test's own
    /// source contains the literal `"fn handle_site_delta("` string it
    /// splits on, so an unbounded slice would silently include the rest of
    /// the file instead of failing loudly).
    #[test]
    fn handle_site_delta_delegates_to_the_verified_applier() {
        let src = include_str!("operations.rs");
        let after = src
            .split("fn handle_site_delta(")
            .nth(1)
            .expect("handle_site_delta must exist");
        let body = after
            .split("\n/// Subscribe to a site contract")
            .next()
            .expect("the anchor after handle_site_delta must exist");
        assert!(body.len() < 2000, "slice unbounded ({} bytes)", body.len());
        assert!(
            body.contains("apply_delta_to_site_state("),
            "handle_site_delta must delegate to apply_delta_to_site_state"
        );
        assert!(
            !body.contains("state.pages.insert"),
            "handle_site_delta must not re-inline unverified page insertion"
        );
        assert!(
            !body.contains("state.pages.remove"),
            "handle_site_delta must not re-inline unverified page removal"
        );
    }

    /// Review follow-up (both lenses independently found this): a delta
    /// with an EARLIER valid field and a LATER field forged by a different
    /// key (only constructible by a malicious same-owner key, e.g. a
    /// compromised secondary signing key) must not leave the valid field's
    /// mutation applied. The single-field rejection tests above can't catch
    /// this (nothing else to partially apply at cardinality 1); this uses a
    /// mixed delta specifically.
    #[test]
    fn apply_delta_to_site_state_rejects_a_mixed_valid_and_forged_delta_atomically() {
        let owner = key(1);
        let attacker = key(2);
        let mut site_state = signed_state(&owner, &[(1, "home", 1_000)]);
        let before = site_state.clone();

        // A validly-signed update to page 1, plus a deletion forged by a
        // different key.
        let valid_page = delta_core::Page::new(1, "t".into(), "updated".into(), 2_000, &owner);
        let mut page_updates = BTreeMap::new();
        page_updates.insert(1, valid_page);
        let forged_deletion = delta_core::SignedPageDeletion::new(2, 3_000, &attacker);
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates,
            page_deletions: vec![forged_deletion],
        };

        let result = apply_delta_to_site_state(&mut site_state, &delta);
        assert!(result.is_err(), "the forged deletion must cause rejection");
        assert_eq!(
            site_state, before,
            "a rejected delta must not leave the valid field's mutation applied"
        );
    }

    #[test]
    fn reconcile_adopts_newer_content_but_not_older() {
        // Bug 1 ("read rolled back to April"): the newer generation's content
        // must win, and an OLDER generation must NOT clobber it — regardless
        // of arrival order.
        let owner = key(1);
        let older = signed_state(&owner, &[(1, "april", 1_000)]);
        let newer = signed_state(&owner, &[(1, "current", 5_000)]);

        // Older-then-newer: newer wins and the state changes.
        let mut s = older.clone();
        assert!(reconcile_into(&mut s, &newer));
        assert_eq!(s.pages[&1].content, "current");

        // Newer-then-older: the older generation adds nothing (no clobber).
        let mut s = newer.clone();
        assert!(!reconcile_into(&mut s, &older));
        assert_eq!(s.pages[&1].content, "current");
    }

    #[test]
    fn reconcile_preserves_deletion_of_newest_page() {
        // MUST-FIX (review): deleting the NEWEST page lowers max(updated_at),
        // so a scalar-recency wholesale replace would let a pre-deletion
        // snapshot out-rank (and resurrect) the true post-deletion state.
        // The tombstone-aware merge must keep the deletion in BOTH orders.
        let owner = key(2);
        let mut pre_delete = signed_state(&owner, &[(1, "home", 100), (2, "newest", 200)]);
        // Post-delete state: page 2 (the newest) deleted, leaving page 1.
        let mut post_delete = pre_delete.clone();
        let deletion = delta_core::SignedPageDeletion::new(2, 300, &owner);
        post_delete
            .delete_page(&deletion, &owner.verifying_key())
            .unwrap();

        // pre-delete state reconciled INTO post-delete: page 2 stays deleted.
        let mut s = post_delete.clone();
        assert!(
            !reconcile_into(&mut s, &pre_delete),
            "resurrecting a deleted page must be a no-op"
        );
        assert!(!s.pages.contains_key(&2), "deleted page must not reappear");

        // Reverse order (post-delete reconciled INTO pre-delete): the delete
        // propagates and page 2 is removed.
        assert!(reconcile_into(&mut pre_delete, &post_delete));
        assert!(
            !pre_delete.pages.contains_key(&2),
            "delete must propagate through merge"
        );
    }

    #[test]
    fn reconcile_first_capture_and_empty() {
        let owner = key(3);
        let real = signed_state(&owner, &[(1, "x", 100)]);

        // First capture: empty placeholder adopts the incoming wholesale.
        let mut s = SiteState::default();
        assert!(reconcile_into(&mut s, &real));
        assert_eq!(s.pages[&1].content, "x");

        // An empty incoming never changes a non-empty state.
        let mut s = real.clone();
        assert!(!reconcile_into(&mut s, &SiteState::default()));
    }

    #[test]
    fn reconcile_preserves_higher_next_page_id() {
        // F3 (review): `SiteState::merge` advances next_page_id only on live
        // inserts, so a merge with a generation whose highest page was
        // created-then-deleted (possibly leaving NO tombstone, pre-tombstone
        // era) would lose the counter and let `create_page` reuse a deleted id.
        // reconcile_into must carry the higher counter forward.
        let owner = key(7);
        let mut existing = signed_state(&owner, &[(1, "home", 100)]); // next_page_id = 2
        let mut incoming = signed_state(&owner, &[(1, "home", 100)]);
        // incoming knew about ids up to 8 (highest created-then-deleted, no
        // tombstone), so its counter is ahead even though it has no new pages.
        incoming.next_page_id = 9;

        reconcile_into(&mut existing, &incoming);
        assert!(
            existing.next_page_id >= 9,
            "the higher id counter must be preserved across the merge (got {})",
            existing.next_page_id
        );
    }

    #[test]
    fn reconcile_rejects_different_owner() {
        // Two different owners must never be blended (prefix collision / a
        // contract-key-resolved mismatch) — that would corrupt both sites.
        let owner_a = key(4);
        let owner_b = key(5);
        let mut a = signed_state(&owner_a, &[(1, "a", 100)]);
        let b = signed_state(&owner_b, &[(1, "b", 999)]);
        assert!(!reconcile_into(&mut a, &b));
        assert_eq!(a.pages[&1].content, "a");
    }

    /// Same mechanism, same reasoning, as
    /// `the_baked_delegate_registry_matches_the_file_on_disk` in `delegate.rs`:
    /// `include_str!` is tracked by rustc's dep-info independently of the build
    /// script's fingerprint, so a stale or empty bake of
    /// `LEGACY_CONTRACT_HASHES` goes red here. A build-script assertion cannot
    /// catch staleness, because in the stale case the script does not run.
    ///
    /// A stale contract table means sites whose stored `contract_key_b58` is
    /// missing or refers to a hash no longer on the network lose their
    /// multi-hop migration fallback, so their state is simply not found.
    ///
    /// Same two limitations as the delegate pin: a genuinely cold build cannot
    /// be stale, though CI is routinely WARM (`ci.yml` caches `target` with
    /// `restore-keys`), so this does have teeth there; and it observes the host
    /// bake, while `dx build --release` uses a separate wasm32 fingerprint and
    /// `OUT_DIR` — strong evidence about the shipped artifact, not a direct
    /// check of it.
    #[test]
    fn the_baked_contract_registry_matches_the_file_on_disk() {
        let toml = include_str!("../../../legacy_contracts.toml");
        let declared = toml.lines().filter(|l| l.trim() == "[[entry]]").count();

        assert!(
            declared > 0,
            "legacy_contracts.toml declares no entries at all — this registry \
             is append-only and can never legitimately be empty"
        );
        assert_eq!(
            LEGACY_CONTRACT_HASHES.len(),
            declared,
            "the baked-in contract table has {} entries but \
             legacy_contracts.toml declares {}. The bundle would ship a STALE \
             or EMPTY table and the multi-hop state-migration fallback would \
             silently stop finding older generations.",
            LEGACY_CONTRACT_HASHES.len(),
            declared
        );
    }

    #[test]
    fn newest_legacy_contract_id_is_the_last_entry_and_excludes_current() {
        // The self-heal re-probe must target the newest legacy generation
        // (last entry in legacy_contracts.toml), and must never re-probe
        // the current key itself.
        let prefix = "abcdef1234";
        let newest_hash = LEGACY_CONTRACT_HASHES.last().copied().unwrap();
        let expected = contract_id_for_prefix_with_hash(prefix, &newest_hash);
        let got = newest_legacy_contract_id_for_prefix(prefix, "some_other_current_key").unwrap();
        assert_eq!(got, expected);

        // If the newest legacy hash IS the current key, fall through to the
        // next-newest rather than re-probing the current key.
        let got2 = newest_legacy_contract_id_for_prefix(prefix, &expected);
        assert_ne!(got2.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn legacy_contract_hashes_table_is_populated() {
        // Guard against an empty or mis-generated legacy_contracts.rs:
        // without at least one entry, users of the immediately-preceding
        // release who hit the no-stored-key path have no fallback.
        assert!(
            !LEGACY_CONTRACT_HASHES.is_empty(),
            "legacy_contracts.toml must contain at least one entry so that users \
             of the previous release can migrate their site state"
        );
    }

    // -----------------------------------------------------------------------
    // freenet-migrate driver adoption (delta#33/#34, freenet-migrate#6)
    // -----------------------------------------------------------------------

    use freenet_migrate::contract_id_from_code_hash;
    use freenet_migrate::driver::policy_check;

    fn cid(n: u8) -> ContractInstanceId {
        ContractInstanceId::new([n; 32])
    }

    fn cbor(state: &SiteState) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(state, &mut buf).expect("CBOR state");
        buf
    }

    /// A representative spread of same-owner generations that exercises the
    /// FoldAll preconditions: a base page, a superset with a second page, the
    /// #34 bug-B state (the second page deleted via tombstone), and a newer
    /// revision of the base page. Config is held constant and content changes
    /// bump `updated_at`, so these samples exercise the HONEST domain over which
    /// the merge is commutative + idempotent + order-invariant: distinct
    /// `updated_at`/`version` per write, and at most one deletion per page. The
    /// THREE known exclusions (all documented at the FoldAll header, all
    /// pre-existing, all shared by today's concurrent sweep): (a) two writes
    /// conflicting at an EQUAL `updated_at`/`version` resolve keep-accumulator
    /// (order-dependent — a concurrent-tab edge, not generational drift); (b) the
    /// same page deleted twice at different `deleted_at` keeps the accumulator's
    /// tombstone (order-dependent in the recorded timestamp, but the page stays
    /// deleted either way — see
    /// `conflicting_tombstone_deleted_at_is_order_dependent_but_page_stays_deleted`);
    /// (c) a pre-tombstone (C1) delete can resurrect. These samples deliberately
    /// avoid (a) and (b) because neither occurs across genuine generations and
    /// neither is what FoldAll folds.
    fn fold_samples(owner: &ed25519_dalek::SigningKey) -> Vec<SiteState> {
        let s1 = signed_state(owner, &[(1, "home", 100)]);
        let s2 = signed_state(owner, &[(1, "home", 100), (2, "about", 200)]);
        let mut s3 = signed_state(owner, &[(1, "home", 100)]);
        s3.delete_page(
            &delta_core::SignedPageDeletion::new(2, 300, owner),
            &owner.verifying_key(),
        )
        .unwrap();
        let s4 = signed_state(owner, &[(1, "updated", 500)]);
        vec![s1, s2, s3, s4]
    }

    #[test]
    fn merge_is_fold_all_sound_over_the_honest_generational_domain() {
        // G1.5: FoldAll is only sound when the merge is commutative +
        // idempotent + fold-order-invariant. `DeltaProbeOps::merge_generations`
        // delegates to `reconcile_into`, so this proves Delta's incident fix
        // satisfies the FoldAll preconditions OVER THE HONEST DOMAIN — distinct
        // `updated_at`/`version` per write and at most one deletion per page, the
        // domain `fold_samples` exercises. The three documented exclusions (equal
        // -timestamp conflicting writes; the same page deleted twice at different
        // `deleted_at`; pre-tombstone-C1 resurrection) are order-dependent, all
        // pre-existing and shared by today's sweep, and are pinned by their own
        // tests rather than asserted commutative here. If any assertion below
        // fails, FoldAll is NOT sound for Delta and the adoption must stop.
        let owner = key(1);
        let samples = fold_samples(&owner);
        let merge = |a: SiteState, b: SiteState| DeltaProbeOps.merge_generations(a, b);
        policy_check::assert_merge_commutative(&samples, merge);
        policy_check::assert_merge_idempotent(&samples, merge);
        policy_check::assert_fold_order_invariant(&samples, merge);
    }

    #[test]
    fn driver_fold_preserves_deletion_of_newest_page_in_both_orders() {
        // The #34 bug-B case driven through the ACTUAL driver: deleting the
        // NEWEST page lowers max(updated_at), which a scalar-recency selector
        // would treat as "older" and resurrect. The tombstone-aware fold must
        // keep the deletion regardless of which generation the deletion lands
        // in — mirroring `reconcile_preserves_deletion_of_newest_page`, but
        // through `ProbeDriver` + `SelectionPolicy::FoldAll`.
        let owner = key(2);
        let full = signed_state(&owner, &[(1, "home", 100), (2, "newest", 200)]);
        let mut deleted = full.clone();
        deleted
            .delete_page(
                &delta_core::SignedPageDeletion::new(2, 300, &owner),
                &owner.verifying_key(),
            )
            .unwrap();

        for order in [
            [deleted.clone(), full.clone()],
            [full.clone(), deleted.clone()],
        ] {
            let responses: HashMap<ContractInstanceId, Option<Vec<u8>>> = HashMap::from([
                (cid(2), Some(cbor(&order[0]))), // newest candidate
                (cid(1), Some(cbor(&order[1]))), // oldest candidate
            ]);
            let candidates = NewestFirst::assume_ordered(vec![cid(2), cid(1)]);
            let outcome = drive_fold_probe(
                candidates,
                &responses,
                SiteState::default(),
                DEFAULT_MAX_PROBE_HOPS,
            );
            let Outcome::Recovered {
                merged,
                truncated_fold,
                ..
            } = outcome
            else {
                panic!("expected recovery, got a non-Recovered outcome");
            };
            assert!(
                !truncated_fold,
                "a 2-candidate fold cannot truncate at 64 hops"
            );
            assert!(
                !merged.pages.contains_key(&2),
                "deleted-newest page resurrected (arrival order {order:?})"
            );
            assert!(
                merged.deleted_pages.contains_key(&2),
                "tombstone must survive the fold (arrival order {order:?})"
            );
        }
    }

    #[test]
    fn driver_probes_newest_generation_first() {
        // The generation-ordering pin: `NewestFirst::from_lineage` sorts by the
        // registry `generation` field descending, so the driver's first GET is
        // the newest generation — the anti-rollback ordering the whole fix
        // rests on. (For FoldAll the RESULT is order-independent, but the probe
        // order and `source` still follow generation.)
        let params = Parameters::from(cbor_site_params("abcdef1234"));
        let lineage = [
            ContractLineageEntry {
                generation: 0,
                code_hash: [10; 32],
                note: "older",
            },
            ContractLineageEntry {
                generation: 1,
                code_hash: [11; 32],
                note: "newer",
            },
        ];
        let candidates = NewestFirst::from_lineage(&params, &lineage);
        let mut driver = ProbeDriver::new(
            DeltaProbeOps,
            SiteState::default(),
            candidates,
            SelectionPolicy::FoldAll(
                FoldAllAck::i_understand_fold_all_resurrects_without_tombstones(),
            ),
        );
        let Step::Get(first) = driver.next_action() else {
            panic!("expected a GET step");
        };
        assert_eq!(
            first,
            contract_id_from_code_hash(&[11; 32], &params),
            "the newest generation (gen 1) must be probed first"
        );
    }

    #[test]
    fn legacy_lineage_orders_registry_newest_first() {
        // `legacy_lineage_newest_first` must present the registry's LAST entry
        // (newest by `legacy_contracts.toml` order) as the first candidate.
        let prefix = "abcdef1234";
        let params = Parameters::from(cbor_site_params(prefix));
        let candidates = legacy_lineage_newest_first(prefix);
        let mut driver = ProbeDriver::new(
            DeltaProbeOps,
            SiteState::default(),
            candidates,
            SelectionPolicy::FoldAll(
                FoldAllAck::i_understand_fold_all_resurrects_without_tombstones(),
            ),
        );
        let Step::Get(first) = driver.next_action() else {
            panic!("expected a GET step");
        };
        let newest_hash = LEGACY_CONTRACT_HASHES.last().copied().unwrap();
        assert_eq!(
            first,
            contract_id_from_code_hash(&newest_hash, &params),
            "the newest legacy generation must be probed first"
        );
    }

    #[test]
    fn driver_candidate_ids_match_delta_derivation() {
        // The driver's candidate ids (blake3(code_hash ‖ params) via
        // `contract_id_from_code_hash`) must be byte-identical to Delta's own
        // `contract_id_for_prefix_with_hash`, or the driver would probe the
        // wrong keys. Guards against a future stdlib change to the id encoding.
        let prefix = "abcdef1234";
        let params = Parameters::from(cbor_site_params(prefix));
        for hash in LEGACY_CONTRACT_HASHES {
            let via_driver = contract_id_from_code_hash(hash, &params).encode();
            let via_delta = contract_id_for_prefix_with_hash(prefix, hash);
            assert_eq!(
                via_driver, via_delta,
                "driver candidate id derivation diverged from Delta's contract key derivation"
            );
        }
    }

    #[test]
    fn driver_fold_is_independent_of_arrival_position() {
        // Out-of-order delivery: whichever probe position an older vs newer
        // generation lands in, the fold keeps the newer content (recency is by
        // `updated_at`, not probe order) and the merged result is identical.
        let owner = key(6);
        let older = signed_state(&owner, &[(1, "old", 100)]);
        let newer = signed_state(&owner, &[(1, "new", 500)]);
        let candidates = NewestFirst::assume_ordered(vec![cid(2), cid(1)]);

        let responses_a: HashMap<ContractInstanceId, Option<Vec<u8>>> =
            HashMap::from([(cid(2), Some(cbor(&newer))), (cid(1), Some(cbor(&older)))]);
        let responses_b: HashMap<ContractInstanceId, Option<Vec<u8>>> =
            HashMap::from([(cid(2), Some(cbor(&older))), (cid(1), Some(cbor(&newer)))]);

        let merged_a = match drive_fold_probe(
            candidates.clone(),
            &responses_a,
            SiteState::default(),
            DEFAULT_MAX_PROBE_HOPS,
        ) {
            Outcome::Recovered { merged, .. } => merged,
            other => panic!("expected recovery, got {other:?}"),
        };
        let merged_b = match drive_fold_probe(
            candidates,
            &responses_b,
            SiteState::default(),
            DEFAULT_MAX_PROBE_HOPS,
        ) {
            Outcome::Recovered { merged, .. } => merged,
            other => panic!("expected recovery, got {other:?}"),
        };

        assert_eq!(merged_a.pages[&1].content, "new");
        assert_eq!(merged_a.pages[&1].updated_at, 500);
        assert_eq!(
            merged_a, merged_b,
            "fold result must not depend on which probe position a generation arrives at"
        );
    }

    #[test]
    fn driver_all_miss_seeds_local_snapshot() {
        // Local-only data must survive a sweep where every candidate is a miss
        // (NotFound / undecodable) — the no-silent-data-loss guarantee, now via
        // the driver. `SeedLocal` carries the local snapshot forward unchanged.
        let owner = key(8);
        let local = signed_state(&owner, &[(1, "local-only", 700)]);
        let responses: HashMap<ContractInstanceId, Option<Vec<u8>>> =
            HashMap::from([(cid(2), None), (cid(1), Some(b"undecodable".to_vec()))]);
        let candidates = NewestFirst::assume_ordered(vec![cid(2), cid(1)]);
        let outcome = drive_fold_probe(
            candidates,
            &responses,
            local.clone(),
            DEFAULT_MAX_PROBE_HOPS,
        );
        let Outcome::SeedLocal { local: seeded } = outcome else {
            panic!("all-miss sweep must seed the local snapshot");
        };
        assert_eq!(
            seeded, local,
            "local-only data must survive an all-miss sweep"
        );
    }

    #[test]
    fn sweep_completes_only_when_every_candidate_resolves() {
        let mut sweep = MigrationSweep::default();
        sweep.register("a");
        sweep.register("b");
        sweep.register("c");
        assert!(!sweep.is_complete(), "no candidate resolved yet");

        // A GET-state resolution that changed local state.
        assert!(
            !sweep.resolve("a", Some(vec![1, 2, 3]), true),
            "1 of 3 resolved"
        );
        // A miss (NotFound) that did not change local state.
        assert!(!sweep.resolve("b", None, false), "2 of 3 resolved");
        // Final candidate resolves -> complete.
        assert!(sweep.resolve("c", Some(vec![4]), false), "all 3 resolved");
        assert!(sweep.is_complete());
        assert!(
            sweep.changed_local,
            "changed_local OR-accumulates across responses"
        );
    }

    #[test]
    fn sweep_register_and_resolve_are_idempotent() {
        let mut sweep = MigrationSweep::default();
        sweep.register("a");
        sweep.register("a"); // duplicate fire
        sweep.register("b");
        assert_eq!(
            sweep.candidates.len(),
            2,
            "duplicate register is one candidate"
        );

        assert!(!sweep.resolve("a", Some(vec![1]), true), "1 of 2");
        // Duplicate delivery for an already-resolved candidate must not
        // re-count, must not flip completion, and must not clobber the recorded
        // response.
        assert!(
            !sweep.resolve("a", Some(vec![9, 9]), false),
            "duplicate delivery is ignored"
        );
        assert_eq!(
            sweep.responses.get("a"),
            Some(&Some(vec![1])),
            "first response kept"
        );
        assert!(
            sweep.changed_local,
            "duplicate must not clear an earlier change flag"
        );

        assert!(sweep.resolve("b", None, false), "b completes the sweep");
    }

    #[test]
    fn sweep_ignores_non_candidate_keys_and_empty_is_never_complete() {
        let mut sweep = MigrationSweep::default();
        assert!(
            !sweep.is_complete(),
            "an empty sweep has nothing to finalize"
        );
        sweep.register("a");
        // A response for a key that was never fired for this prefix is ignored.
        assert!(!sweep.resolve("not-a-candidate", Some(vec![1]), true));
        assert!(
            !sweep.changed_local,
            "a non-candidate must not set changed_local"
        );
        assert!(
            sweep.responses.is_empty(),
            "a non-candidate must not be recorded"
        );
        assert!(
            sweep.resolve("a", None, false),
            "resolving the sole candidate completes it"
        );
    }

    #[test]
    fn sweep_forward_put_gate_follows_changed_local() {
        // The write gate: `plan_sweep_forward_put` forward-PUTs iff a legacy
        // generation actually changed SITES (changed_local). A dominated sweep
        // must NOT forward-PUT (no write-amplification) — the exact pre-driver
        // condition.
        let owner = key(4);
        let real = signed_state(&owner, &[(1, "x", 100)]);
        let k = cid(7).encode();

        let mut changed = MigrationSweep::default();
        changed.register(&k);
        changed.resolve(&k, Some(cbor(&real)), true);
        assert!(
            plan_sweep_forward_put("abcdef1234", &changed, SiteState::default()).forward_put,
            "a legacy generation that changed SITES must forward-PUT"
        );

        let mut dominated = MigrationSweep::default();
        dominated.register(&k);
        dominated.resolve(&k, Some(cbor(&real)), false);
        assert!(
            !plan_sweep_forward_put("abcdef1234", &dominated, real.clone()).forward_put,
            "a dominated sweep must NOT forward-PUT (no write-amplification)"
        );
    }

    #[test]
    fn bug_b_deletion_propagates_forward_only_when_current_lacks_it() {
        // Bug-B write-forward correctness: the forward PUT is gated on reconcile
        // having changed SITES. A legacy generation carrying a deletion the
        // current key LACKS changes SITES -> must forward-PUT so the deletion
        // propagates to the current key. The reverse (current already has the
        // deletion) is dominated -> must NOT forward-PUT.
        let owner = key(2);
        let pre_delete = signed_state(&owner, &[(1, "home", 100), (2, "newest", 200)]);
        let mut post_delete = pre_delete.clone();
        post_delete
            .delete_page(
                &delta_core::SignedPageDeletion::new(2, 300, &owner),
                &owner.verifying_key(),
            )
            .unwrap();
        let k = cid(7).encode();

        // Current key LACKS the deletion (pre_delete); a legacy generation HAS
        // it (post_delete). reconcile_into(current, legacy) changes SITES.
        let mut current = pre_delete.clone();
        let changed = reconcile_into(&mut current, &post_delete);
        assert!(
            changed,
            "the recovered deletion must change the pre-delete current state"
        );
        assert!(
            !current.pages.contains_key(&2),
            "the deletion is present in the state we PUT forward"
        );
        let mut sweep = MigrationSweep::default();
        sweep.register(&k);
        sweep.resolve(&k, Some(cbor(&post_delete)), changed);
        assert!(
            plan_sweep_forward_put("abcdef1234", &sweep, pre_delete.clone()).forward_put,
            "a recovered deletion the current key lacks must be written forward"
        );

        // Reverse: current key ALREADY has the deletion; the pre-delete
        // generation is dominated -> no change -> no forward PUT.
        let mut current2 = post_delete.clone();
        let changed2 = reconcile_into(&mut current2, &pre_delete);
        assert!(!changed2, "resurrecting a deleted page must be a no-op");
        let mut sweep2 = MigrationSweep::default();
        sweep2.register(&k);
        sweep2.resolve(&k, Some(cbor(&pre_delete)), changed2);
        assert!(
            !plan_sweep_forward_put("abcdef1234", &sweep2, post_delete.clone()).forward_put,
            "a dominated (deletion-already-present) generation must not write-amplify"
        );
    }

    #[test]
    fn plan_late_response_action_covers_finalize_and_late_cases() {
        // A still-tracked sweep -> normal resolve, regardless of change.
        assert_eq!(
            plan_late_response_action(true, true),
            LateResponseAction::Resolve
        );
        assert_eq!(
            plan_late_response_action(true, false),
            LateResponseAction::Resolve
        );
        // Sweep already finalized + a late response that CHANGED SITES -> one-off
        // forward PUT (the BLOCKING fix: a premature timeout completed the sweep,
        // then the real response arrived and reconciled newer data into SITES).
        assert_eq!(
            plan_late_response_action(false, true),
            LateResponseAction::ForwardPutNow
        );
        // Sweep already finalized + a late response that changed nothing ->
        // ignore (no write-amplification).
        assert_eq!(
            plan_late_response_action(false, false),
            LateResponseAction::Ignore
        );
    }

    #[test]
    fn resolve_ors_changed_local_for_a_late_but_live_response() {
        // A candidate resolved as a timeout-miss (changed=false); its real
        // response then arrives while the sweep is STILL live and DID change
        // SITES. `resolve` must OR changed_local so the eventual finalize
        // forward-PUTs the late data — without re-counting completion or
        // overwriting the first recorded response.
        let mut sweep = MigrationSweep::default();
        sweep.register("a");
        sweep.register("b");
        assert!(!sweep.resolve("a", None, false), "a timed out as a miss");
        assert!(!sweep.changed_local);
        assert!(
            !sweep.resolve("a", Some(vec![1]), true),
            "a late response for an already-resolved candidate must not re-complete"
        );
        assert!(
            sweep.changed_local,
            "a late change on a still-live sweep must still gate the forward PUT"
        );
        assert_eq!(
            sweep.responses.get("a"),
            Some(&None),
            "the first (miss) response is kept, not overwritten"
        );
        assert!(
            sweep.resolve("b", None, false),
            "completing b finalizes with changed_local carried"
        );
    }

    #[test]
    fn driver_marks_fold_truncated_past_the_fixed_hop_cap() {
        // The hop cap is FIXED at DEFAULT_MAX_PROBE_HOPS (not `len().max(64)`,
        // which could never truncate), so a lineage larger than the cap actually
        // truncates and the warning is real. Delta's registry is 5 (never
        // truncates); this pins the guard for a future lineage explosion.
        let owner = key(9);
        let real = signed_state(&owner, &[(1, "x", 100)]);
        let ids: Vec<ContractInstanceId> =
            (1..=(DEFAULT_MAX_PROBE_HOPS as u8 + 1)).map(cid).collect();
        let responses: HashMap<ContractInstanceId, Option<Vec<u8>>> =
            ids.iter().map(|id| (*id, Some(cbor(&real)))).collect();
        let outcome = drive_fold_probe(
            NewestFirst::assume_ordered(ids),
            &responses,
            SiteState::default(),
            DEFAULT_MAX_PROBE_HOPS,
        );
        let Outcome::Recovered { truncated_fold, .. } = outcome else {
            panic!("expected recovery");
        };
        assert!(
            truncated_fold,
            "a lineage exceeding the hop cap must mark the fold truncated"
        );
    }

    #[test]
    fn sweep_forward_put_gate_uses_recorded_current_key_state() {
        // Codex P2: a legacy response that beats the current-key response with
        // IDENTICAL state leaves changed_local = true, but the current key
        // already holds that state — so no PUT. Only a legacy generation that
        // recovered data the current key LACKS should PUT.
        let owner = key(4);
        let current = signed_state(&owner, &[(1, "home", 100)]);
        let with_extra = signed_state(&owner, &[(1, "home", 100), (2, "extra", 200)]);
        let k = cid(7).encode();

        // Legacy re-supplied identical state; SITES == current key state -> NO
        // put, even though changed_local is true (legacy first-captured).
        let mut identical = MigrationSweep::default();
        identical.register(&k);
        identical.resolve(&k, Some(cbor(&current)), true);
        identical.current_key_state = Some(current.clone());
        assert!(
            !plan_sweep_forward_put("abcdef1234", &identical, current.clone()).forward_put,
            "a legacy generation identical to the current key must not write-amplify"
        );

        // Legacy recovered data the current key lacks; SITES != current key
        // state -> PUT.
        let mut recovered = MigrationSweep::default();
        recovered.register(&k);
        recovered.resolve(&k, Some(cbor(&with_extra)), true);
        recovered.current_key_state = Some(current.clone());
        assert!(
            plan_sweep_forward_put("abcdef1234", &recovered, with_extra.clone()).forward_put,
            "a legacy generation that recovered data the current key lacks must PUT"
        );

        // Current key never returned real state -> fall back to changed_local.
        let mut no_ck = MigrationSweep::default();
        no_ck.register(&k);
        no_ck.resolve(&k, Some(cbor(&current)), true);
        assert!(no_ck.current_key_state.is_none());
        assert!(
            plan_sweep_forward_put("abcdef1234", &no_ck, current.clone()).forward_put,
            "with no current-key state recorded, fall back to changed_local (PUT)"
        );
    }

    #[test]
    fn stale_sweep_timeout_is_noop_against_a_superseded_generation() {
        // A per-candidate timeout armed for one sweep must not deliver a miss to
        // a later same-prefix sweep (River's per-fire route-token pattern).
        assert!(
            sweep_timeout_is_current(Some(5), 5),
            "matching generation fires"
        );
        assert!(
            !sweep_timeout_is_current(Some(6), 5),
            "a superseded sweep: the stale timeout no-ops"
        );
        assert!(
            !sweep_timeout_is_current(None, 5),
            "a finalized/removed sweep: the stale timeout no-ops"
        );
    }

    #[test]
    fn backup_after_current_key_puts_only_when_it_exceeds_the_current_state() {
        // Option-B unload-window fix: the delegate backup is requested AFTER the
        // current-key GET resolved, so by the time the (local) backup response
        // is reconciled, SITES already holds the current-key state S. The gate
        // handle_restored_site_state uses is `reconcile_into` returning "changed",
        // so it PUTs forward ONLY when the backup strictly exceeds S (genuine
        // recovery) — never when the backup merely equals the network current
        // state (no write-amplification). This pins that gate.
        let owner = key(4);
        let current = signed_state(&owner, &[(1, "home", 100)]); // S: network current key
        let backup_equal = current.clone(); // backup == S
        let backup_more = signed_state(&owner, &[(1, "home", 100), (2, "recovered", 200)]); // backup ⊋ S

        // Backup equal to the current state → no change → no forward PUT.
        let mut sites = current.clone();
        assert!(
            !reconcile_into(&mut sites, &backup_equal),
            "a backup equal to the current key state must not trigger a forward PUT"
        );

        // Backup carrying data the current key lacks → change → recovery PUT.
        let mut sites = current.clone();
        assert!(
            reconcile_into(&mut sites, &backup_more),
            "a backup exceeding the current key state must trigger the recovery PUT"
        );
        assert!(
            sites.pages.contains_key(&2),
            "the recovered page is present in the state we PUT forward"
        );
    }

    #[test]
    fn current_key_state_is_recorded_once_as_the_gate_baseline() {
        // The sweep's current-key baseline (used ONLY by the finalize
        // write-amplification gate) records the FIRST current-key state seen and
        // ignores later ones.
        let owner = key(4);
        let s1 = signed_state(&owner, &[(1, "home", 100)]);
        let s2 = signed_state(&owner, &[(1, "home", 100), (2, "extra", 200)]);

        let mut sweep = MigrationSweep::default();
        sweep.register("k");
        assert!(
            sweep.record_current_key_state(s1.clone()),
            "first recording"
        );
        assert!(
            !sweep.record_current_key_state(s2.clone()),
            "a later current-key response must not overwrite the baseline"
        );
        assert_eq!(
            sweep.current_key_state.as_ref(),
            Some(&s1),
            "the first current-key state is kept as the baseline"
        );
    }

    #[test]
    fn backup_requested_once_per_prefix_independent_of_sweep_and_content() {
        // Codex P1 (delta-check): the unload-window backup request must fire
        // exactly once per prefix per session, DECOUPLED from the sweep — so it
        // still fires when a one-candidate sweep finalized first, when the
        // current-key response is empty/undecodable, or when the site has no
        // legacy candidates. `should_request_backup` depends only on the
        // already-requested set, never on sweep state or response content.
        let mut requested = BTreeSet::new();

        // First current-key resolution for a prefix → request (whatever the
        // ordering or response content).
        assert!(
            should_request_backup(&requested, "aaaaaaaaaa"),
            "first current-key resolution requests the backup"
        );
        requested.insert("aaaaaaaaaa".to_string());
        // A second current-key resolution for the same prefix → no re-request
        // (e.g. an empty current-key response after a nonempty one, or a
        // steady-state re-GET).
        assert!(
            !should_request_backup(&requested, "aaaaaaaaaa"),
            "a second current-key resolution does not re-request"
        );
        // A different prefix requests independently.
        assert!(
            should_request_backup(&requested, "bbbbbbbbbb"),
            "a different prefix requests independently"
        );
    }

    #[test]
    fn baseline_update_prevents_a_second_identical_finalize_put() {
        // Codex P2: a restored-backup PUT of B (> current C) sets the sweep
        // baseline to B; finalize's "SITES != current-key" gate must then see
        // baseline == SITES and NOT fire a second identical PUT.
        let owner = key(4);
        let current = signed_state(&owner, &[(1, "home", 100)]); // C
        let recovered = signed_state(&owner, &[(1, "home", 100), (2, "recovered", 200)]); // B > C

        let mut sweep = MigrationSweep::default();
        sweep.register("k"); // legacy candidate that will miss
        sweep.resolve("k", None, false); // all legacy candidates miss
        sweep.record_current_key_state(current.clone()); // baseline = C
                                                         // Backup PUT of B happened outside finalize → baseline updated to B.
        sweep.set_current_key_baseline(recovered.clone());

        // Finalize now sees SITES (== B) equal to the baseline → NO second PUT.
        assert!(
            !plan_sweep_forward_put("abcdef1234", &sweep, recovered.clone()).forward_put,
            "finalize must not re-PUT state the restored-backup PUT already wrote"
        );
    }

    #[test]
    fn conflicting_tombstone_deleted_at_is_order_dependent_but_page_stays_deleted() {
        // Codex P3 honest-domain pin: `SiteState::merge`'s `deleted_pages`
        // or_insert keeps the ACCUMULATOR's deletion when the same page is
        // deleted at two different `deleted_at` (concurrent deletes). This is
        // order-dependent in the recorded timestamp — but HARMLESS: the page
        // stays deleted either way; only the recorded `deleted_at` differs. Pins
        // the current contract semantics (SiteState::merge is out of scope to
        // change), documenting why these samples are excluded from the
        // commutativity property set.
        let owner = key(2);
        let mut del_early = signed_state(&owner, &[(1, "home", 100)]);
        del_early
            .delete_page(
                &delta_core::SignedPageDeletion::new(2, 300, &owner),
                &owner.verifying_key(),
            )
            .unwrap();
        let mut del_late = signed_state(&owner, &[(1, "home", 100)]);
        del_late
            .delete_page(
                &delta_core::SignedPageDeletion::new(2, 900, &owner),
                &owner.verifying_key(),
            )
            .unwrap();

        let a = DeltaProbeOps.merge_generations(del_early.clone(), del_late.clone());
        let b = DeltaProbeOps.merge_generations(del_late.clone(), del_early.clone());

        // Page stays deleted in BOTH orders (the harmless part).
        assert!(!a.pages.contains_key(&2));
        assert!(!b.pages.contains_key(&2));
        assert!(a.deleted_pages.contains_key(&2));
        assert!(b.deleted_pages.contains_key(&2));
        // But the recorded deleted_at is the accumulator's → order-dependent.
        assert_eq!(
            a.deleted_pages[&2].deleted_at, 300,
            "keeps the accumulator's (early) deletion timestamp"
        );
        assert_eq!(
            b.deleted_pages[&2].deleted_at, 900,
            "keeps the accumulator's (late) deletion timestamp"
        );
        assert_ne!(
            a.deleted_pages[&2].deleted_at, b.deleted_pages[&2].deleted_at,
            "conflicting-tombstone deleted_at is order-dependent (documented, harmless)"
        );
    }
}
