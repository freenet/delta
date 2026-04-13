# Delta

Delta is a decentralized publishing app built on [Freenet](https://freenet.org).

## Try It

If you have Freenet running locally, open Delta in your browser:

<http://127.0.0.1:7509/v1/contract/web/EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr/>

Not running Freenet yet? See the [Freenet quickstart](https://freenet.org/quickstart).

## Repository Layout

- `contracts/site-contract` — the site contract (shared state)
- `delegates/site-delegate` — the site delegate (private state / signing)
- `common` — shared types used by contract, delegate, and UI
- `ui` — the Delta web UI
- `legacy_contracts.toml` / `legacy_delegates.toml` — WASM migration manifests

## License

See [LICENSE](LICENSE).
