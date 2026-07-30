#![allow(unexpected_cfgs)]

use ciborium::{de::from_reader, ser::into_writer};
use delta_core::{SiteParameters, SiteState, SiteStateDelta, SiteStateSummary};
use freenet_stdlib::prelude::*;

#[allow(dead_code)]
struct Contract;

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        let bytes = state.as_ref();
        if bytes.is_empty() {
            return Ok(ValidateResult::Valid);
        }

        let site_state = from_reader::<SiteState, &[u8]>(bytes)
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let params = from_reader::<SiteParameters, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        site_state
            .verify(&params)
            .map(|_| ValidateResult::Valid)
            .map_err(|e| ContractError::InvalidUpdateWithInfo {
                reason: format!("State verification failed: {e}"),
            })
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let params = from_reader::<SiteParameters, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let mut site_state = if state.as_ref().is_empty() {
            SiteState::default()
        } else {
            from_reader::<SiteState, &[u8]>(state.as_ref())
                .map_err(|e| ContractError::Deser(e.to_string()))?
        };

        for update in data {
            match update {
                UpdateData::State(new_state) => {
                    let other = from_reader::<SiteState, &[u8]>(new_state.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    site_state.merge(&params, &other).map_err(|e| {
                        ContractError::InvalidUpdateWithInfo {
                            reason: e.to_string(),
                        }
                    })?;
                }
                UpdateData::Delta(d) => {
                    if d.as_ref().is_empty() {
                        continue;
                    }
                    let delta = from_reader::<SiteStateDelta, &[u8]>(d.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    site_state.apply_delta(&delta, &params).map_err(|e| {
                        ContractError::InvalidUpdateWithInfo {
                            reason: e.to_string(),
                        }
                    })?;
                }
                _ => {}
            }
        }

        let mut buf = Vec::new();
        into_writer(&site_state, &mut buf).map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(UpdateModification::valid(State::from(buf)))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        if state.as_ref().is_empty() {
            let summary = SiteStateSummary::default();
            let mut buf = Vec::new();
            into_writer(&summary, &mut buf).map_err(|e| ContractError::Deser(e.to_string()))?;
            return Ok(StateSummary::from(buf));
        }

        let site_state = from_reader::<SiteState, &[u8]>(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary = site_state.summarize();
        let mut buf = Vec::new();
        into_writer(&summary, &mut buf).map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(StateSummary::from(buf))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let site_state = if state.as_ref().is_empty() {
            SiteState::default()
        } else {
            from_reader::<SiteState, &[u8]>(state.as_ref())
                .map_err(|e| ContractError::Deser(e.to_string()))?
        };

        let peer_summary = from_reader::<SiteStateSummary, &[u8]>(summary.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        let delta = site_state.compute_delta(&peer_summary);

        // `compute_delta` already returns `None` when nothing has changed
        // relative to the peer's summary. Core's convergence check tests
        // whether the returned delta bytes are EMPTY, so that `None` must
        // become a zero-byte `StateDelta`, not a CBOR-encoded placeholder
        // struct (which is never empty: ciborium writes field names as map
        // keys, so even an empty `SiteStateDelta` serializes to ~39 bytes).
        // Do NOT re-introduce a serialized placeholder here.
        let buf = match delta {
            Some(d) => {
                let mut buf = Vec::new();
                into_writer(&d, &mut buf).map_err(|e| ContractError::Deser(e.to_string()))?;
                buf
            }
            None => Vec::new(),
        };

        Ok(StateDelta::from(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_core::{Page, SiteConfig};
    use ed25519_dalek::SigningKey;

    fn test_owner() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// #5072: `get_state_delta` against a peer that already has the exact
    /// same state must return an EMPTY delta. Core's convergence check
    /// (`get_state_delta(our_state, their_summary).is_empty()`) is the only
    /// signal it uses to decide two peers have converged; a non-empty
    /// "nothing changed" delta means that signal can never fire.
    #[test]
    fn self_delta_against_own_summary_is_empty() {
        let owner = test_owner();

        let mut site_state = SiteState::new(
            SiteConfig {
                name: "My Site".into(),
                ..Default::default()
            },
            &owner,
        );
        let page = Page::new(1, "Home".into(), "# Welcome".into(), 1000, &owner);
        site_state
            .upsert_page(1, page, &owner.verifying_key())
            .unwrap();

        let mut state_buf = Vec::new();
        into_writer(&site_state, &mut state_buf).unwrap();

        let summary =
            Contract::summarize_state(Parameters::from(Vec::new()), State::from(state_buf.clone()))
                .expect("summarize_state should succeed");

        let delta = Contract::get_state_delta(
            Parameters::from(Vec::new()),
            State::from(state_buf),
            StateSummary::from(summary.as_ref().to_vec()),
        )
        .expect("get_state_delta should succeed");

        assert!(
            delta.as_ref().is_empty(),
            "delta against own summary should be empty (0 bytes), got {} bytes: {:?}",
            delta.as_ref().len(),
            delta.as_ref()
        );
    }
}
