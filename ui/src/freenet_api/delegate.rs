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
/// deleted sites. Set at most once per sweep dispatch, and cleared by a dropped
/// connection so the reconnect re-probes (see
/// [`reset_legacy_migration_for_reconnect`]).
///
/// Note it stays `false` forever when `LEGACY_DELEGATES` is empty, because
/// `fire_legacy_migration` returns before latching. Every reader treats "not
/// fired" as "a sweep may still be needed", which is harmless when there is
/// nothing to sweep — but do not start reading this flag as "startup is
/// complete".
static LEGACY_MIGRATION_FIRED: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Whether the `freenet-migrate` secret carry-forward has been started this
/// page load.
///
/// This latch is load-bearing, not tidiness. `start_delegate_secret_migration`
/// is called from the current delegate's `KnownSites` arm, and the migration's
/// own read-merge-write sends `GetKnownSites` to that same delegate — whose
/// reply falls through into that arm (deliberately, so the shipped sweep still
/// sees it). Without a latch that is a self-retrigger loop: each run spawns
/// another run, one per data-bearing predecessor.
///
/// It does not self-limit in the environment that matters. In the production
/// gateway iframe `localStorage` is unavailable and each run builds a fresh
/// `BrowserMarkers::default()`, so no marker ever reports `AlreadyMigrated`
/// and the recursion never terminates — it just keeps multiple runs concurrent
/// in the shared reply registry, which is exactly where same-kind reply
/// correlation gets dangerous. In dev, working `localStorage` halts it after
/// about two rounds, which is why testing by hand does not reveal it.
static SECRET_MIGRATION_FIRED: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Claim the once-per-page-load right to run the secret migration.
///
/// Returns `true` for the first caller and `false` for every later one. Split
/// out from [`start_delegate_secret_migration`] so the latch can be tested on
/// the host: the starter itself is `wasm32`-only, and an untested latch is how
/// the loop above shipped in the first place.
fn claim_secret_migration_slot() -> bool {
    !SECRET_MIGRATION_FIRED.with_mut(|fired| std::mem::replace(fired, true))
}

/// Whether the CURRENT delegate's `KnownSites` reply has arrived.
///
/// Distinct from [`CURRENT_SITES_LOADED`], which means "the current delegate
/// answered AND held state" and can also be set by a legacy contribution. This
/// one is purely "did the current delegate answer yet", which is what tells the
/// UI whether an empty site list means "none" or "not found yet" (#52).
static CURRENT_KNOWN_SITES_ANSWERED: GlobalSignal<bool> = GlobalSignal::new(|| false);

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
                        //
                        // #52 proposed dispatching the sweep here instead, to
                        // take it off the critical path. Measured on a live
                        // node, it does not pay: the gap between this point and
                        // the current delegate's reply is 188-355 ms (cold
                        // wasmtime compilation of the freshly re-keyed
                        // delegate), the node answers delegate ops serially so
                        // early-dispatched legacy probes queue behind that same
                        // compile anyway, and the ordering rule means their
                        // results cannot be APPLIED any earlier regardless. The
                        // measured saving was ~50 ms, against a buffering
                        // mechanism in the one code path where a mistake
                        // silently destroys user data. Not worth it.
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

/// How long to keep saying we are still looking after the current delegate has
/// answered. Its reply is what starts the legacy sweep, so this has to cover
/// the sweep's own round trip.
#[cfg(target_arch = "wasm32")]
const DISCOVERY_SETTLE_GRACE_MS: u64 = 6_000;

/// Hard stop on the "looking for your sites" state, armed at page load, for the
/// case where the current delegate never answers at all.
///
/// Deliberately generous: #52 measured a recovery window of minutes, and
/// flipping to the newcomer welcome while recovery is still running is the exact
/// harm this whole change exists to remove. The cost of erring long is that a
/// genuine first-time user sees "Looking for your sites" for a while — with the
/// "Get Started" button live throughout, so they are never blocked.
#[cfg(target_arch = "wasm32")]
const DISCOVERY_SETTLE_FALLBACK_MS: u64 = 90_000;

