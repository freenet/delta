//! Delegate integration for signing and key persistence.
//!
//! All signing goes through the delegate. The UI sends content to sign,
//! the delegate signs with its stored key and returns the signed object.
//! The response handler then sends the signed data to the network.

#[allow(unused_imports)]
use ciborium::{de::from_reader, ser::into_writer};
use delta_core::{DelegateResponse, PageId};
use dioxus::prelude::*;
#[allow(unused_imports)]
use freenet_stdlib::client_api::ClientRequest;
#[allow(unused_imports)]
use freenet_stdlib::client_api::DelegateRequest as StdlibDelegateRequest;
use freenet_stdlib::prelude::*;
use std::collections::BTreeMap;

use crate::state;

/// Site delegate WASM.
const SITE_DELEGATE_WASM: &[u8] = include_bytes!("../../public/contracts/site_delegate.wasm");

// Legacy delegate keys for migration (auto-generated from legacy_delegates.toml).
include!(concat!(env!("OUT_DIR"), "/legacy_delegates.rs"));

/// Pending signed pages waiting to be sent to the network.
pub static PENDING_UPDATES: GlobalSignal<BTreeMap<(String, PageId), ContractKey>> =
    GlobalSignal::new(BTreeMap::new);

/// Pending config update waiting for delegate signature.
static PENDING_CONFIG: GlobalSignal<Option<ContractKey>> = GlobalSignal::new(|| None);

/// Legacy delegate that holds the signing key (when current delegate has none).
/// Stores (delegate_key_bytes, code_hash_bytes) so we can reconstruct the DelegateKey.
static LEGACY_SIGNING_DELEGATE: GlobalSignal<Option<([u8; 32], [u8; 32])>> =
    GlobalSignal::new(|| None);

/// Prefixes for which the CURRENT delegate has a signing key.
/// Used to decide whether to route signing through current vs legacy delegate.
static CURRENT_KEY_PREFIXES: GlobalSignal<Vec<String>> = GlobalSignal::new(Vec::new);

/// Whether the current delegate has ANY signing key (legacy single-key format).
static HAS_CURRENT_LEGACY_KEY: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Whether the current delegate's KnownSites has been loaded (and was non-empty).
/// When true, legacy KnownSites are ignored to respect site removals.
static CURRENT_SITES_LOADED: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Prefixes for which we've received PublicKey from any delegate (legacy or current).
/// Used to resolve race: PublicKey may arrive before KnownSites creates the site entry.
static OWNER_PREFIXES: GlobalSignal<Vec<String>> = GlobalSignal::new(Vec::new);

/// Whether legacy migration has already been fired. Deferring legacy queries
/// until the current delegate's KnownSites response arrives guarantees that
/// a legacy response cannot race ahead of the current one and resurrect
/// deleted sites.
static LEGACY_MIGRATION_FIRED: GlobalSignal<bool> = GlobalSignal::new(|| false);

// Tombstones let us persist removed prefixes across refreshes WITHOUT
// changing the delegate WASM schema — the delegate just stores/returns
// the Vec<KnownSiteRecord> as-is; the UI interprets sentinel entries via
// KnownSiteRecord::is_tombstone / KnownSiteRecord::tombstone in delta_core.

/// Register the site delegate with the Freenet node.
pub fn register_delegate() {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_bindgen_futures::spawn_local(async {
            let delegate_code = DelegateCode::from(SITE_DELEGATE_WASM.to_vec());
            let params = Parameters::from(Vec::<u8>::new());
            let delegate = Delegate::from((&delegate_code, &params));
            let container = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(delegate));

            let request = ClientRequest::DelegateOp(StdlibDelegateRequest::RegisterDelegate {
                delegate: container,
                cipher: StdlibDelegateRequest::DEFAULT_CIPHER,
                nonce: StdlibDelegateRequest::DEFAULT_NONCE,
            });

            let mut api = super::connection::WEB_API.write();
            if let Some(web_api) = api.as_mut() {
                match web_api.send(request).await {
                    Ok(_) => {
                        log("Delta: delegate registered");
                        drop(api);
                        // Load persisted data. Legacy migration is deferred
                        // until the current delegate's KnownSites response
                        // arrives (see the KnownSites arm of
                        // handle_delegate_response) — otherwise a legacy
                        // response could race ahead and resurrect sites the
                        // user removed.
                        request_public_key();
                        load_known_sites();
                    }
                    Err(e) => log(&format!("Delta: delegate registration failed: {e:?}")),
                }
            }
        });
    }
}

