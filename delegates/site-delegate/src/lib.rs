#![allow(unexpected_cfgs)]

use ciborium::{de::from_reader, ser::into_writer};
use delta_core::{DelegateRequest, DelegateResponse, Page, SignedConfig, SignedPageDeletion};
use ed25519_dalek::SigningKey;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateCtx, DelegateError, DelegateInterface,
    InboundDelegateMsg, MessageOrigin, OutboundDelegateMsg, Parameters,
};

const SIGNING_KEY_STORAGE_KEY: &str = "delta:signing_key";

pub struct SiteDelegate;

#[delegate]
impl DelegateInterface for SiteDelegate {
    fn process(
        ctx: &mut DelegateCtx,
        _parameters: Parameters<'static>,
        origin: Option<MessageOrigin>,
        message: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        // Verify origin
        match origin {
            Some(MessageOrigin::WebApp(_)) => {}
            None => {
                return Err(DelegateError::Other("missing message origin".to_string()));
            }
        }

        match message {
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                if app_msg.processed {
                    return Err(DelegateError::Other(
                        "cannot process already processed message".into(),
                    ));
                }
                handle_app_message(ctx, app_msg)
            }
            other => Err(DelegateError::Other(format!(
                "unexpected message type: {other:?}"
            ))),
        }
    }
}

fn handle_app_message(
    ctx: &mut DelegateCtx,
    msg: ApplicationMessage,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let request: DelegateRequest = from_reader(msg.payload.as_slice())
        .map_err(|e| DelegateError::Other(format!("failed to deserialize request: {e}")))?;

    let response = match request {
        DelegateRequest::StoreSigningKey { key_bytes } => {
            if key_bytes.len() != 32 {
                DelegateResponse::Error("signing key must be 32 bytes".into())
            } else {
                ctx.set_secret(SIGNING_KEY_STORAGE_KEY.as_bytes(), &key_bytes);
                DelegateResponse::KeyStored
            }
        }

        DelegateRequest::SignPage {
            page_id,
            title,
            content,
            updated_at,
        } => match load_signing_key(ctx) {
            Ok(key) => {
                let page = Page::new(page_id, title, content, updated_at, &key);
                DelegateResponse::SignedPage { page_id, page }
            }
            Err(e) => DelegateResponse::Error(e),
        },

        DelegateRequest::SignPageDeletion {
            page_id,
            deleted_at,
        } => match load_signing_key(ctx) {
            Ok(key) => {
                let deletion = SignedPageDeletion::new(page_id, deleted_at, &key);
                DelegateResponse::SignedDeletion(deletion)
            }
            Err(e) => DelegateResponse::Error(e),
        },

        DelegateRequest::SignConfig { config } => match load_signing_key(ctx) {
            Ok(key) => {
                let signed = SignedConfig::new(config, &key);
                DelegateResponse::SignedConfig(signed)
            }
            Err(e) => DelegateResponse::Error(e),
        },

        DelegateRequest::GetPublicKey => match load_signing_key(ctx) {
            Ok(key) => DelegateResponse::PublicKey(key.verifying_key()),
            Err(e) => DelegateResponse::Error(e),
        },
    };

    let mut payload = Vec::new();
    into_writer(&response, &mut payload)
        .map_err(|e| DelegateError::Other(format!("failed to serialize response: {e}")))?;

    Ok(vec![OutboundDelegateMsg::ApplicationMessage(
        ApplicationMessage::new(payload).processed(true),
    )])
}

fn load_signing_key(ctx: &mut DelegateCtx) -> Result<SigningKey, String> {
    let Some(key_bytes) = ctx.get_secret(SIGNING_KEY_STORAGE_KEY.as_bytes()) else {
        return Err("no signing key stored — store key first".into());
    };

    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "stored key is not 32 bytes".to_string())?;

    Ok(SigningKey::from_bytes(&key_array))
}