/// Whether the settle grace timer has already been armed (it is idempotent, but
/// re-arming on every response would keep pushing the deadline out).
static DISCOVERY_SETTLE_ARMED: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Monotonic id of the current discovery round.
///
/// Settle timers are `spawn_local` tasks with no cancellation handle, so the
/// only way to retire one is to make it check on wake whether it still belongs
/// to the round it was armed for. A reconnect starts a NEW round, and every
/// timer from the old one must become a no-op.
///
/// Deliberately a plain atomic rather than a `GlobalSignal`: nothing renders
/// from it (it is a cancellation token, not reactive state), and it must be
/// readable from a plain `#[test]` so the arm/reset/fire sequence can actually
/// be driven. A `GlobalSignal` needs a Dioxus runtime, which is why every other
/// test around this code is either a pure function or a source scrape.
static DISCOVERY_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The round id a timer should stamp itself with as it arms.
fn current_discovery_generation() -> u64 {
    DISCOVERY_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

/// Start a new discovery round, retiring every timer already in flight.
fn begin_new_discovery_generation() {
    DISCOVERY_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Whether a timer armed for `armed_generation` may still settle discovery.
///
/// **Idempotence is the wrong property here.** `settle_site_discovery` is
/// idempotent, so double-settling is harmless — but that says nothing about
/// WHICH round a callback belongs to. Without this check, a grace timer armed
/// before a socket drop wakes up mid-way through the SECOND sweep and settles
/// it, putting the bare "Welcome to Delta" back on screen during recovery: the
/// exact screen this change exists to remove, in the exact scenario the
/// reconnect path exists to handle. The 90 s fallback has the same shape, and
/// because the reset arms a fresh one, a flapping socket accumulates them.
fn settle_timer_may_fire(armed_generation: u64) -> bool {
    armed_generation == current_discovery_generation()
}

/// Arm the "discovery is over" timer once the current delegate has answered —
/// which is also the moment the legacy sweep is dispatched. Until discovery
/// settles the UI shows a recovery message instead of the bare "Welcome to
/// Delta" empty state (#52).
fn arm_discovery_settle_if_ready() {
    if !*CURRENT_KNOWN_SITES_ANSWERED.read() {
        return;
    }
    let already_armed = DISCOVERY_SETTLE_ARMED.with_mut(|armed| std::mem::replace(armed, true));
    if !already_armed {
        let generation = current_discovery_generation();
        #[cfg(not(target_arch = "wasm32"))]
        let _ = generation;
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::sleep(std::time::Duration::from_millis(DISCOVERY_SETTLE_GRACE_MS))
                .await;
            if settle_timer_may_fire(generation) {
                state::settle_site_discovery();
            }
        });
    }
}

/// Forget that the legacy sweep ran, so the next successful reconnect re-runs it.
///
/// The sweep is fire-once per page load. If the socket dies after it was
/// dispatched but before its replies came back, those replies are lost, and the
/// reconnect's fresh `KnownSites` round would skip the sweep entirely — leaving
/// a returning user with no route to their sites short of a full reload. Called
/// from the connection error handler. Introduced by #52.
///
/// The reset is UNCONDITIONAL once the sweep has fired. An earlier version
/// skipped it when `CURRENT_SITES_LOADED` was set, reading that flag as
/// "recovery already produced something" — it does not mean that. It is set
/// whenever the CURRENT delegate held any record or tombstone, which says
/// nothing about whether any legacy reply arrived. A user whose current
/// delegate holds two sites while a third lives only in a legacy generation
/// would have had the sweep suppressed on every reconnect, and the legacy-only
/// site would never be recovered that session. Re-dispatching a sweep that did
/// complete is cheap and idempotent; skipping one that did not is data the user
/// never gets back.
///
/// Discovery is also returned to `Pending`, because the sweep it is re-running
/// is exactly what discovery reports on. Without that, a socket flap after the
/// grace period settles leaves the UI showing the bare "Welcome to Delta" for
/// the whole of the second sweep — the precise screen this change exists to
/// remove, in the scenario this function exists to handle.
pub fn reset_legacy_migration_for_reconnect() {
    // Bind first: never hold a signal read guard while writing another.
    let fired = *LEGACY_MIGRATION_FIRED.read();
    if fired {
        log("Delta: connection dropped mid-recovery; will re-probe legacy delegates on reconnect");
        *LEGACY_MIGRATION_FIRED.write() = false;
        // Re-arm the secret carry-forward too. A socket flap can strand it
        // mid-walk with predecessors unread, and its own markers do not
        // persist in the iframe, so without this the retry never happens.
        *SECRET_MIGRATION_FIRED.write() = false;
        *CURRENT_KNOWN_SITES_ANSWERED.write() = false;
        *DISCOVERY_SETTLE_ARMED.write() = false;
        state::reopen_site_discovery();

        // ORDER MATTERS. Start the new round FIRST, so every timer still
        // sleeping from the previous one is retired before anything new is
        // armed. Reversed, the fresh fallback below would stamp itself with the
        // OLD round and retire itself an instant later.
        //
        // This is not belt-and-braces. Without it, the grace timer armed before
        // the socket dropped wakes during the SECOND sweep and settles it,
        // showing the bare "Welcome to Delta" mid-recovery — the screen this
        // change removes, in the scenario this function handles. Note that
        // "settling is idempotent" does NOT cover this: idempotence says
        // nothing about which round a callback belongs to.
        begin_new_discovery_generation();

        // The page-load fallback has very likely fired by now, and a reopened
        // discovery with no deadline would strand the spinner if the second
        // sweep also fails. Arm a fresh one. Accumulating fallbacks across a
        // flapping socket is harmless because each is stamped with its round
        // and all but the current one no-op.
        arm_discovery_fallback();
    }
}