/// Store a signing key in the delegate's secret storage, keyed by site prefix.
pub fn store_signing_key(key_bytes: &[u8; 32], prefix: Option<&str>) {
    // Track that this prefix has a key in the current delegate
    if let Some(p) = prefix {
        CURRENT_KEY_PREFIXES.with_mut(|prefixes| {
            if !prefixes.contains(&p.to_string()) {
                prefixes.push(p.to_string());
            }
        });
    }
    let request = delta_core::DelegateRequest::StoreSigningKey {
        key_bytes: key_bytes.to_vec(),
        prefix: prefix.map(|s| s.to_string()),
    };
    send_delegate_request(&request);
}

/// Save the current known sites list to the delegate for persistence.
///
/// Tombstones for removed sites are stored alongside real entries (using a
/// sentinel name) so that deletions survive a page refresh. Without this,
/// a legacy delegate responding with old KnownSites could resurrect sites
/// the user explicitly removed.
pub fn save_known_sites() {
    let sites = state::SITES.read();
    let mut records: Vec<delta_core::KnownSiteRecord> = sites
        .values()
        .map(|s| delta_core::KnownSiteRecord {
            prefix: s.prefix.clone(),
            name: s.name.clone(),
            is_owner: s.role == state::SiteRole::Owner,
            contract_key_b58: s.contract_key.map(|ck| ck.encoded_contract_id()),
        })
        .collect();
    let live_prefixes: std::collections::HashSet<String> = sites.keys().cloned().collect();
    drop(sites);
    for removed_prefix in state::REMOVED_PREFIXES.read().iter() {
        // Belt and braces: if a prefix is somehow both live and tombstoned
        // (e.g. add/remove race), never serialize the tombstone — a live
        // site wins. `clear_tombstone` is the primary defense; this
        // prevents a persisted contradiction if it is ever bypassed.
        if live_prefixes.contains(removed_prefix) {
            continue;
        }
        records.push(delta_core::KnownSiteRecord::tombstone(removed_prefix));
    }
    let request = delta_core::DelegateRequest::StoreKnownSites { sites: records };
    send_delegate_request(&request);
}

/// Request the delegate to load known sites.
fn load_known_sites() {
    let request = delta_core::DelegateRequest::GetKnownSites;
    send_delegate_request(&request);
}

/// Ask the delegate to sign a page. The response will be handled by
/// `handle_delegate_response` which sends the UPDATE to the network.
pub fn request_sign_page(
    site_prefix: &str,
    contract_key: ContractKey,
    page_id: PageId,
    title: String,
    content: String,
    updated_at: u64,
    order: u32,
) {
    // Register pending update so the response handler knows where to send it
    PENDING_UPDATES
        .write()
        .insert((site_prefix.to_string(), page_id), contract_key);

    let request = delta_core::DelegateRequest::SignPage {
        page_id,
        title,
        content,
        updated_at,
        order,
        prefix: Some(site_prefix.to_string()),
    };
    send_signing_request(&request);
}

/// Ask the delegate to sign a page deletion.
pub fn request_sign_deletion(
    site_prefix: &str,
    contract_key: ContractKey,
    page_id: PageId,
    deleted_at: u64,
) {
    PENDING_UPDATES
        .write()
        .insert((site_prefix.to_string(), page_id), contract_key);

    let request = delta_core::DelegateRequest::SignPageDeletion {
        page_id,
        deleted_at,
        prefix: Some(site_prefix.to_string()),
    };
    send_signing_request(&request);
}

/// Ask the delegate to sign a config update (e.g. rename).
pub fn request_sign_config(site_prefix: &str, contract_key: ContractKey, new_name: String) {
    *PENDING_CONFIG.write() = Some(contract_key);

    let sites = state::SITES.read();
    let config = if let Some(site) = sites.get(site_prefix) {
        site.state.config.config.clone()
    } else {
        return;
    };
    drop(sites);

    let request = delta_core::DelegateRequest::SignConfig {
        config,
        prefix: Some(site_prefix.to_string()),
    };
    send_signing_request(&request);
    let _ = new_name; // name already set in config
}

