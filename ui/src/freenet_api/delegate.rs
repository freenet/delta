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
/// Maps (site_prefix, page_id) → contract key to update.
pub static PENDING_UPDATES: GlobalSignal<BTreeMap<(String, PageId), ContractKey>> =
    GlobalSignal::new(BTreeMap::new);

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
                        // Check current delegate for stored key
                        request_public_key();
                        // Try to migrate from legacy delegates
                        fire_legacy_migration();
                    }
                    Err(e) => log(&format!("Delta: delegate registration failed: {e:?}")),
                }
            }
        });
    }
}

/// Store a signing key in the delegate's secret storage.
pub fn store_signing_key(key_bytes: &[u8; 32]) {
    let request = delta_core::DelegateRequest::StoreSigningKey {
        key_bytes: key_bytes.to_vec(),
    };
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
    };
    send_delegate_request(&request);
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
    };
    send_delegate_request(&request);
}

/// Ask the delegate for the stored public key (checks if key exists).
fn request_public_key() {
    let request = delta_core::DelegateRequest::GetPublicKey;
    send_delegate_request(&request);
}

/// Handle a delegate response — route signed objects to the network.
pub fn handle_delegate_response(values: Vec<OutboundDelegateMsg>) {
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
                }
                DelegateResponse::PublicKey(vk) => {
                    let prefix = delta_core::pubkey_to_prefix(&vk);
                    log(&format!("Delta: delegate has key for site prefix {prefix}"));
                    // Mark the site as owner if we have it
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
                DelegateResponse::SignedConfig(config) => {
                    log(&format!(
                        "Delta: delegate signed config v{}",
                        config.config.version
                    ));
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

/// After receiving a signed deletion, update local state and send to network.
fn handle_signed_deletion(deletion: delta_core::SignedPageDeletion) {
    let page_id = deletion.page_id;
    let pending = PENDING_UPDATES.write().remove(&find_pending_key(page_id));

    let prefix = state::CURRENT_SITE.read().clone();
    if let Some(prefix) = &prefix {
        let mut sites = state::SITES.write();
        if let Some(site) = sites.get_mut(prefix) {
            site.state.pages.remove(&page_id);
        }
    }

    if let Some(contract_key) = pending {
        let delta = delta_core::SiteStateDelta {
            config: None,
            page_updates: BTreeMap::new(),
            page_deletions: vec![deletion],
        };
        super::operations::update_site(&contract_key, &delta);
    }
}

/// Find the pending key for a page_id (searches current site).
fn find_pending_key(page_id: PageId) -> (String, PageId) {
    let prefix = state::CURRENT_SITE.read().clone().unwrap_or_default();
    (prefix, page_id)
}

fn send_delegate_request(request: &delta_core::DelegateRequest) {
    #[cfg(target_arch = "wasm32")]
    {
        let mut payload = Vec::new();
        into_writer(request, &mut payload).expect("CBOR serialization");

        let delegate_code = DelegateCode::from(SITE_DELEGATE_WASM.to_vec());
        let params = Parameters::from(Vec::<u8>::new());
        let delegate = Delegate::from((&delegate_code, &params));
        let delegate_key = delegate.key().clone();

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
        let _ = request;
    }
}

/// Attempt to migrate signing keys from legacy delegate versions.
/// Fires GetPublicKey to each legacy delegate — if one responds,
/// the key gets migrated to the current delegate.
fn fire_legacy_migration() {
    #[cfg(target_arch = "wasm32")]
    {
        // CodeHash is available via freenet_stdlib::prelude::* (already imported)

        if LEGACY_DELEGATES.is_empty() {
            return;
        }

        log(&format!(
            "Delta: attempting migration from {} legacy delegate(s)",
            LEGACY_DELEGATES.len()
        ));

        for (i, (key_bytes, code_hash_bytes)) in LEGACY_DELEGATES.iter().enumerate() {
            let legacy_code_hash = CodeHash::new(*code_hash_bytes);
            let legacy_delegate_key = DelegateKey::new(*key_bytes, legacy_code_hash);

            let request = delta_core::DelegateRequest::GetPublicKey;
            let mut payload = Vec::new();
            if into_writer(&request, &mut payload).is_err() {
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
                            // Expected if legacy delegate isn't installed
                        }
                    }
                }
            });
        }
    }
}

fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{msg}");
}