/// Arm the hard fallback that ends the "looking for your sites" state no matter
/// what happens. Called from app start, and again whenever a reconnect starts a
/// new discovery round.
///
/// Stamped with the round it was armed for, so a fallback left over from an
/// earlier round cannot settle a later one.
pub fn arm_discovery_fallback() {
    let generation = current_discovery_generation();
    #[cfg(not(target_arch = "wasm32"))]
    let _ = generation;
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::sleep(std::time::Duration::from_millis(
            DISCOVERY_SETTLE_FALLBACK_MS,
        ))
        .await;
        if settle_timer_may_fire(generation) {
            state::settle_site_discovery();
        }
    });
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
            // Offer the reply to any in-flight `freenet-migrate` round-trip, and
            // then FALL THROUGH to the normal handling regardless.
            //
            // Delta's delegate protocol has no request ids — adding one would
            // change the delegate WASM and re-key it, the exact event this
            // migration exists to survive — so the migration correlates replies
            // by (delegate key, reply kind, and whatever identity the reply
            // itself carries); see `delegate_migration::correlation` for why
            // kind alone is not sound while this sweep runs concurrently.
            //
            // The fall-through is load-bearing, not laziness. The hand-rolled
            // sweep in `fire_legacy_migration` runs CONCURRENTLY with the
            // migration, from this same trigger, and probes the same legacy
            // delegates with overlapping request kinds. If the migration
            // CONSUMED a reply, the sweep would never see it: a stolen
            // `KnownSites` reply would silently skip the sweep's restore for
            // that generation, so a legacy site would not appear in the UI on
            // the first load after a re-key — a user-visible regression caused
            // purely by adding the migration. Falling through keeps the
            // adoption strictly ADDITIVE.
            //
            // Double-handling is safe: the sweep's handlers are exactly the ones
            // that already run for these replies today, and all of them are
            // idempotent (tombstone-aware site merge, per-prefix key re-store,
            // tombstone-aware state reconcile).
            #[cfg(target_arch = "wasm32")]
            crate::freenet_api::delegate_migration::wasm_transport::offer_response(
                &responding_key,
                &response,
            );

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
                    if !is_legacy {
                        *CURRENT_KNOWN_SITES_ANSWERED.write() = true;
                        // The `freenet-migrate` secret carry-forward is started
                        // from the SAME arm, and nowhere else, so it inherits the
                        // ordering invariant documented in AGENTS.md
                        // ("Known-Sites Tombstone Convention"): the current
                        // delegate has already answered, so a predecessor's reply
                        // can no longer be applied ahead of it and resurrect a
                        // site the user removed.
                        start_delegate_secret_migration();
                        fire_legacy_migration();
                    }
                    arm_discovery_settle_if_ready();
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

/// Public wrapper so the `freenet-migrate` transport can address a specific
/// delegate. Same fire-and-forget send the rest of this module uses; the
/// migration layers request/reply correlation on top of it.
pub fn send_to_delegate_key_pub(request: &delta_core::DelegateRequest, delegate_key: DelegateKey) {
    send_to_delegate_key(request, delegate_key);
}