/// Ask the delegate for the stored public key (checks if key exists).
fn request_public_key() {
    let request = delta_core::DelegateRequest::GetPublicKey;
    send_delegate_request(&request);
}

/// Compute the current delegate key.
fn current_delegate_key() -> DelegateKey {
    let delegate_code = DelegateCode::from(SITE_DELEGATE_WASM.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    delegate.key().clone()
}

/// Handle a delegate response — route signed objects to the network.
pub fn handle_delegate_response(responding_key: DelegateKey, values: Vec<OutboundDelegateMsg>) {
    let is_legacy = responding_key != current_delegate_key();
    for msg in values {
        if let OutboundDelegateMsg::ApplicationMessage(app_msg) = msg {
            let response: DelegateResponse = match from_reader(app_msg.payload.as_slice()) {
                Ok(r) => r,
                Err(e) => {
                    log(&format!(
                        "Delta: failed to deserialize delegate response: {e}"
                    ));
                    continue;
                }
            };
            match response {
                DelegateResponse::KeyStored => {
                    log("Delta: signing key stored in delegate");
                    // CURRENT_KEY_PREFIXES is updated optimistically in store_signing_key
                }
                DelegateResponse::SigningKey(key_bytes) => {
                    log("Delta: received signing key from delegate");
                    // Store key in current delegate with its prefix (migrates from legacy)
                    if let Ok(key_arr) = <[u8; 32]>::try_from(key_bytes.as_slice()) {
                        let sk = ed25519_dalek::SigningKey::from_bytes(&key_arr);
                        let prefix = delta_core::pubkey_to_prefix(&sk.verifying_key());
                        store_signing_key(&key_arr, Some(&prefix));
                    }
                    // Also handle export if the export modal is showing
                    crate::components::export_key::handle_signing_key_response(key_bytes);
                }
                DelegateResponse::PublicKey(vk) => {
                    let prefix = delta_core::pubkey_to_prefix(&vk);
                    if is_legacy {
                        log(&format!(
                            "Delta: legacy delegate has signing key for site {prefix}"
                        ));
                        // Store the legacy delegate key for routing signing requests
                        let key_bytes: [u8; 32] = responding_key.bytes().try_into().unwrap();
                        let code_hash: [u8; 32] = **responding_key.code_hash();
                        *LEGACY_SIGNING_DELEGATE.write() = Some((key_bytes, code_hash));
                    } else {
                        log(&format!(
                            "Delta: current delegate has key for site {prefix}"
                        ));
                        *HAS_CURRENT_LEGACY_KEY.write() = true;
                    }
                    // Record this prefix as owned (resolves race with KnownSites)
                    OWNER_PREFIXES.with_mut(|prefixes| {
                        if !prefixes.contains(&prefix) {
                            prefixes.push(prefix.clone());
                        }
                    });
                    // Mark the site as owner if it exists already
                    let mut sites = state::SITES.write();
                    if let Some(site) = sites.get_mut(&prefix) {
                        site.role = state::SiteRole::Owner;
                        site.owner_pubkey = vk.to_bytes();
                    }
                }
                DelegateResponse::SignedPage { page_id, page } => {
                    log(&format!("Delta: delegate signed page {page_id}"));
                    // Find the pending update and send to network
                    handle_signed_page(page_id, page);
                }
                DelegateResponse::SignedDeletion(deletion) => {
                    log(&format!(
                        "Delta: delegate signed deletion for page {}",
                        deletion.page_id
                    ));
                    handle_signed_deletion(deletion);
                }
                DelegateResponse::SignedConfig(signed_config) => {
                    log(&format!(
                        "Delta: delegate signed config v{}",
                        signed_config.config.version
                    ));
                    handle_signed_config(signed_config);
                }
                DelegateResponse::SitesStored => {
                    log("Delta: known sites saved to delegate");
                }
                DelegateResponse::KnownSites(records) => {
                    // Split tombstones out first — they populate
                    // REMOVED_PREFIXES and must NOT be restored as sites.
                    let (tombstones, real_records): (Vec<_>, Vec<_>) = records
                        .into_iter()
                        .partition(delta_core::KnownSiteRecord::is_tombstone);

                    // Tombstones from either current or legacy delegate are
                    // merged: a legacy delegate may still hold a removal we
                    // recorded before a delegate upgrade, and the current
                    // delegate holds all removals after this fix ships.
                    //
                    // If a tombstone arrives for a prefix that's currently in
                    // SITES (e.g. added via hash route before known_sites
                    // loaded), remove it too — the user's intent to delete
                    // must not be silently ignored.
                    if !tombstones.is_empty() {
                        log(&format!(
                            "Delta: loaded {} tombstone(s) from delegate{}",
                            tombstones.len(),
                            if is_legacy { " (legacy)" } else { "" }
                        ));
                        state::REMOVED_PREFIXES.with_mut(|removed| {
                            for t in &tombstones {
                                if !removed.contains(&t.prefix) {
                                    removed.push(t.prefix.clone());
                                }
                            }
                        });
                        state::SITES.with_mut(|sites| {
                            for t in &tombstones {
                                sites.remove(&t.prefix);
                            }
                        });
                    }

                    if is_legacy && *CURRENT_SITES_LOADED.read() {
                        // Current delegate is the source of truth. Ignore
                        // legacy real records to respect site removals.
                        log(&format!(
                            "Delta: skipping {} legacy known site(s) (current is authoritative)",
                            real_records.len()
                        ));
                    } else {
                        log(&format!(
                            "Delta: loaded {} known site(s) from delegate{}",
                            real_records.len(),
                            if is_legacy { " (legacy)" } else { "" }
                        ));
                        if !is_legacy {
                            // The current delegate has responded. Flip the
                            // flag whenever it holds ANY state — real
                            // records OR tombstones — because both signal
                            // "this user has initialized the current
                            // delegate, so any absent prefix is a removal,
                            // not a never-seen site." An empty-empty
                            // response means the user is pre-migration;
                            // leave the flag off so legacy migration can
                            // populate the initial state.
                            let has_any = !real_records.is_empty() || !tombstones.is_empty();
                            if has_any {
                                *CURRENT_SITES_LOADED.write() = true;
                            }
                            for r in &real_records {
                                if r.is_owner {
                                    CURRENT_KEY_PREFIXES.with_mut(|prefixes| {
                                        if !prefixes.contains(&r.prefix) {
                                            prefixes.push(r.prefix.clone());
                                        }
                                    });
                                }
                            }
                        }
                        let has_real = !real_records.is_empty();
                        let has_tombstones = !tombstones.is_empty();
                        restore_known_sites(real_records);
                        // If legacy contributed ANY state — real records OR
                        // tombstones — persist to the current delegate so
                        // the merged view survives a refresh. Without this,
                        // a legacy delegate holding ONLY tombstones would
                        // leak those tombstones back out of REMOVED_PREFIXES
                        // on next load and resurrect removed sites.
                        if is_legacy && (has_real || has_tombstones) {
                            save_known_sites();
                            *CURRENT_SITES_LOADED.write() = true;
                        }
                    }

                    // Once the current delegate has responded, it is safe
                    // to query legacy delegates: any legacy KnownSites
                    // response is now either blocked (CURRENT_SITES_LOADED
                    // is set) or merged into a fresh migration path.
                    if !is_legacy && !*LEGACY_MIGRATION_FIRED.read() {
                        *LEGACY_MIGRATION_FIRED.write() = true;
                        fire_legacy_migration();
                    }
                }
                DelegateResponse::SiteStateStored => {
                    log("Delta: site state backed up to delegate");
                }
                DelegateResponse::SiteState {
                    prefix,
                    state_bytes,
                } => {
                    log(&format!(
                        "Delta: restoring site {prefix} from delegate backup"
                    ));
                    handle_restored_site_state(&prefix, &state_bytes);
                }
                DelegateResponse::Error(e) => {
                    log(&format!("Delta: delegate error: {e}"));
                }
            }
        }
    }
}

/// After receiving a signed page from the delegate, update local state and send to network.
fn handle_signed_page(page_id: PageId, page: delta_core::Page) {
    // Find which site/contract this is for
    let pending = PENDING_UPDATES.write().remove(&find_pending_key(page_id));

    // Update local state
    let prefix = state::CURRENT_SITE.read().clone();
    if let Some(prefix) = &prefix {
        let mut sites = state::SITES.write();
        if let Some(site) = sites.get_mut(prefix) {
            site.state.pages.insert(page_id, page.clone());
            if page_id >= site.state.next_page_id {
                site.state.next_page_id = page_id + 1;
            }
        }
    }

    // Send UPDATE to network
    if let Some(contract_key) = pending {
        let mut updates = BTreeMap::new();
        updates.insert(page_id, page);
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates: updates,
            page_deletions: Vec::new(),
        };
        super::operations::update_site(&contract_key, &delta);
    }
}

