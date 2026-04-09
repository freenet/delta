#![allow(unexpected_cfgs)]

use ciborium::{de::from_reader, ser::into_writer};
use delta_core::{DelegateRequest, DelegateResponse, Page, SignedConfig, SignedPageDeletion};
use ed25519_dalek::SigningKey;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateCtx, DelegateError, DelegateInterface,
    InboundDelegateMsg, MessageOrigin, OutboundDelegateMsg, Parameters,
};

const LEGACY_SIGNING_KEY: &str = "delta:signing_key";
const KNOWN_SITES_STORAGE_KEY: &str = "delta:known_sites";

fn signing_key_for_prefix(prefix: &str) -> String {
    format!("delta:signing_key:{prefix}")
}

pub struct SiteDelegate;

#[delegate]
impl DelegateInterface for SiteDelegate {
    fn process(
        ctx: &mut DelegateCtx,
        _parameters: Parameters<'static>,
        origin: Option<MessageOrigin>,
        message: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
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
        DelegateRequest::StoreSigningKey { key_bytes, prefix } => {
            if key_bytes.len() != 32 {
                DelegateResponse::Error("signing key must be 32 bytes".into())
            } else {
                let storage_key = match &prefix {
                    Some(p) => signing_key_for_prefix(p),
                    None => LEGACY_SIGNING_KEY.to_string(),
                };
                ctx.set_secret(storage_key.as_bytes(), &key_bytes);
                DelegateResponse::KeyStored
            }
        }

        DelegateRequest::SignPage {
            page_id,
            title,
            content,
            updated_at,
            order,
            prefix,
        } => match load_signing_key(ctx, prefix.as_deref()) {
            Ok(key) => {
                let page = Page::new_with_order(page_id, title, content, updated_at, order, &key);
                DelegateResponse::SignedPage { page_id, page }
            }
            Err(e) => DelegateResponse::Error(e),
        },

        DelegateRequest::SignPageDeletion {
            page_id,
            deleted_at,
            prefix,
        } => match load_signing_key(ctx, prefix.as_deref()) {
            Ok(key) => {
                let deletion = SignedPageDeletion::new(page_id, deleted_at, &key);
                DelegateResponse::SignedDeletion(deletion)
            }
            Err(e) => DelegateResponse::Error(e),
        },

        DelegateRequest::SignConfig { config, prefix } => {
            match load_signing_key(ctx, prefix.as_deref()) {
                Ok(key) => {
                    let signed = SignedConfig::new(config, &key);
                    DelegateResponse::SignedConfig(signed)
                }
                Err(e) => DelegateResponse::Error(e),
            }
        }

        DelegateRequest::GetPublicKey => match load_signing_key(ctx, None) {
            Ok(key) => DelegateResponse::PublicKey(key.verifying_key()),
            Err(e) => DelegateResponse::Error(e),
        },

        DelegateRequest::GetSigningKey => match load_signing_key(ctx, None) {
            Ok(key) => DelegateResponse::SigningKey(key.to_bytes().to_vec()),
            Err(e) => DelegateResponse::Error(e),
        },

        DelegateRequest::StoreKnownSites { sites } => {
            let mut buf = Vec::new();
            into_writer(&sites, &mut buf)
                .map_err(|e| DelegateError::Other(format!("CBOR serialization: {e}")))?;
            ctx.set_secret(KNOWN_SITES_STORAGE_KEY.as_bytes(), &buf);
            DelegateResponse::SitesStored
        }

        DelegateRequest::GetKnownSites => {
            if let Some(data) = ctx.get_secret(KNOWN_SITES_STORAGE_KEY.as_bytes()) {
                match from_reader::<Vec<delta_core::KnownSiteRecord>, _>(data.as_slice()) {
                    Ok(sites) => DelegateResponse::KnownSites(sites),
                    Err(e) => DelegateResponse::Error(format!("deserialize known sites: {e}")),
                }
            } else {
                DelegateResponse::KnownSites(Vec::new())
            }
        }

        DelegateRequest::StoreSiteState {
            prefix,
            state_bytes,
        } => {
            let key = format!("delta:site_state:{prefix}");
            ctx.set_secret(key.as_bytes(), &state_bytes);
            DelegateResponse::SiteStateStored
        }

        DelegateRequest::GetSiteState { prefix } => {
            let key = format!("delta:site_state:{prefix}");
            if let Some(data) = ctx.get_secret(key.as_bytes()) {
                DelegateResponse::SiteState {
                    prefix,
                    state_bytes: data,
                }
            } else {
                DelegateResponse::Error(format!("no backed-up state for site {prefix}"))
            }
        }
    };

    let mut payload = Vec::new();
    into_writer(&response, &mut payload)
        .map_err(|e| DelegateError::Other(format!("failed to serialize response: {e}")))?;

    Ok(vec![OutboundDelegateMsg::ApplicationMessage(
        ApplicationMessage::new(payload).processed(true),
    )])
}

/// Load a signing key. Tries per-prefix first, then falls back to legacy single-key storage.
fn load_signing_key(ctx: &mut DelegateCtx, prefix: Option<&str>) -> Result<SigningKey, String> {
    // Try per-prefix key first
    if let Some(p) = prefix {
        let per_prefix_key = signing_key_for_prefix(p);
        if let Some(key_bytes) = ctx.get_secret(per_prefix_key.as_bytes()) {
            return parse_signing_key(&key_bytes);
        }
    }

    // Fall back to legacy single-key storage
    let Some(key_bytes) = ctx.get_secret(LEGACY_SIGNING_KEY.as_bytes()) else {
        return Err("no signing key stored -- store key first".into());
    };
    parse_signing_key(&key_bytes)
}

fn parse_signing_key(key_bytes: &[u8]) -> Result<SigningKey, String> {
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "stored key is not 32 bytes".to_string())?;
    Ok(SigningKey::from_bytes(&key_array))
}
