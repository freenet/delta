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
        // `MessageOrigin` became `#[non_exhaustive]` in freenet-stdlib
        // 0.5.0 when the `Delegate(DelegateKey)` variant was introduced
        // for inter-delegate attestation. The site delegate is driven
        // exclusively by the Delta web app via `MessageOrigin::WebApp` —
        // inter-delegate calls are not a supported authorization path, so
        // any `Delegate(..)` origin is rejected. The trailing wildcard
        // guards against future variants.
        match origin {
            Some(MessageOrigin::WebApp(_)) => {}
            Some(MessageOrigin::Delegate(caller)) => {
                return Err(DelegateError::Other(format!(
                    "site-delegate does not accept inter-delegate calls (caller: {caller})"
                )));
            }
            None => {
                return Err(DelegateError::Other("missing message origin".to_string()));
            }
            _ => {
                return Err(DelegateError::Other(
                    "unknown MessageOrigin variant — \
                     site-delegate must be rebuilt against a newer freenet-stdlib"
                        .into(),
                ));
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

        DelegateRequest::GetSigningKeyForPrefix { prefix } => {
            match load_signing_key(ctx, Some(prefix.as_str())) {
                Ok(key) => DelegateResponse::SigningKey(key.to_bytes().to_vec()),
                Err(e) => DelegateResponse::Error(e),
            }
        }

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
    let key_bytes = select_key_bytes(prefix, |name| ctx.get_secret(name.as_bytes()))
        .ok_or_else(|| "no signing key stored -- store key first".to_string())?;
    parse_signing_key(&key_bytes)
}

/// Key-selection policy for `load_signing_key`, extracted so it can be
/// unit-tested without a `DelegateCtx`.
///
/// The pre-V7 bug was that `GetSigningKey` had no prefix, so this function
/// (effectively) was always called with `prefix = None` from the export
/// path, silently returning the legacy single-key slot regardless of which
/// site was being exported. The priority must be: per-prefix key wins
/// when available; fall back to legacy only when no per-prefix key is
/// stored (or when no prefix was supplied at all).
fn select_key_bytes<F>(prefix: Option<&str>, mut read: F) -> Option<Vec<u8>>
where
    F: FnMut(&str) -> Option<Vec<u8>>,
{
    if let Some(p) = prefix {
        if let Some(bytes) = read(&signing_key_for_prefix(p)) {
            return Some(bytes);
        }
    }
    read(LEGACY_SIGNING_KEY)
}

fn parse_signing_key(key_bytes: &[u8]) -> Result<SigningKey, String> {
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "stored key is not 32 bytes".to_string())?;
    Ok(SigningKey::from_bytes(&key_array))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn store(entries: &[(&str, Vec<u8>)]) -> HashMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn per_prefix_key_wins_when_both_slots_populated() {
        // Regression test for the pre-V7 bug: when both per-prefix and
        // legacy slots hold keys, the per-prefix key must win so that
        // export returns the key matching the site's owner_pubkey.
        let per_prefix_bytes = vec![1u8; 32];
        let legacy_bytes = vec![2u8; 32];
        let store = store(&[
            (
                signing_key_for_prefix("abcdef1234").as_str(),
                per_prefix_bytes.clone(),
            ),
            (LEGACY_SIGNING_KEY, legacy_bytes),
        ]);

        let got = select_key_bytes(Some("abcdef1234"), |name| store.get(name).cloned());
        assert_eq!(got, Some(per_prefix_bytes));
    }

    #[test]
    fn falls_back_to_legacy_when_no_prefix_provided() {
        let legacy_bytes = vec![9u8; 32];
        let store = store(&[(LEGACY_SIGNING_KEY, legacy_bytes.clone())]);
        let got = select_key_bytes(None, |name| store.get(name).cloned());
        assert_eq!(got, Some(legacy_bytes));
    }

    #[test]
    fn falls_back_to_legacy_when_per_prefix_empty() {
        // Useful for users migrating from V1–V6: they have a key in the
        // legacy slot but no per-prefix entry yet. The delegate should
        // still return that key on signing requests.
        let legacy_bytes = vec![5u8; 32];
        let store = store(&[(LEGACY_SIGNING_KEY, legacy_bytes.clone())]);
        let got = select_key_bytes(Some("abcdef1234"), |name| store.get(name).cloned());
        assert_eq!(got, Some(legacy_bytes));
    }

    #[test]
    fn returns_none_when_nothing_stored() {
        let empty: HashMap<String, Vec<u8>> = HashMap::new();
        assert_eq!(
            select_key_bytes(Some("abcdef1234"), |name| empty.get(name).cloned()),
            None
        );
        assert_eq!(
            select_key_bytes(None, |name| empty.get(name).cloned()),
            None
        );
    }

    /// The site delegate is driven exclusively by the Delta web app via
    /// `MessageOrigin::WebApp`; inter-delegate invocation is not a supported
    /// authorization path. Regression test for the new rejection arm added
    /// alongside the freenet-stdlib 0.5.0/0.6.0 bump — `MessageOrigin::Delegate`
    /// did not exist at all before that bump, so this test pins the explicit
    /// "reject inter-delegate caller" policy so it cannot regress silently
    /// if a future refactor reorders the `match origin` arms.
    #[test]
    fn rejects_inter_delegate_origin() {
        use freenet_stdlib::prelude::{CodeHash, DelegateKey};

        // A trivial StoreKnownSites request payload so the inbound
        // message is structurally valid — the test only needs to
        // verify the origin rejection fires BEFORE request dispatch.
        let request = DelegateRequest::StoreKnownSites { sites: vec![] };
        let mut payload = Vec::new();
        into_writer(&request, &mut payload).unwrap();
        let app_msg = ApplicationMessage::new(payload);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let caller = DelegateKey::new([0xA1u8; 32], CodeHash::new([0xA2u8; 32]));
        let origin = Some(MessageOrigin::Delegate(caller));

        let result = SiteDelegate::process(
            &mut DelegateCtx::default(),
            Parameters::from(vec![]),
            origin,
            inbound_msg,
        );
        assert!(result.is_err(), "inter-delegate caller must be rejected");

        if let Err(DelegateError::Other(msg)) = result {
            assert!(
                msg.contains("does not accept inter-delegate calls"),
                "expected 'inter-delegate' rejection message, got: {msg}"
            );
        } else {
            panic!("expected DelegateError::Other, got {result:?}");
        }
    }
}