/// After receiving a signed config, update local state and send to network.
fn handle_signed_config(signed_config: delta_core::SignedConfig) {
    let contract_key = PENDING_CONFIG.write().take();

    // Update local state
    if let Some(prefix) = state::CURRENT_SITE.read().clone() {
        let mut sites = state::SITES.write();
        if let Some(site) = sites.get_mut(&prefix) {
            site.state.config = signed_config.clone();
            site.name = signed_config.config.name.clone();
        }
    }

    // Send UPDATE to network
    if let Some(ck) = contract_key {
        let delta = delta_core::SiteStateDelta {
            config: Some(signed_config),
            page_updates: BTreeMap::new(),
            page_deletions: Vec::new(),
        };
        super::operations::update_site(&ck, &delta);
    }
}

/// After receiving a signed deletion, update local state and send to network.
fn handle_signed_deletion(deletion: delta_core::SignedPageDeletion) {
    let page_id = deletion.page_id;
    log(&format!(
        "Delta: handling signed deletion for page {page_id}"
    ));
    let pending = PENDING_UPDATES.write().remove(&find_pending_key(page_id));

    let prefix = state::CURRENT_SITE.read().clone();
    if let Some(prefix) = &prefix {
        let mut sites = state::SITES.write();
        if let Some(site) = sites.get_mut(prefix) {
            site.state.pages.remove(&page_id);
        }
    }

    if let Some(contract_key) = pending {
        log(&format!(
            "Delta: sending deletion UPDATE to network for page {page_id}"
        ));
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates: BTreeMap::new(),
            page_deletions: vec![deletion],
        };
        super::operations::update_site(&contract_key, &delta);
    } else {
        log(&format!(
            "Delta: no pending contract key for deletion of page {page_id} - not sent to network"
        ));
    }
}

