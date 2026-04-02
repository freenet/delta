//! Delegate integration for persistent signing key storage.

use ciborium::{de::from_reader, ser::into_writer};
use delta_core::DelegateResponse;
use freenet_stdlib::client_api::ClientRequest;
// Alias to avoid collision with delta_core::DelegateRequest
use freenet_stdlib::client_api::DelegateRequest as StdlibDelegateRequest;
use freenet_stdlib::prelude::*;

/// Site delegate WASM.
const SITE_DELEGATE_WASM: &[u8] = include_bytes!("../../public/contracts/site_delegate.wasm");

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
                    Ok(_) => log("Delta: delegate registered"),
                    Err(e) => log(&format!("Delta: delegate registration failed: {e:?}")),
                }
            }
        });
    }
}

/// Store a signing key in the delegate's secret storage.
pub fn store_signing_key(prefix: &str, key_bytes: &[u8; 32]) {
    let request = delta_core::DelegateRequest::StoreSigningKey {
        key_bytes: key_bytes.to_vec(),
    };
    send_delegate_request(&request);
    let _ = prefix; // prefix used for logging context
}

/// Handle a delegate response.
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
                    log(&format!(
                        "Delta: delegate has public key: {}",
                        bs58::encode(vk.as_bytes()).into_string()
                    ));
                }
                DelegateResponse::SignedPage { page_id, page: _ } => {
                    log(&format!("Delta: delegate signed page {page_id}"));
                }
                DelegateResponse::Error(e) => {
                    log(&format!("Delta: delegate error: {e}"));
                }
                _ => {}
            }
        }
    }
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

fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{msg}");
}
