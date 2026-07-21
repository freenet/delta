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

/// Prefixes for which the CURRENT delegate is confirmed to hold a signing key.
/// Signing ALWAYS routes to the current delegate (see `signing_target`); this
/// tracks whether the key is confirmed there yet, purely for diagnostics (a
/// sign before it is confirmed may fail transiently until the per-prefix key
/// export migration completes).
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

            // stdlib 0.8 removed the world-known `DEFAULT_CIPHER` / `DEFAULT_NONCE`
            // constants; supply a per-install random cipher/nonce instead. See
            // `site_delegate_cipher_material` for the full rationale.
            let (cipher, nonce) = site_delegate_cipher_material();
            let request = ClientRequest::DelegateOp(StdlibDelegateRequest::RegisterDelegate {
                delegate: container,
                cipher,
                nonce,
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

/// localStorage keys for the persisted per-install site-delegate registration
/// cipher/nonce (base58-encoded). See [`site_delegate_cipher_material`].
#[cfg(target_arch = "wasm32")]
const SITE_DELEGATE_CIPHER_KEY: &str = "delta_site_delegate_cipher_v1";
#[cfg(target_arch = "wasm32")]
const SITE_DELEGATE_NONCE_KEY: &str = "delta_site_delegate_nonce_v1";

/// The (cipher, nonce) to register the site delegate with.
///
/// stdlib 0.8 removed the world-known `DelegateRequest::DEFAULT_CIPHER` /
/// `DEFAULT_NONCE` constants Delta previously passed here. They were a PUBLIC
/// 32-byte key + 24-byte nonce baked into stdlib, so every node encrypted its
/// delegate secrets at rest under a key anyone could reproduce. We replace
/// them with a random 32-byte cipher + 24-byte nonce generated once and kept
/// STABLE, so:
///
///   * Stability: `register_delegate()` fires on every (re)connect. Registering
///     the SAME delegate with DIFFERENT material would, on a node that still
///     honors the client-supplied cipher, make it unable to decrypt the previous
///     registration's secrets — bricking a site's stored signing keys / known
///     sites / state backups. A stable value avoids that.
///   * Confidentiality: the at-rest key becomes a per-install secret instead of
///     a world-known constant.
///
/// Where the material lives depends on the browser context:
///
///   * `localStorage` available (normal top-level origin): persisted there, so it
///     is stable ACROSS reloads and restarts, per browser profile.
///   * `localStorage` unavailable — the production gateway iframe runs with an
///     opaque origin, so `window.localStorage` THROWS and both load/persist
///     silently no-op. In that case we fall back to a page-lifetime in-memory
///     value ([`in_memory_cipher_material`]) so the delegate re-registers with
///     identical material on every reconnect within the page's lifetime (it is
///     not stable across a full page RELOAD there).
///
/// NOTE — on a node built against stdlib-0.8 core, this cipher/nonce are IGNORED
/// server-side: the node derives a per-delegate DEK from its own KEK, and
/// decrypts pre-0.8 secrets written under the old world-known cipher via a
/// built-in legacy fallback. So migration of OLD delegate secrets does NOT
/// depend on Delta supplying any particular key, and cross-reload rotation of
/// the client value is harmless on the 0.8 fleet. The client cipher only still
/// matters on a pre-0.8 node that honors the client-supplied value — see the
/// equivalent River analysis (freenet/river#394) for the full assessment.
#[cfg(target_arch = "wasm32")]
fn site_delegate_cipher_material() -> ([u8; 32], [u8; 24]) {
    // 1. Prefer localStorage where available — it survives reloads/restarts.
    if let Some((cipher, nonce)) = load_persisted_cipher_material() {
        return (cipher, nonce);
    }
    // 2. localStorage missing or unreadable (e.g. the sandboxed gateway iframe).
    //    Use a page-stable process-memory value instead of generating a fresh
    //    one every call, so the delegate re-registers with identical material on
    //    every reconnect within the page's lifetime.
    let (cipher, nonce) = in_memory_cipher_material();
    // 3. Write through to localStorage when it IS available (a harmless no-op in
    //    the iframe) so a subsequent reload can recover the same value.
    persist_cipher_material(&cipher, &nonce);
    (cipher, nonce)
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// Process-memory fallback for the site-delegate cipher/nonce. `None` until
    /// first use; see [`in_memory_cipher_material`]. wasm is single-threaded, so
    /// this thread-local is effectively process-global for the page lifetime.
    static IN_MEMORY_CIPHER_MATERIAL: std::cell::RefCell<Option<([u8; 32], [u8; 24])>> =
        const { std::cell::RefCell::new(None) };
}

/// Page-stable process-memory cipher/nonce, used when `localStorage` is
/// unavailable (the sandboxed gateway iframe). Generated once on first use and
/// returned unchanged thereafter.
#[cfg(target_arch = "wasm32")]
fn in_memory_cipher_material() -> ([u8; 32], [u8; 24]) {
    IN_MEMORY_CIPHER_MATERIAL.with(|cell| {
        *cell
            .borrow_mut()
            .get_or_insert_with(generate_cipher_material)
    })
}

/// Generate a fresh random (cipher, nonce). `OsRng` routes through the UI's
/// `getrandom` backend (`window.crypto.getRandomValues` on wasm; the OS entropy
/// pool natively), so this is a CSPRNG on every target.
#[cfg(any(target_arch = "wasm32", test))]
fn generate_cipher_material() -> ([u8; 32], [u8; 24]) {
    use rand::RngCore;
    let mut cipher = [0u8; 32];
    let mut nonce = [0u8; 24];
    let mut rng = rand::rngs::OsRng;
    rng.fill_bytes(&mut cipher);
    rng.fill_bytes(&mut nonce);
    (cipher, nonce)
}

#[cfg(target_arch = "wasm32")]
fn load_persisted_cipher_material() -> Option<([u8; 32], [u8; 24])> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    let cipher_b58 = storage.get_item(SITE_DELEGATE_CIPHER_KEY).ok()??;
    let nonce_b58 = storage.get_item(SITE_DELEGATE_NONCE_KEY).ok()??;
    let cipher: [u8; 32] = bs58::decode(&cipher_b58).into_vec().ok()?.try_into().ok()?;
    let nonce: [u8; 24] = bs58::decode(&nonce_b58).into_vec().ok()?.try_into().ok()?;
    Some((cipher, nonce))
}

#[cfg(target_arch = "wasm32")]
fn persist_cipher_material(cipher: &[u8; 32], nonce: &[u8; 24]) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(
                SITE_DELEGATE_CIPHER_KEY,
                &bs58::encode(cipher).into_string(),
            );
            let _ = storage.set_item(SITE_DELEGATE_NONCE_KEY, &bs58::encode(nonce).into_string());
        }
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

