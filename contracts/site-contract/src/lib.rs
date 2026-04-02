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

        let mut buf = Vec::new();
        match delta {
            Some(d) => into_writer(&d, &mut buf),
            None => into_writer(
                &SiteStateDelta {
                    config: None,
                    page_updates: std::collections::BTreeMap::new(),
                    page_deletions: Vec::new(),
                },
                &mut buf,
            ),
        }
        .map_err(|e| ContractError::Deser(e.to_string()))?;

        Ok(StateDelta::from(buf))
    }
}