/// Find the pending key for a page_id (searches current site).
fn find_pending_key(page_id: PageId) -> (String, PageId) {
    let prefix = state::CURRENT_SITE.read().clone().unwrap_or_default();
    (prefix, page_id)
}

/// Public wrapper so export_key module can send signing-related delegate requests.
pub fn send_delegate_request_pub(request: &delta_core::DelegateRequest) {
    send_signing_request(request);
}

/// Whether the current delegate has a signing key for any site.
pub fn has_current_key() -> bool {
    !CURRENT_KEY_PREFIXES.read().is_empty() || *HAS_CURRENT_LEGACY_KEY.read()
}

/// Send a request to the current delegate.
fn send_delegate_request(request: &delta_core::DelegateRequest) {
    send_to_delegate_key(request, current_delegate_key());
}

/// Extract the prefix from a signing-related delegate request.
fn request_prefix(request: &delta_core::DelegateRequest) -> Option<&str> {
    match request {
        delta_core::DelegateRequest::SignPage { prefix, .. }
        | delta_core::DelegateRequest::SignPageDeletion { prefix, .. }
        | delta_core::DelegateRequest::SignConfig { prefix, .. } => prefix.as_deref(),
        _ => None,
    }
}

/// Send a signing request. Routes to the current delegate if it has the key
/// for this prefix, otherwise falls back to the legacy delegate.
fn send_signing_request(request: &delta_core::DelegateRequest) {
    let prefix = request_prefix(request);
    let current_has_key = match prefix {
        Some(p) => CURRENT_KEY_PREFIXES.read().contains(&p.to_string()),
        None => *HAS_CURRENT_LEGACY_KEY.read(),
    };

    let key = if current_has_key {
        current_delegate_key()
    } else if let Some((key_bytes, code_hash_bytes)) = *LEGACY_SIGNING_DELEGATE.read() {
        log("Delta: routing signing request through legacy delegate");
        DelegateKey::new(key_bytes, CodeHash::new(code_hash_bytes))
    } else {
        current_delegate_key()
    };
    send_to_delegate_key(request, key);
}

