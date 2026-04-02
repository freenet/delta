use ciborium::de::from_reader;
use delta_core::SiteState;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use std::sync::Arc;

use crate::state::{self, KnownSite, SiteRole};

/// Site contract WASM (embedded at build time).
const SITE_CONTRACT_WASM: &[u8] = include_bytes!("../../public/contracts/site_contract.wasm");

/// Handle an incoming response from the Freenet node.
pub fn handle_response(response: HostResponse) {
    match response {
        HostResponse::ContractResponse(contract_response) => {
            handle_contract_response(contract_response);
        }
        HostResponse::DelegateResponse { .. } => {
            log("Delta: delegate response received");
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
            log(&format!("Delta: GET response for {key}"));
            let state_bytes = state.to_vec();
            if !state_bytes.is_empty() {
                handle_site_state(key, &state_bytes);
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
        }
        ContractResponse::UpdateResponse { key, .. } => {
            log(&format!("Delta: UPDATE succeeded for {key}"));
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
    let prefix = key_to_prefix(&key);

    let mut sites = state::SITES.write();
    if let Some(existing) = sites.get_mut(&prefix) {
        existing.state = site_state;
        existing.name = name;
    } else {
        sites.insert(
            prefix.clone(),
            KnownSite {
                name,
                prefix: prefix.clone(),
                role: SiteRole::Visitor,
                state: site_state,
                owner_pubkey: [0u8; 32],
            },
        );
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

    let prefix = key_to_prefix(&key);
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
}

/// Subscribe to a site contract to receive live updates.
#[allow(dead_code)]
pub fn subscribe_to_site(contract_key: &ContractKey) {
    let key = *contract_key.id();
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
    let key = *contract_key.id();
    send(move |api| {
        Box::pin(async move {
            let request = ClientRequest::ContractOp(ContractRequest::Get {
                key,
                return_contract_code: false,
                subscribe: false,
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
                data: UpdateData::State(buf.into()),
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

/// Derive a short prefix from a contract key (first 10 chars of encoded ID).
fn key_to_prefix(key: &ContractKey) -> String {
    let encoded = key.encoded_contract_id();
    encoded[..10.min(encoded.len())].to_string()
}

fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{msg}");
}
