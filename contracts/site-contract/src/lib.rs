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
    use delta_core::{Page, SignedPageDeletion, SiteConfig};
    use ed25519_dalek::SigningKey;

    fn test_owner() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// #5072: `get_state_delta` against a peer that already has the exact
    /// same state must return an EMPTY delta. freenet-core's InterestSync
    /// staleness backstop (run only once two peers' summary bytes already
    /// differ, per `plan_fanout_send` skipping byte-identical summaries
    /// first) treats an empty `get_state_delta` result as "converged
    /// despite differing summary bytes"; a non-empty "nothing changed"
    /// delta means that backstop can never fire for this contract.
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

    /// delta#43 follow-up: a deletion pending relative to a specific peer's
    /// summary must produce a NON-EMPTY delta (see `deletion_pending_relative_to_peer_summary_is_not_an_empty_delta`
    /// in `delta-core`), and applying that delta through the contract's real
    /// `update_state` entry point must actually remove the page and record
    /// the tombstone on the peer that missed the live delete broadcast.
    /// This is the scenario the `self_delta_against_own_summary_is_empty`
    /// fix must not silence: a stale-but-summary-matching peer still needs
    /// to hear about the deletion.
    #[test]
    fn get_state_delta_carries_a_pending_deletion_and_heals_a_stale_peer() {
        let owner = test_owner();
        let params = SiteParameters::from_owner(&owner.verifying_key());
        let mut params_buf = Vec::new();
        into_writer(&params, &mut params_buf).unwrap();

        let mut site_state = SiteState::new(SiteConfig::default(), &owner);
        let page = Page::new(1, "Home".into(), "# Welcome".into(), 1000, &owner);
        site_state
            .upsert_page(1, page.clone(), &owner.verifying_key())
            .unwrap();

        // A peer that received the page before the deletion and hasn't
        // heard about it since (its state IS the pre-deletion state).
        let mut stale_peer_state = SiteState::new(SiteConfig::default(), &owner);
        stale_peer_state
            .upsert_page(1, page, &owner.verifying_key())
            .unwrap();
        let mut peer_state_buf = Vec::new();
        into_writer(&stale_peer_state, &mut peer_state_buf).unwrap();
        let peer_summary = Contract::summarize_state(
            Parameters::from(Vec::new()),
            State::from(peer_state_buf.clone()),
        )
        .expect("summarize_state should succeed");

        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site_state
            .delete_page(&deletion, &owner.verifying_key())
            .unwrap();
        let mut site_state_buf = Vec::new();
        into_writer(&site_state, &mut site_state_buf).unwrap();

        let delta = Contract::get_state_delta(
            Parameters::from(Vec::new()),
            State::from(site_state_buf),
            StateSummary::from(peer_summary.as_ref().to_vec()),
        )
        .expect("get_state_delta should succeed");

        assert!(
            !delta.as_ref().is_empty(),
            "a peer that still has the deleted page must get a non-empty delta"
        );

        let healed = Contract::update_state(
            Parameters::from(params_buf),
            State::from(peer_state_buf),
            vec![UpdateData::Delta(delta)],
        )
        .expect("update_state should apply the deletion delta");

        let healed_state = from_reader::<SiteState, &[u8]>(healed.unwrap_valid().as_ref())
            .expect("healed state should deserialize");

        assert!(
            healed_state.pages.is_empty(),
            "stale peer should have dropped the deleted page"
        );
        assert!(
            healed_state.deleted_pages.contains_key(&1),
            "stale peer should have adopted the tombstone"
        );
    }

    /// Review follow-up: `self_delta_against_own_summary_is_empty` proves
    /// `get_state_delta` can produce empty bytes; this pins the OTHER half
    /// of the contract this PR now relies on more heavily: `update_state`'s
    /// `UpdateData::Delta(d)` arm must treat those empty bytes as a no-op
    /// (`if d.as_ref().is_empty() { continue; }`) rather than attempting a
    /// CBOR decode. Nothing pinned this before. Deleting the guard leaves
    /// the whole workspace green today, yet every empty delta this PR now
    /// routinely emits would fail `update_state` with a deser error instead
    /// of no-opping.
    #[test]
    fn empty_delta_through_update_state_is_a_no_op() {
        let owner = test_owner();
        let params = SiteParameters::from_owner(&owner.verifying_key());
        let mut params_buf = Vec::new();
        into_writer(&params, &mut params_buf).unwrap();

        let mut site_state = SiteState::new(SiteConfig::default(), &owner);
        let page = Page::new(1, "Home".into(), "# Welcome".into(), 1000, &owner);
        site_state
            .upsert_page(1, page, &owner.verifying_key())
            .unwrap();
        let mut state_buf = Vec::new();
        into_writer(&site_state, &mut state_buf).unwrap();

        let result = Contract::update_state(
            Parameters::from(params_buf),
            State::from(state_buf.clone()),
            vec![UpdateData::Delta(StateDelta::from(Vec::new()))],
        )
        .expect("an empty delta must be a no-op, not a decode error");

        let resulting_state = result.unwrap_valid();
        let resulting = from_reader::<SiteState, &[u8]>(resulting_state.as_ref())
            .expect("resulting state should deserialize");
        let original = from_reader::<SiteState, &[u8]>(state_buf.as_slice())
            .expect("original state should deserialize");

        assert_eq!(
            resulting, original,
            "an empty delta must leave the state unchanged"
        );
    }

    /// Review follow-up: the ORIGINAL `self_delta_against_own_summary_is_empty`
    /// fixture never deletes a page, so it can't exercise the
    /// `page_deletions` branch this PR added to `compute_delta` at all — it
    /// would stay green even if that branch unconditionally included every
    /// local tombstone regardless of the peer's summary. This fixture
    /// deletes a page first, so the state genuinely has a non-empty
    /// `deleted_pages`, then diffs against its OWN (necessarily
    /// deletion-unaware, since `summarize()` never reflects tombstones)
    /// current summary. Dropping the `summary.pages.contains_key(id)` filter
    /// in `compute_delta` would make a converged site re-emit its whole
    /// tombstone set on every heartbeat forever; this test would catch that.
    #[test]
    fn self_delta_is_empty_for_a_state_that_has_deleted_pages() {
        let owner = test_owner();

        let mut site_state = SiteState::new(SiteConfig::default(), &owner);
        let page = Page::new(1, "Home".into(), "# Welcome".into(), 1000, &owner);
        site_state
            .upsert_page(1, page, &owner.verifying_key())
            .unwrap();
        let deletion = SignedPageDeletion::new(1, 2000, &owner);
        site_state
            .delete_page(&deletion, &owner.verifying_key())
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
            "a converged site with a tombstone must not re-emit it against \
             its own current summary, got {} bytes: {:?}",
            delta.as_ref().len(),
            delta.as_ref()
        );
    }
}