fn send_to_delegate_key(request: &delta_core::DelegateRequest, delegate_key: DelegateKey) {
    #[cfg(target_arch = "wasm32")]
    {
        let mut payload = Vec::new();
        into_writer(request, &mut payload).expect("CBOR serialization");

        let app_msg = ApplicationMessage::new(payload).processed(false);

        let client_request =
            ClientRequest::DelegateOp(StdlibDelegateRequest::ApplicationMessages {
                key: delegate_key,
                params: Parameters::from(Vec::<u8>::new()),
                inbound: vec![InboundDelegateMsg::ApplicationMessage(app_msg)],
            });

        wasm_bindgen_futures::spawn_local(async move {
            let mut api = super::connection::WEB_API.write();
            if let Some(web_api) = api.as_mut() {
                if let Err(e) = web_api.send(client_request).await {
                    log(&format!("Delta: delegate request failed: {e:?}"));
                }
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (request, delegate_key);
    }
}

/// Restore known sites from delegate-persisted records.
/// For each site, creates a placeholder entry and sends GET+SUBSCRIBE.
fn restore_known_sites(records: Vec<delta_core::KnownSiteRecord>) {
    for record in records {
        // Tombstones must be partitioned out before this call — seeing one
        // here means a new caller bypassed the partition in
        // handle_delegate_response and we're about to resurrect a removed
        // site. Fail loudly in debug builds and skip in release.
        debug_assert!(
            !record.is_tombstone(),
            "tombstone reached restore_known_sites — partition is broken"
        );
        if record.is_tombstone() {
            continue;
        }
        let prefix = record.prefix.clone();

        // Don't restore sites the user explicitly removed
        if state::REMOVED_PREFIXES.read().contains(&prefix) {
            continue;
        }

        // Skip if already loaded (e.g. from hash route), but still fix owner role
        if state::SITES.read().contains_key(&prefix) {
            if delta_core::is_site_owned(record.is_owner, &prefix, &OWNER_PREFIXES.read()) {
                let mut sites = state::SITES.write();
                if let Some(site) = sites.get_mut(&prefix) {
                    site.role = state::SiteRole::Owner;
                }
            }
            continue;
        }

        // Check if PublicKey already confirmed ownership (may arrive before KnownSites)
        let role = if delta_core::is_site_owned(record.is_owner, &prefix, &OWNER_PREFIXES.read()) {
            state::SiteRole::Owner
        } else {
            state::SiteRole::Visitor
        };

        let new_contract_key = state::contract_key_from_prefix(&prefix);

        let old_key_b58 = record.contract_key_b58.clone();
        let new_key_b58 = new_contract_key.encoded_contract_id();
        let stored_key_is_stale = old_key_b58.as_ref().is_some_and(|old| *old != new_key_b58);

        if stored_key_is_stale {
            log(&format!(
                "Delta: contract WASM upgrade detected for site {prefix}, migrating state"
            ));
        }

        let site = state::KnownSite {
            name: record.name,
            prefix: prefix.clone(),
            role,
            state: delta_core::SiteState::default(),
            owner_pubkey: [0u8; 32],
            contract_key: Some(new_contract_key),
        };

        state::SITES.with_mut(|sites| {
            sites.insert(prefix.clone(), site);
        });

        // Enter the initial-capture window for this prefix. Until one
        // non-empty GET response arrives, every incoming response is
        // treated as a candidate; the first wins and siblings are
        // dropped via `finalize_prefix_capture`.
        super::operations::mark_prefix_migrating(&prefix);

        // Always GET the current contract key.
        super::operations::get_site(&new_contract_key);

        // If the delegate persisted a stale `contract_key_b58`, probe
        // that specific key in addition to the generic legacy-hash
        // sweep: the user's state most likely lives there.
        if stored_key_is_stale {
            if let Some(old_b58) = old_key_b58.as_deref() {
                super::operations::get_for_migration(old_b58, &prefix);
            }
        }

        // Only fire the generic legacy-hash sweep when the stored
        // contract key is either absent or demonstrably stale. In the
        // steady state where the delegate-persisted
        // `contract_key_b58` matches the current WASM, there is no
        // release-era before the current one whose state could live
        // on the network under this prefix — the site was created
        // under the current contract WASM. Skipping the sweep avoids
        // a startup thundering herd: N sites × M legacy hashes of
        // redundant GETs that will all NotFound.
        if old_key_b58.is_none() || stored_key_is_stale {
            super::operations::fire_legacy_contract_migrations(&prefix, &new_key_b58);
        }
    }

    // Replay any pending hash navigation (from deep link)
    // This runs AFTER known sites are restored, so the site might already be known
    crate::components::replay_pending_hash();

    // Select the first site if none selected (and no pending hash handled it)
    #[cfg(target_arch = "wasm32")]
    if state::CURRENT_SITE.read().is_none() {
        if let Some(prefix) = state::SITES.read().keys().next().cloned() {
            wasm_bindgen_futures::spawn_local(async move {
                state::select_site(&prefix);
            });
        }
    }
}

/// Attempt to migrate data from legacy delegate versions.
/// Sends separate requests (GetPublicKey, GetKnownSites, GetSigningKey) to each
/// legacy delegate. Old delegates that don't support all requests will error for
/// those individually, which is fine -- we take whatever we can get.
fn fire_legacy_migration() {
    #[cfg(target_arch = "wasm32")]
    {
        if LEGACY_DELEGATES.is_empty() {
            return;
        }

        log(&format!(
            "Delta: attempting migration from {} legacy delegate(s)",
            LEGACY_DELEGATES.len()
        ));

        // Send each request type separately so one failure doesn't kill the batch
        let requests = [
            delta_core::DelegateRequest::GetPublicKey,
            delta_core::DelegateRequest::GetKnownSites,
            delta_core::DelegateRequest::GetSigningKey,
        ];

        for (i, (key_bytes, code_hash_bytes)) in LEGACY_DELEGATES.iter().enumerate() {
            for req in &requests {
                let legacy_code_hash = CodeHash::new(*code_hash_bytes);
                let legacy_delegate_key = DelegateKey::new(*key_bytes, legacy_code_hash);

                let mut payload = Vec::new();
                if into_writer(req, &mut payload).is_err() {
                    continue;
                }

                let app_msg = ApplicationMessage::new(payload).processed(false);
                let client_request =
                    ClientRequest::DelegateOp(StdlibDelegateRequest::ApplicationMessages {
                        key: legacy_delegate_key,
                        params: Parameters::from(Vec::<u8>::new()),
                        inbound: vec![InboundDelegateMsg::ApplicationMessage(app_msg)],
                    });

                let idx = i;
                wasm_bindgen_futures::spawn_local(async move {
                    let mut api = super::connection::WEB_API.write();
                    if let Some(web_api) = api.as_mut() {
                        match web_api.send(client_request).await {
                            Ok(_) => log(&format!("Delta: legacy migration request #{idx} sent")),
                            Err(_) => {
                                // Expected if legacy delegate isn't installed on this node
                            }
                        }
                    }
                });
            }
        }
    }
}

/// Back up a site's state to the delegate for resilience against network drops.
pub fn backup_site_state(prefix: &str, site_state: &delta_core::SiteState) {
    let mut state_bytes = Vec::new();
    if into_writer(site_state, &mut state_bytes).is_err() {
        log("Delta: failed to serialize site state for backup");
        return;
    }
    let request = delta_core::DelegateRequest::StoreSiteState {
        prefix: prefix.to_string(),
        state_bytes,
    };
    send_delegate_request(&request);
}

/// Request a site's backed-up state from the delegate.
pub fn request_site_state_backup(prefix: &str) {
    let request = delta_core::DelegateRequest::GetSiteState {
        prefix: prefix.to_string(),
    };
    send_delegate_request(&request);
}

/// Handle a restored site state from delegate backup -- PUT it to the network.
fn handle_restored_site_state(prefix: &str, state_bytes: &[u8]) {
    let site_state: delta_core::SiteState = match from_reader(state_bytes) {
        Ok(s) => s,
        Err(e) => {
            log(&format!(
                "Delta: failed to deserialize backed-up state for {prefix}: {e}"
            ));
            return;
        }
    };

    let mut sites = state::SITES.write();
    if let Some(site) = sites.get_mut(prefix) {
        if site.state == delta_core::SiteState::default() {
            log(&format!(
                "Delta: restoring site {prefix} from backup ({} pages)",
                site_state.pages.len()
            ));
            site.state = site_state.clone();
            site.name = site_state.config.config.name.clone();
            site.owner_pubkey = site_state.owner.to_bytes();
            drop(sites);

            // PUT the backed-up state to the network to restore it
            let params = delta_core::SiteParameters {
                prefix: prefix.to_string(),
            };
            super::operations::put_site(&params, &site_state);
        }
    }
}

fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{msg}");
}
