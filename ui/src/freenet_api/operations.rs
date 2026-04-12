use ciborium::de::from_reader;
use delta_core::SiteState;
use dioxus::prelude::ReadableExt;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use std::sync::Arc;

use crate::state::{self, KnownSite, SiteRole};
use dioxus::prelude::*;
use std::collections::BTreeMap;

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
/// probing several historical contract WASM hashes at startup; the first
/// GET that returns non-empty state wins and the rest are cleared by
/// `clear_pending_migrations_for_prefix` to prevent an older state from
/// racing ahead of a newer one.
static PENDING_MIGRATIONS: GlobalSignal<BTreeMap<String, String>> =
    GlobalSignal::new(BTreeMap::new);

/// Handle an incoming response from the Freenet node.
pub fn handle_response(response: HostResponse) {
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

            // Check if this is a migration GET (old key)
            let migration_prefix = PENDING_MIGRATIONS.write().remove(&key_b58);

            let state_bytes = state.to_vec();
            if let Some(prefix) = &migration_prefix {
                if !state_bytes.is_empty() {
                    // Migration: PUT state to new contract key.
                    log(&format!(
                        "Delta: migrating state for site {prefix} from old key to new key"
                    ));
                    let new_key = state::contract_key_from_prefix(prefix);
                    handle_site_state(new_key, &state_bytes);
                    migrate_state_to_new_key(prefix, &state_bytes);
                    // Persist the new contract key so migration doesn't re-run
                    super::delegate::save_known_sites();
                    // Cancel any concurrent legacy-hash probes still
                    // in flight for this prefix — their responses, if
                    // they land after ours, would overwrite the fresh
                    // state with whatever lives at an older hash.
                    clear_pending_migrations_for_prefix(prefix);
                } else {
                    // Old state gone from network — don't eagerly retry
                    // the current key here. Several migration probes
                    // may be running for the same prefix; let each one
                    // resolve independently. If the current key itself
                    // was not separately GET'd by the caller, the
                    // NotFound branch below will still request it.
                    log(&format!(
                        "Delta: migration GET returned empty for site {prefix}"
                    ));
                }
            } else {
                if !state_bytes.is_empty() {
                    handle_site_state(key, &state_bytes);
                }
                // Subscribe AFTER successful GET
                subscribe_to_site(&key);
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

/// Process a full site state received from GET or full state update.
fn handle_site_state(key: ContractKey, state_bytes: &[u8]) {
    if state_bytes.is_empty() {
        return;
    }

    let site_state: SiteState = match from_reader(state_bytes) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("Delta: failed to deserialize site state: {e}"));
            return;
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

    // Don't re-add sites the user explicitly removed
    if state::REMOVED_PREFIXES.read().contains(&prefix) {
        log(&format!(
            "Delta: blocked re-add of removed site {prefix} from network response"
        ));
        return;
    }

    let mut sites = state::SITES.write();
    if let Some(existing) = sites.get_mut(&prefix) {
        existing.state = site_state;
        existing.name = name;
        existing.owner_pubkey = owner_pubkey;
        if existing.contract_key.is_none() {
            existing.contract_key = Some(key);
        }
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
    }
    drop(sites);

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

/// Drop every pending migration entry whose target is `prefix`. Called
/// after one migration GET resolves successfully, so that subsequent
/// responses from probes for the same prefix (racing older contract
/// hashes) can't overwrite the just-migrated state with stale data.
fn clear_pending_migrations_for_prefix(prefix: &str) {
    PENDING_MIGRATIONS.with_mut(|pending| {
        pending.retain(|_, p| p != prefix);
    });
}

/// Migrate state from old contract to new contract key.
/// PUTs the raw state bytes with the new WASM + params.
fn migrate_state_to_new_key(prefix: &str, state_bytes: &[u8]) {
    let params = delta_core::SiteParameters {
        prefix: prefix.to_string(),
    };
    let mut params_buf = Vec::new();
    ciborium::ser::into_writer(&params, &mut params_buf).expect("CBOR params");

    let state_buf = state_bytes.to_vec();

    log(&format!(
        "Delta: PUT migrated state ({} bytes) to new contract for site {prefix}",
        state_buf.len()
    ));

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