/// Carry predecessor delegates' secrets forward onto the current delegate, via
/// `freenet-migrate`'s delegate half.
///
/// Runs once the current delegate has answered, alongside the hand-rolled sweep
/// in [`fire_legacy_migration`] rather than replacing it yet. That staging is
/// deliberate and mirrors how Delta adopted the CONTRACT half (see the driver
/// header in `super::operations`): the library owns the walk order, marker
/// bookkeeping, never-clobber writes and per-predecessor classification, while
/// the existing sweep continues to own the UI-state restoration it also does
/// (site list reconciliation, contract GETs, hash-route replay) — which is not
/// what the library migrates. Both paths are idempotent and never-clobber, so
/// running both is safe; retiring the hand-rolled secret probing is a follow-up
/// once this is field-validated.
pub fn start_delegate_secret_migration() {
    #[cfg(target_arch = "wasm32")]
    {
        use super::delegate_migration as migration;

        if LEGACY_DELEGATES.is_empty() {
            return;
        }
        // Once per page load. See SECRET_MIGRATION_FIRED: this call site is
        // re-entered by the migration's own GetKnownSites reply, so without
        // the latch each run spawns another, unbounded in the iframe.
        if !claim_secret_migration_slot() {
            return;
        }
        let lineage = migration::delta_delegate_lineage(LEGACY_DELEGATES);
        let successor = current_delegate_key();
        // Seed the locally-known prefixes so a site whose key is stranded in a
        // predecessor is probed even when that predecessor's own known-sites
        // list is empty or unsupported.
        let known_prefixes: Vec<String> = state::SITES.read().keys().cloned().collect();

        wasm_bindgen_futures::spawn_local(async move {
            let channel = migration::wasm_transport::WasmChannel;
            let mut markers = migration::wasm_transport::BrowserMarkers::default();
            let report = migration::run_delegate_migration(
                &channel,
                &mut markers,
                successor,
                &lineage,
                known_prefixes,
            )
            .await;

            // `imported_total()` under-reports and can read zero for a migration
            // that recovered everything (freenet-migrate#16), so it is NOT
            // rendered as "recovered N secrets". Log the classification instead.
            log(&format!(
                "Delta: delegate secret migration finished — complete={}, retry_may_help={}, \
                 predecessors={}",
                report.is_complete(),
                report.retry_may_help(),
                report.predecessors.len()
            ));
            if report.any_unresponsive() {
                // The freenet/river#204 gate: some predecessor could not be
                // reached, so its data may exist and simply could not be
                // migrated. Never treat this as a clean fresh install.
                log(
                    "Delta: some predecessor delegates did not respond; their data may exist \
                     but could not be migrated automatically",
                );
            }
        });
    }
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
    if LEGACY_DELEGATES.is_empty() {
        return;
    }
    let already_fired = LEGACY_MIGRATION_FIRED.with_mut(|fired| std::mem::replace(fired, true));
    #[cfg(not(target_arch = "wasm32"))]
    let _ = already_fired;
    #[cfg(target_arch = "wasm32")]
    if !already_fired {
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

        // NOTE: the send order here is NOT cosmetic, despite looking it.
        // Every probe is dispatched concurrently, so reordering buys no
        // measurable time — but with an empty current delegate the FIRST
        // non-empty legacy reply latches `CURRENT_SITES_LOADED` and calls
        // `save_known_sites()`, after which `skip_older_legacy` discards every
        // remaining generation bar the newest. Send order therefore decides
        // which generation's view is persisted. #52 proposed reversing this to
        // newest-first; that may well be an improvement (the newest generation
        // is the one the reconciliation rules already treat as authoritative),
        // but it is a change to stored data and belongs in its own change with
        // its own tests, not smuggled in as a latency tweak.
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

    /// The baked `LEGACY_DELEGATES` must match `legacy_delegates.toml`.
    ///
    /// This is the pin that actually catches the bug #52 turned out to be, and
    /// it is deliberately NOT a build-script assertion. **A build script's
    /// `assert!` only runs when the build script runs**, and in the stale case
    /// Cargo skips it precisely because it believes nothing changed — so an
    /// assertion inside `build.rs` is structurally incapable of catching a
    /// stale bake. It guards a different, rarer class (a malformed or absent
    /// file, where the script does run) and is kept for that.
    ///
    /// `include_str!` is recorded in rustc's own dep-info, independently of the
    /// build script's fingerprint, so this test's copy of the registry is
    /// always current while the constant is current only if the script really
    /// re-ran. That asymmetry IS the mechanism: it goes red on a stale bake and
    /// on an empty one.
    ///
    /// Two limitations, stated precisely because an earlier version of this
    /// comment understated the test's reach in its own favour:
    ///
    /// - A genuinely cold build always runs the build script, so this cannot
    ///   be stale there. That does NOT mean it is toothless in CI: `ci.yml`
    ///   caches `target` with `restore-keys: ${{ runner.os }}-cargo-`, so CI
    ///   runs are routinely warm and the pin does have teeth. Its power is
    ///   greatest on warm incremental builds — including the local
    ///   `cargo make publish-delta` against a warm `target/`, which is where
    ///   the bug actually bites.
    /// - It observes the HOST bake. `dx build --release` compiles for
    ///   wasm32-unknown-unknown, which has its own fingerprint and its own
    ///   `OUT_DIR`, so a host bake being fresh is strong evidence rather than
    ///   a direct check of the shipped artifact. Nothing here inspects the
    ///   published bundle.
    #[test]
    fn the_baked_delegate_registry_matches_the_file_on_disk() {
        let toml = include_str!("../../../legacy_delegates.toml");
        // Exact match on the trimmed line, so a commented-out `# [[entry]]`
        // is not counted.
        let declared = toml.lines().filter(|l| l.trim() == "[[entry]]").count();

        assert!(
            declared > 0,
            "legacy_delegates.toml declares no entries at all — this registry \
             is append-only and can never legitimately be empty"
        );
        assert_eq!(
            LEGACY_DELEGATES.len(),
            declared,
            "the baked-in migration table has {} entries but \
             legacy_delegates.toml declares {}. The bundle would ship a STALE \
             or EMPTY table, the startup sweep would not ask the delegate \
             holding a returning user's data, and every returning user would \
             land on an empty \"Welcome to Delta\". Re-run the build; if that \
             fixes it, ui/build.rs is missing its rerun-if-changed directive.",
            LEGACY_DELEGATES.len(),
            declared
        );
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

    // ---- freenet/delta#52: when discovery starts, and what it may apply ----

    #[test]
    fn discovery_settling_is_gated_on_the_current_delegate_answering() {
        // Until it answers, an empty site list means "not found yet", and
        // saying "Welcome to Delta" is the data-loss impression #52 is about.
        // `arm_discovery_settle_if_ready` is not host-callable (it arms a wasm
        // timer), so pin its guard at the source level; the needle is assembled
        // at runtime so this test cannot satisfy itself via `include_str!`.
        let src = include_str!("delegate.rs");
        let body = src
            .split("fn arm_discovery_settle_if_ready()")
            .nth(1)
            .expect("arm_discovery_settle_if_ready must exist");
        let body = &body[..body.find("\n}\n").expect("body must be brace-bounded")];
        let needle = format!("{}{}", "!*CURRENT_KNOWN_SITES_", "ANSWERED.read()");
        assert!(
            body.contains(&needle),
            "the settle timer must not be armed before the current delegate has answered"
        );
    }

    /// Serialises the tests that mutate the process-global discovery
    /// generation, so they cannot interfere with each other under the default
    /// parallel test runner.
    static GENERATION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The race, driven end to end: arm a timer, reconnect, then let the old
    /// timer fire.
    ///
    /// This is a real behavioural test, not a scrape — the generation counter
    /// is a plain atomic precisely so the sequence is drivable without a Dioxus
    /// runtime. Deleting the check in either timer makes the third assertion
    /// meaningless, and mutating `settle_timer_may_fire` to `true` fails it
    /// outright.
    #[test]
    fn a_timer_from_the_previous_round_cannot_settle_the_current_one() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // t=0: the grace timer arms for the round in progress.
        let armed_before_drop = current_discovery_generation();
        assert!(
            settle_timer_may_fire(armed_before_drop),
            "a timer must be allowed to settle the round it was armed for"
        );

        // t=3s: socket drops, reconnect starts a new round.
        begin_new_discovery_generation();

        // t=6s: the ORIGINAL timer wakes. It must not settle the new round —
        // doing so puts the bare "Welcome to Delta" back up mid-recovery.
        assert!(
            !settle_timer_may_fire(armed_before_drop),
            "a timer armed before the reconnect must NOT settle the round that \
             replaced it; settling is idempotent, but idempotence says nothing \
             about which round a callback belongs to"
        );

        // A timer armed for the new round still works, or the spinner would
        // never come down.
        let armed_after_drop = current_discovery_generation();
        assert!(
            settle_timer_may_fire(armed_after_drop),
            "the reconnect's own timers must still be able to settle"
        );
    }

    /// A flapping socket must not resurrect an intermediate round either.
    #[test]
    fn only_the_newest_round_may_settle_after_repeated_flaps() {
        let _guard = GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let first = current_discovery_generation();
        begin_new_discovery_generation();
        let second = current_discovery_generation();
        begin_new_discovery_generation();
        let third = current_discovery_generation();

        assert!(!settle_timer_may_fire(first));
        assert!(!settle_timer_may_fire(second));
        assert!(settle_timer_may_fire(third));
    }

    /// Both timers must consult the guard, and the reset must start the new
    /// round BEFORE arming the replacement fallback.
    ///
    /// The behavioural tests above prove the predicate is right; they cannot
    /// see whether the wasm-only timer bodies actually call it, nor the
    /// ordering inside `reset_legacy_migration_for_reconnect`. Needles are
    /// assembled at runtime.
    #[test]
    fn both_settle_timers_are_generation_guarded_and_the_reset_orders_correctly() {
        let src = include_str!("delegate.rs");
        let guard = format!("{}{}", "settle_timer_may_", "fire(generation)");

        for func in [
            "fn arm_discovery_settle_if_ready()",
            "pub fn arm_discovery_fallback()",
        ] {
            let body = src.split(func).nth(1).unwrap_or_else(|| {
                panic!("{func} must exist");
            });
            let body = &body[..body.find("\n}\n").expect("body must be brace-bounded")];
            assert!(
                body.contains(&guard),
                "{func} arms a timer that outlives its round, so it must check \
                 the generation before settling"
            );
        }

        let reset = src
            .split("pub fn reset_legacy_migration_for_reconnect()")
            .nth(1)
            .expect("reset must exist");
        let reset = &reset[..reset.find("\n}\n").expect("body must be brace-bounded")];

        let bump = format!("{}{}", "begin_new_discovery_", "generation()");
        let rearm = format!("{}{}", "arm_discovery_", "fallback()");
        let bump_at = reset
            .find(&bump)
            .expect("the reset must start a new discovery round");
        let rearm_at = reset
            .find(&rearm)
            .expect("the reset must arm a replacement fallback");
        assert!(
            bump_at < rearm_at,
            "the new round must start BEFORE the replacement fallback is armed, \
             or the fallback stamps itself with the round it is replacing and \
             retires itself immediately"
        );
    }

    /// `fire_legacy_migration` owns its own once-per-page-load latch.
    ///
    /// That placement is load-bearing and easy to lose. Its caller is the
    /// current delegate's `KnownSites` arm, and `register_delegate()` re-issues
    /// `load_known_sites()` on every reconnect — so the call site fires more
    /// than once by design. If the internal `if !already_fired` guard is
    /// dropped, every reconnect re-dispatches all 3xN legacy probes.
    ///
    /// The latch must also be taken BEFORE the guard, otherwise it never
    /// latches at all. Both facts are checked inside the function body only,
    /// and the needles are assembled at runtime, so this test cannot satisfy
    /// itself via `include_str!`.
    #[test]
    fn the_legacy_sweep_latches_itself_against_repeat_dispatch() {
        let src = include_str!("delegate.rs");
        let body = src
            .split("fn fire_legacy_migration()")
            .nth(1)
            .expect("fire_legacy_migration must exist");
        let body = &body[..body.find("\n}\n").expect("body must be brace-bounded")];

        let latch = format!("{}{}", "LEGACY_MIGRATION_", "FIRED.with_mut(");
        let guard = format!("{}{}", "if !already_", "fired");

        let latch_at = body
            .find(&latch)
            .expect("the sweep must take its own fire-once latch");
        let guard_at = body.find(&guard).expect(
            "the sweep must skip dispatch when already fired — its caller runs \
             again on every reconnect",
        );
        assert!(
            latch_at < guard_at,
            "the latch must be taken before the guard is consulted, or it never \
             latches"
        );
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
