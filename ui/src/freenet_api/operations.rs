use ciborium::de::from_reader;
use delta_core::SiteState;
use dioxus::prelude::ReadableExt;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use std::sync::Arc;

use crate::state::{self, KnownSite, SiteRole};
use dioxus::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

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
                        return;
                    }
                    // Reconcile this legacy generation into local state via a
                    // tombstone-aware merge (see `reconcile_into`): only genuinely
                    // new data is adopted, deletions on either side are preserved,
                    // and an older/dominated generation is a no-op. We do NOT drop
                    // late responses and do NOT cancel sibling probes, so a
                    // still-newer generation can also contribute. If the merge
                    // changed anything, PUT the RECONCILED local state (not the raw
                    // incoming bytes) forward to the current contract key so a
                    // deletion the merge preserved isn't undone on the current key.
                    let new_key = state::contract_key_from_prefix(&prefix);
                    if handle_site_state(new_key, &state_bytes) {
                        log(&format!(
                            "Delta: merged newer data for site {prefix}; \
                             migrating reconciled state to the current contract key"
                        ));
                        if let Some(merged) =
                            state::SITES.read().get(&prefix).map(|s| s.state.clone())
                        {
                            let params = delta_core::SiteParameters {
                                prefix: prefix.clone(),
                            };
                            put_site(&params, &merged);
                        }
                        super::delegate::save_known_sites();
                    } else {
                        log(&format!(
                            "Delta: legacy generation for site {prefix} added no new \
                             data (dominated by current); ignored"
                        ));
                    }
                    // Initial capture has progressed for this prefix; a
                    // subsequent current-key GET now flows through the
                    // LiveUpdate path (still merge-guarded).
                    MIGRATING_PREFIXES.with_mut(|set| {
                        set.remove(&prefix);
                    });
                }
                GetClassification::InitialCurrentKey { prefix } => {
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
            if PENDING_MIGRATIONS.write().remove(&key_b58).is_some() {
                log(&format!(
                    "Delta: legacy contract key {key_b58} has no state; \
                     another probe may still succeed"
                ));
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
        for (&page_id, page) in &delta.page_updates {
            site.state.pages.insert(page_id, page.clone());
            if page_id >= site.state.next_page_id {
                site.state.next_page_id = page_id + 1;
            }
        }
        for deletion in &delta.page_deletions {
            site.state.pages.remove(&deletion.page_id);
        }
        if let Some(config) = &delta.config {
            site.state.config = config.clone();
            site.name = config.config.name.clone();
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
    let params = delta_core::SiteParameters {
        prefix: prefix.to_string(),
    };
    let mut params_buf = Vec::new();
    ciborium::ser::into_writer(&params, &mut params_buf).expect("CBOR params");

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
}