/// Whether `responding_key` is the NEWEST legacy delegate — the last entry in
/// `legacy_delegates.toml`, i.e. the delegate immediately preceding the
/// current one. Only this legacy delegate's KnownSites real records are
/// unioned into the current view once the current delegate is authoritative
/// (see the KnownSites handler for the generation-aware rationale).
fn is_newest_legacy_delegate(responding_key: &DelegateKey) -> bool {
    let Some((newest_key, newest_hash)) = LEGACY_DELEGATES.last() else {
        return false;
    };
    let key_matches = responding_key.bytes() == newest_key.as_slice();
    let hash_matches = **responding_key.code_hash() == *newest_hash;
    key_matches && hash_matches
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
                        // A legacy delegate holds a (single-slot) signing key.
                        // We do NOT record it for routing: signing NEVER goes
                        // to a legacy delegate (a legacy delegate lacking THIS
                        // prefix's per-prefix key would sign this site's content
                        // with another site's single-slot key -> cross-site
                        // corruption). The per-prefix key export migration
                        // copies the key onto the current delegate instead.
                        log(&format!(
                            "Delta: legacy delegate reports a signing key for site {prefix} \
                             (not used for routing; key is migrated to the current delegate)"
                        ));
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

                    // Tombstone application rules:
                    //
                    // 1. Once the current delegate has responded
                    //    (CURRENT_SITES_LOADED), it is authoritative for
                    //    tombstones as well as real records — legacy
                    //    tombstones must be ignored, otherwise a legacy
                    //    delegate holding a stale removal record can
                    //    resurrect-then-re-delete a site that the user has
                    //    since explicitly re-visited. See the
                    //    "delete-then-revisit vanishes" bug.
                    //
                    // 2. A tombstone whose prefix is currently present in
                    //    SITES is ALWAYS ignored. If the user has an active
                    //    site for that prefix (e.g. they just called
                    //    visit_site / create_new_site / import_site_key),
                    //    their live intent beats any stale removal record,
                    //    even from the current delegate. `clear_tombstone`
                    //    is the primary defense; this is the guardrail for
                    //    ordering races between save_known_sites and a
                    //    load_known_sites response already in flight.
                    let tombstones_to_apply: Vec<_> = {
                        let live_sites = state::SITES.read();
                        let live_prefixes: std::collections::HashSet<&str> =
                            live_sites.keys().map(String::as_str).collect();
                        filter_applicable_tombstones(
                            &tombstones,
                            is_legacy,
                            *CURRENT_SITES_LOADED.read(),
                            &live_prefixes,
                        )
                    };
                    let skipped = tombstones.len() - tombstones_to_apply.len();
                    if !tombstones.is_empty() {
                        log(&format!(
                            "Delta: loaded {} tombstone(s) from delegate{} ({} applied, {} skipped)",
                            tombstones.len(),
                            if is_legacy { " (legacy)" } else { "" },
                            tombstones_to_apply.len(),
                            skipped
                        ));
                        state::REMOVED_PREFIXES.with_mut(|removed| {
                            for t in &tombstones_to_apply {
                                if !removed.contains(&t.prefix) {
                                    removed.push(t.prefix.clone());
                                }
                            }
                        });
                        state::SITES.with_mut(|sites| {
                            for t in &tombstones_to_apply {
                                sites.remove(&t.prefix);
                            }
                        });
                    }

                    // Generation-aware reconciliation of legacy real records.
                    //
                    // Once the current delegate is authoritative
                    // (CURRENT_SITES_LOADED), we UNION real records only from
                    // the NEWEST legacy delegate (the one immediately preceding
                    // current), and skip OLDER ones. Rationale:
                    //   * The newest legacy delegate reflects the user's site
                    //     list as of just before the current delegate's
                    //     upgrade, so it carries a genuinely-new site that was
                    //     never migrated forward (the real 0.6->0.8 case). Its
                    //     records are still filtered by REMOVED_PREFIXES and
                    //     already-live in `restore_known_sites`, so a site the
                    //     user removed under the CURRENT delegate stays removed.
                    //   * It is post-tombstone (the convention has existed since
                    //     V3), so a site removed while it was current is a
                    //     TOMBSTONE there, never a real record — unioning it
                    //     cannot resurrect a removed site.
                    //   * OLDER legacy delegates can hold a FROZEN real record
                    //     for a site removed later (a pre-tombstone removal
                    //     deleted the record only from the delegate current at
                    //     removal time), so unioning them WOULD resurrect a
                    //     removed site. We must not.
                    let skip_older_legacy = is_legacy
                        && *CURRENT_SITES_LOADED.read()
                        && !is_newest_legacy_delegate(&responding_key);
                    if skip_older_legacy {
                        log(&format!(
                            "Delta: skipping {} known site(s) from an older legacy delegate (current authoritative)",
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
                        }
                        let has_real = !real_records.is_empty();
                        let has_tombstones = !tombstones.is_empty();
                        // Capture prefixes before restore_known_sites consumes records.
                        // Used below to fetch delegate-backed-up state from legacy delegates.
                        let legacy_prefixes: Vec<String> = if is_legacy {
                            real_records.iter().map(|r| r.prefix.clone()).collect()
                        } else {
                            Vec::new()
                        };
                        restore_known_sites(real_records);
                        // If legacy contributed ANY state — real records OR
                        // tombstones — persist the merged view to the current
                        // delegate so it survives a refresh. Without this, a
                        // legacy delegate holding ONLY tombstones would leak
                        // those tombstones back out of REMOVED_PREFIXES on
                        // next load and resurrect removed sites.
                        if is_legacy && (has_real || has_tombstones) {
                            save_known_sites();
                            *CURRENT_SITES_LOADED.write() = true;
                            // Fetch backed-up site state from this legacy
                            // delegate. If the network GETs all fail (state
                            // GC'd, node offline, etc.), the delegate backup
                            // is the only remaining copy of the user's data.
                            // handle_restored_site_state reconciles via a
                            // tombstone-aware merge, so a newer network GET
                            // always dominates a stale backup regardless of
                            // arrival order.
                            for prefix in &legacy_prefixes {
                                let req = delta_core::DelegateRequest::GetSiteState {
                                    prefix: prefix.clone(),
                                };
                                send_to_delegate_key(&req, responding_key.clone());
                            }
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
///
/// Routing derives the owning site from the page signature itself: every
/// `Page::verify` checks against an owner pubkey, and only the owner who
/// actually signed it will verify. This is safe because nothing broadcasts
/// mis-keyed objects — every signing request goes to a single delegate that
/// holds THIS site's key (current, or the confirmed legacy single-slot
/// delegate), so a signed page can only verify against its true owner. It is
/// robust to multiple in-flight requests, site switches, and out-of-order
/// responses without a delegate WASM change. (See AGENTS.md
/// "Delegate-response routing MUST use signature verification".)
fn handle_signed_page(page_id: PageId, page: delta_core::Page) {
    let Some((prefix, contract_key)) = find_owner_for_signed_page(&page, page_id) else {
        log(&format!(
            "Delta: signed page {page_id} doesn't verify against any known owner — dropping"
        ));
        return;
    };

    // Don't resurrect a page the user just tombstoned. If the user
    // deletes a page after the delegate started signing it, the
    // signed response can race with the deletion's tombstone; without
    // this guard the local state would silently re-add the page and
    // a fresh page-UPDATE would compete with the deletion-UPDATE on
    // the network. (#17 skeptical review)
    {
        let sites = state::SITES.read();
        if sites
            .get(&prefix)
            .map(|s| s.state.deleted_pages.contains_key(&page_id))
            .unwrap_or(false)
        {
            log(&format!(
                "Delta: signed page {page_id} was tombstoned before delegate responded — dropping"
            ));
            return;
        }
    }

    {
        let mut sites = state::SITES.write();
        if let Some(site) = sites.get_mut(&prefix) {
            site.state.pages.insert(page_id, page.clone());
            if page_id >= site.state.next_page_id {
                site.state.next_page_id = page_id + 1;
            }
        }
    }

    let mut updates = BTreeMap::new();
    updates.insert(page_id, page);
    let delta = delta_core::SiteStateDelta {
        config: None,
        page_updates: updates,
        page_deletions: Vec::new(),
    };
    super::operations::update_site(&contract_key, &delta);

    // Best-effort cleanup of the correlation map. Not load-bearing for routing
    // (which is verification-based) but keeps the map bounded — the #17
    // concurrent-same-page case is handled by verification, not by map state.
    PENDING_UPDATES.write().remove(&(prefix, page_id));
}

/// After receiving a signed config, update local state and send to network.
/// See `handle_signed_page` for why routing is via signature verification.
fn handle_signed_config(signed_config: delta_core::SignedConfig) {
    let Some((prefix, contract_key)) = find_owner_for_signed_config(&signed_config) else {
        log("Delta: signed config doesn't verify against any known owner — dropping");
        return;
    };

    {
        let mut sites = state::SITES.write();
        if let Some(site) = sites.get_mut(&prefix) {
            site.state.config = signed_config.clone();
            site.name = signed_config.config.name.clone();
        }
    }

    let delta = delta_core::SiteStateDelta {
        config: Some(signed_config),
        page_updates: BTreeMap::new(),
        page_deletions: Vec::new(),
    };
    super::operations::update_site(&contract_key, &delta);

    // Best-effort cleanup of the correlation slot.
    PENDING_CONFIG.write().take();
}

/// Find the (prefix, contract_key) for a signed config by checking
/// its signature against every known owner.
fn find_owner_for_signed_config(
    signed: &delta_core::SignedConfig,
) -> Option<(String, ContractKey)> {
    let sites = state::SITES.read();
    sites.iter().find_map(|(prefix, site)| {
        if signed.verify(&site.state.owner).is_ok() {
            site.contract_key.map(|ck| (prefix.clone(), ck))
        } else {
            None
        }
    })
}

/// After receiving a signed deletion, update local state and send to network.
/// See `handle_signed_page` for why routing is via signature verification.
fn handle_signed_deletion(deletion: delta_core::SignedPageDeletion) {
    let page_id = deletion.page_id;
    log(&format!(
        "Delta: handling signed deletion for page {page_id}"
    ));

    let Some((prefix, contract_key)) = find_owner_for_signed_deletion(&deletion) else {
        log(&format!(
            "Delta: signed deletion for {page_id} doesn't verify against any known owner — dropping"
        ));
        return;
    };

    {
        let mut sites = state::SITES.write();
        if let Some(site) = sites.get_mut(&prefix) {
            site.state.pages.remove(&page_id);
        }
    }

    log(&format!(
        "Delta: sending deletion UPDATE to network for page {page_id}"
    ));
    let delta = delta_core::SiteStateDelta {
        config: None,
        page_updates: BTreeMap::new(),
        page_deletions: vec![deletion],
    };
    super::operations::update_site(&contract_key, &delta);

    PENDING_UPDATES.write().remove(&(prefix, page_id));
}

/// Find the (prefix, contract_key) for a signed page by checking its
/// signature against every known owner. The owner who signed it will
/// be the only one whose `Page::verify` succeeds.
fn find_owner_for_signed_page(
    page: &delta_core::Page,
    page_id: PageId,
) -> Option<(String, ContractKey)> {
    let sites = state::SITES.read();
    sites.iter().find_map(|(prefix, site)| {
        if page.verify(page_id, &site.state.owner).is_ok() {
            site.contract_key.map(|ck| (prefix.clone(), ck))
        } else {
            None
        }
    })
}

/// Find the (prefix, contract_key) for a signed deletion. Same
/// principle as `find_owner_for_signed_page` — only the owner who
/// signed the deletion will verify it.
fn find_owner_for_signed_deletion(
    deletion: &delta_core::SignedPageDeletion,
) -> Option<(String, ContractKey)> {
    let sites = state::SITES.read();
    sites.iter().find_map(|(prefix, site)| {
        if deletion.verify(&site.state.owner).is_ok() {
            site.contract_key.map(|ck| (prefix.clone(), ck))
        } else {
            None
        }
    })
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

/// Extract the prefix from a signing-related delegate request, so
/// `send_signing_request` can report whether the current delegate has
/// confirmed this site's key yet (diagnostic only; routing is unconditional).
fn request_prefix(request: &delta_core::DelegateRequest) -> Option<&str> {
    match request {
        delta_core::DelegateRequest::SignPage { prefix, .. }
        | delta_core::DelegateRequest::SignPageDeletion { prefix, .. }
        | delta_core::DelegateRequest::SignConfig { prefix, .. } => prefix.as_deref(),
        delta_core::DelegateRequest::GetSigningKeyForPrefix { prefix } => Some(prefix.as_str()),
        _ => None,
    }
}

/// The delegate a signing request is routed to. ALWAYS the current delegate,
/// regardless of `current_has_key` — signing is NEVER routed to a legacy
/// delegate.
///
/// A legacy delegate that lacks THIS prefix's per-prefix key falls back to its
/// legacy single slot (a DIFFERENT site's key) and would sign this site's
/// content with the wrong key, which then verifies against — and corrupts — an
/// unrelated site (cross-site mis-sign). The per-prefix key EXPORT migration
/// (`migrate_per_prefix_signing_key`) copies each owned prefix's key onto the
/// current delegate, so signing on the current delegate is the correct path.
/// If the key isn't there yet (the brief post-upgrade window before the export
/// migration completes, or the deferred V6/V7 cohort — see freenet/delta#35),
/// the current delegate returns a clean "no signing key stored" error and the
/// sign fails transiently rather than corrupting another site; it self-resolves
/// once the export migration confirms the key (runs at startup and re-probes on
/// each KnownSites response). This is exactly main's safe "fail, don't corrupt"
/// behavior. `current_has_key` is accepted so the invariant "even a
/// not-yet-migrated site routes to current, never legacy" is unit-testable.
fn signing_target(_current_has_key: bool) -> DelegateKey {
    current_delegate_key()
}

/// Send a signing request. ALWAYS to the current delegate — never a legacy
/// delegate (see `signing_target` for the anti-cross-site-corruption rationale).
fn send_signing_request(request: &delta_core::DelegateRequest) {
    let prefix = request_prefix(request);
    let current_has_key = match prefix {
        Some(p) => CURRENT_KEY_PREFIXES.read().contains(&p.to_string()),
        None => *HAS_CURRENT_LEGACY_KEY.read(),
    };
    if !current_has_key {
        // Not-yet-migrated (post-upgrade window) or the deferred V6/V7 cohort:
        // the current delegate may not hold the key yet, so this sign may fail
        // transiently and self-resolve once the export migration confirms it.
        // We still route to the CURRENT delegate — never to a legacy delegate.
        log(
            "Delta: signing on current delegate before its key is confirmed; \
             may fail transiently until per-prefix key migration completes",
        );
    }
    send_to_delegate_key(request, signing_target(current_has_key));
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

        // Bug 2 fix: for owned sites, rescue the per-site signing key from
        // legacy delegates. Runs even if the site is already loaded (the
        // site can be restored while its key is still stranded in a legacy
        // delegate after a delegate WASM upgrade). Deduplicated per session.
        if delta_core::is_site_owned(record.is_owner, &prefix, &OWNER_PREFIXES.read()) {
            migrate_per_prefix_signing_key(&prefix);
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
        let is_owner = delta_core::is_site_owned(record.is_owner, &prefix, &OWNER_PREFIXES.read());
        let role = if is_owner {
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

        // Enter the initial-capture window for this prefix. Incoming state
        // responses are reconciled via a tombstone-aware merge
        // (`handle_site_state` / `reconcile_into`), so every candidate
        // generation contributes and none can clobber newer data or resurrect
        // a deletion — arrival order no longer matters.
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

        // Fire the generic legacy-hash sweep when the stored contract key
        // is absent or demonstrably stale (state could live under any past
        // generation). Otherwise, for OWNED sites only, fire a single
        // self-heal probe of the NEWEST legacy generation.
        //
        // The self-heal probe is Bug 1's fix for ALREADY-corrupted users: a
        // prior broken migration can leave the real current content stranded
        // under the immediately-preceding contract generation while the
        // delegate-stored key already points at the current WASM (so the
        // stale-key sweep never fires and the current key holds an older
        // generation). Re-probing the newest legacy generation, plus the
        // tombstone-aware merge in `handle_site_state`, lets them recover on
        // reload. Bounds (SHOULD-FIX write-amplification):
        //   * OWNED-only: content corruption is an owner's edit/delete history;
        //     a visitor just reads the current key, so re-probing for them is
        //     pointless read-amplification.
        //   * The merge never PUTs a dominated generation forward (an already-
        //     fresh current key yields no change -> no write), so the residual
        //     cost is a single extra GET per owned site per load, not the N×M
        //     sweep and not a write.
        if old_key_b58.is_none() || stored_key_is_stale {
            super::operations::fire_legacy_contract_migrations(&prefix, &new_key_b58);
        } else if is_owner {
            super::operations::fire_newest_legacy_contract_migration(&prefix, &new_key_b58);
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

/// The delegate request that rescues an owned site's per-SITE signing key
/// from a legacy delegate. Pure so the "migration actually asks for the
/// per-prefix key" invariant is unit-testable.
///
/// Bug 2 was that the legacy-migration path only ever sent prefix-BLIND
/// `GetPublicKey` / `GetSigningKey`, which read the legacy single-key slot
/// (`delta:signing_key`) and never a per-prefix key
/// (`delta:signing_key:{prefix}`). Sites created under delegate V6+
/// (~2026-04-09+) store their key per-prefix only, so their key was never
/// migrated after a delegate WASM upgrade and `SignPage` failed with "no
/// signing key stored".
fn per_prefix_key_migration_request(prefix: &str) -> delta_core::DelegateRequest {
    delta_core::DelegateRequest::GetSigningKeyForPrefix {
        prefix: prefix.to_string(),
    }
}

/// Rescue / confirm an owned site's per-site signing key (Bug 2).
///
/// Sends `GetSigningKeyForPrefix{prefix}` to the CURRENT delegate AND to every
/// legacy delegate:
///
///   * CURRENT delegate — if it already holds the key (a normal user, or a
///     prior session that migrated it), the `SigningKey` response re-stores it
///     and marks the prefix confirmed in `CURRENT_KEY_PREFIXES`. This is the
///     RELIABLE "current has the key" signal that replaced the old, unreliable
///     "populate from `is_owner`" optimism.
///   * LEGACY delegates — a V8+ delegate holding the key returns it so we
///     migrate it forward. V6/V7 delegates predate `GetSigningKeyForPrefix`
///     and can't answer it, so a key stranded ONLY in a V6/V7 delegate is not
///     recovered here; that narrow cohort is deferred to freenet/delta#35 (a
///     properly-designed, no-broadcast recovery). We never route signing to a
///     legacy delegate — see `signing_target`.
///
/// Guarded on `CURRENT_KEY_PREFIXES` rather than a fire-once flag: we keep
/// probing across successive `KnownSites` responses until the key is confirmed
/// in the current delegate, so a transient first-probe failure self-corrects
/// (SHOULD-FIX: no mark-before-probe dead-end). Once confirmed we stop.
fn migrate_per_prefix_signing_key(prefix: &str) {
    if CURRENT_KEY_PREFIXES.read().contains(&prefix.to_string()) {
        return;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let request = per_prefix_key_migration_request(prefix);
        // Probe the current delegate (confirmation).
        send_delegate_request(&request);
        // Probe every legacy delegate (V8+ recovery).
        for (key_bytes, code_hash_bytes) in LEGACY_DELEGATES.iter() {
            let legacy_delegate_key = DelegateKey::new(*key_bytes, CodeHash::new(*code_hash_bytes));
            send_to_delegate_key(&request, legacy_delegate_key);
        }
        log(&format!(
            "Delta: probing current + {} legacy delegate(s) for the per-site signing key of {prefix}",
            LEGACY_DELEGATES.len()
        ));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = per_prefix_key_migration_request(prefix);
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
    // Try the current delegate first.
    send_delegate_request(&request);
    // Also try every legacy delegate -- the backup may be stranded under
    // an old delegate key if the user upgraded without the state being
    // migrated to the current delegate yet.
    #[cfg(target_arch = "wasm32")]
    {
        for (key_bytes, code_hash_bytes) in LEGACY_DELEGATES.iter() {
            let legacy_code_hash = CodeHash::new(*code_hash_bytes);
            let legacy_delegate_key = DelegateKey::new(*key_bytes, legacy_code_hash);
            send_to_delegate_key(&request, legacy_delegate_key);
        }
    }
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
        // Tombstone-aware merge (see `reconcile_into`): the backup contributes
        // only genuinely-new data and can never resurrect a page the live
        // state has since deleted, nor clobber a newer generation we already
        // captured from the network — regardless of arrival order.
        if super::operations::reconcile_into(&mut site.state, &site_state) {
            let merged = site.state.clone();
            log(&format!(
                "Delta: reconciled site {prefix} from backup ({} pages)",
                merged.pages.len()
            ));
            site.name = merged.config.config.name.clone();
            site.owner_pubkey = merged.owner.to_bytes();
            drop(sites);

            // Persist the merged state to the current delegate so it
            // survives future delegate WASM upgrades.
            backup_site_state(prefix, &merged);

            // PUT the reconciled state to the network to restore it.
            let params = delta_core::SiteParameters {
                prefix: prefix.to_string(),
            };
            super::operations::put_site(&params, &merged);

            // If a migration sweep for this prefix is still in flight, this
            // backup PUT already put `merged` on the network current key —
            // update its baseline so the sweep's finalize doesn't fire a second,
            // identical PUT of the same state.
            super::operations::note_forward_put_baseline(prefix, &merged);
        }
    }
}

fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{msg}");
}

/// Decide which tombstones from a KnownSites response should actually be
/// applied to local state, given whether the response came from a legacy
/// delegate, whether the current delegate has already been loaded, and
/// which site prefixes are currently live in `SITES`.
///
/// Two rules:
///
/// 1. Once the current delegate has spoken (`current_sites_loaded`), legacy
///    tombstones are dropped. The current delegate is authoritative for the
///    removal set; a stale legacy tombstone must not override it.
///
/// 2. A tombstone whose prefix is currently live (explicitly re-added by
///    the user via `visit_site` / `create_new_site` / `import_site_key`) is
///    always dropped, regardless of source. Live intent beats a stale
///    removal record.
fn filter_applicable_tombstones(
    tombstones: &[delta_core::KnownSiteRecord],
    is_legacy: bool,
    current_sites_loaded: bool,
    live_prefixes: &std::collections::HashSet<&str>,
) -> Vec<delta_core::KnownSiteRecord> {
    tombstones
        .iter()
        .filter(|t| {
            if is_legacy && current_sites_loaded {
                return false;
            }
            if live_prefixes.contains(t.prefix.as_str()) {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn tomb(prefix: &str) -> delta_core::KnownSiteRecord {
        delta_core::KnownSiteRecord::tombstone(prefix)
    }

    /// The old stdlib `DEFAULT_CIPHER`/`DEFAULT_NONCE` were world-known
    /// constants. After the 0.8 bump we generate a per-install random value;
    /// pin that it is non-trivial (not all-zero) and actually random (two
    /// generations differ) so a regression can't silently re-introduce a
    /// world-known at-rest key.
    #[test]
    fn generate_cipher_material_is_random_and_nonzero() {
        let (c1, n1) = generate_cipher_material();
        let (c2, n2) = generate_cipher_material();
        assert_ne!(c1, [0u8; 32], "cipher must not be all-zero");
        assert_ne!(n1, [0u8; 24], "nonce must not be all-zero");
        assert_ne!(c1, c2, "two generations must differ (CSPRNG)");
        assert_ne!(n1, n2, "two generations must differ (CSPRNG)");
    }

    #[test]
    fn signing_never_routes_to_a_legacy_delegate() {
        // BLOCKER (review): a SignPage for a NOT-YET-MIGRATED owned site must
        // never route to a legacy delegate. A legacy delegate lacking this
        // prefix's per-prefix key would fall back to its legacy single slot
        // (another site's key) and sign this site's content with the wrong key
        // -> cross-site mis-sign. Signing always targets the CURRENT delegate;
        // if it lacks the key it fails safely instead of corrupting a site.
        let current = current_delegate_key();

        // Load-bearing case: current does NOT have the key yet (not migrated).
        assert_eq!(
            signing_target(false),
            current,
            "a not-yet-migrated site must still route to the current delegate"
        );
        // Steady state: current has the key.
        assert_eq!(signing_target(true), current);

        // And in NEITHER case does it route to any legacy delegate. If a future
        // change re-adds "route to legacy when the current key is missing",
        // signing_target(false) would return a legacy key and this fails.
        for (key_bytes, code_hash_bytes) in LEGACY_DELEGATES.iter() {
            let legacy = DelegateKey::new(*key_bytes, CodeHash::new(*code_hash_bytes));
            assert_ne!(
                signing_target(false),
                legacy,
                "signing must never route to a legacy delegate"
            );
            assert_ne!(signing_target(true), legacy);
        }
    }

    #[test]
    fn newest_legacy_delegate_is_the_last_toml_entry() {
        // Only the newest legacy delegate's KnownSites real records are
        // unioned once the current delegate is authoritative. Misidentifying
        // it would either resurrect old removals (unioning too many) or drop a
        // genuine legacy-only migration (unioning too few).
        assert!(
            !LEGACY_DELEGATES.is_empty(),
            "legacy delegates table must be populated"
        );
        let (newest_key, newest_hash) = LEGACY_DELEGATES.last().unwrap();
        let newest = DelegateKey::new(*newest_key, CodeHash::new(*newest_hash));
        assert!(is_newest_legacy_delegate(&newest));

        if LEGACY_DELEGATES.len() > 1 {
            let (old_key, old_hash) = LEGACY_DELEGATES.first().unwrap();
            let oldest = DelegateKey::new(*old_key, CodeHash::new(*old_hash));
            assert!(
                !is_newest_legacy_delegate(&oldest),
                "an older legacy delegate must not be treated as newest"
            );
        }
        // The current delegate is not a legacy delegate at all.
        assert!(!is_newest_legacy_delegate(&current_delegate_key()));
    }

    #[test]
    fn per_prefix_key_migration_asks_for_the_prefix_specific_key() {
        // Bug 2 regression guard. The migration MUST ask each legacy
        // delegate for the per-PREFIX signing key. The pre-fix path only
        // ever sent prefix-blind GetPublicKey / GetSigningKey, which read
        // `delta:signing_key` (the legacy single-key slot) and never
        // `delta:signing_key:{prefix}` — so keys for sites created under
        // delegate V6+ were never migrated and edits failed with "no
        // signing key stored". If this ever regresses to a prefix-blind
        // request, per-prefix keys silently stop migrating again.
        let req = per_prefix_key_migration_request("abcdef1234");
        match req {
            delta_core::DelegateRequest::GetSigningKeyForPrefix { prefix } => {
                assert_eq!(prefix, "abcdef1234");
            }
            other => {
                panic!("per-prefix key migration must send GetSigningKeyForPrefix, got {other:?}")
            }
        }
    }

    #[test]
    fn applies_current_delegate_tombstone_when_not_live() {
        let tombstones = vec![tomb("abc")];
        let live: HashSet<&str> = HashSet::new();
        let result = filter_applicable_tombstones(&tombstones, false, false, &live);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].prefix, "abc");
    }

    #[test]
    fn skips_tombstone_for_live_site_even_from_current_delegate() {
        // User just re-visited "abc"; a stale tombstone for "abc" from the
        // current delegate must not clobber the live entry.
        let tombstones = vec![tomb("abc")];
        let live: HashSet<&str> = ["abc"].into_iter().collect();
        let result = filter_applicable_tombstones(&tombstones, false, false, &live);
        assert!(result.is_empty());
    }

    #[test]
    fn skips_legacy_tombstone_when_current_is_authoritative() {
        // Primary fix for "delete then re-visit vanishes": legacy delegate
        // still holds the old removal record, but the current delegate has
        // already responded without it.
        let tombstones = vec![tomb("abc")];
        let live: HashSet<&str> = HashSet::new();
        let result = filter_applicable_tombstones(&tombstones, true, true, &live);
        assert!(result.is_empty());
    }

    #[test]
    fn applies_legacy_tombstone_before_current_loaded() {
        // Pre-migration: current delegate hasn't responded yet, so legacy
        // tombstones still seed REMOVED_PREFIXES.
        let tombstones = vec![tomb("abc")];
        let live: HashSet<&str> = HashSet::new();
        let result = filter_applicable_tombstones(&tombstones, true, false, &live);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn skips_legacy_tombstone_when_site_is_live_even_before_current_loads() {
        // Even if current hasn't loaded, if the user explicitly re-added
        // a site (e.g. via hash-route visit before known_sites loaded),
        // a legacy tombstone must not yank it.
        let tombstones = vec![tomb("abc"), tomb("def")];
        let live: HashSet<&str> = ["abc"].into_iter().collect();
        let result = filter_applicable_tombstones(&tombstones, true, false, &live);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].prefix, "def");
    }

    #[test]
    fn empty_tombstones_yields_empty_result() {
        // Sanity: the function must handle an empty batch cleanly for
        // every flag combination, otherwise a future refactor could
        // regress the iterator path.
        let live_empty: HashSet<&str> = HashSet::new();
        let live_some: HashSet<&str> = ["abc"].into_iter().collect();
        for &is_legacy in &[false, true] {
            for &current_loaded in &[false, true] {
                for live in [&live_empty, &live_some] {
                    assert!(
                        filter_applicable_tombstones(&[], is_legacy, current_loaded, live)
                            .is_empty()
                    );
                }
            }
        }
    }

    #[test]
    fn applies_current_delegate_tombstone_after_current_loaded() {
        // Production steady-state case: the current delegate has
        // responded (CURRENT_SITES_LOADED=true), no live site for the
        // prefix, current delegate sends a tombstone. This must still
        // apply — the CURRENT_SITES_LOADED gate is only for LEGACY
        // tombstones, not current ones.
        let tombstones = vec![tomb("abc")];
        let live: HashSet<&str> = HashSet::new();
        let result = filter_applicable_tombstones(&tombstones, false, true, &live);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].prefix, "abc");
    }

    #[test]
    fn mixed_batch_partitions_correctly() {
        let tombstones = vec![tomb("live"), tomb("gone"), tomb("also-gone")];
        let live: HashSet<&str> = ["live"].into_iter().collect();
        let result = filter_applicable_tombstones(&tombstones, false, true, &live);
        let prefixes: Vec<&str> = result.iter().map(|t| t.prefix.as_str()).collect();
        assert_eq!(prefixes, vec!["gone", "also-gone"]);
    }
}
