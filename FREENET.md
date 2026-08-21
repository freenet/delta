# FREENET.md

This file enumerates the Freenet contracts and delegates published from this repository — what each one is for, where its source lives, and how to depend on it — for anyone integrating with Delta rather than building it. It's a convention (see [freenet-core#5194](https://github.com/freenet/freenet-core/issues/5194)), not a protocol requirement: a fixed, predictable place to look before reading source.

## Contracts

### site-contract
- **Purpose:** Holds a single Delta site — its pages, metadata, and the owner authorisation that makes the state self-validating.
- **Source:** [`contracts/site-contract/`](contracts/site-contract/)
- **Shared types crate:** `delta-core` ([`common/`](common/)) — the state types live here, independent of the WASM target.
- **Deployed key:** none fixed. Every site is its own instance, keyed from that site's owner key, so there are as many keys as there are sites.
- **Migration:** re-keys on any WASM change; [`legacy_contracts.toml`](legacy_contracts.toml) records every prior generation so a client can recover a site dormant across an upgrade.

### web-container-contract
- **Purpose:** Serves the compiled Delta UI as a Freenet contract asset — this is what a browser loads when it opens Delta.
- **Deployed key:** `EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr` (fixed — one instance, republished in place on every UI release via a monotonically-versioned author-signed update, never re-keyed for a UI-only change).
- The compiled `web_container_contract.wasm` artifact is River's, reused rather than vendored as source.

## Delegates

### site-delegate
- **Purpose:** Executes Delta-specific background logic on the user's own node, including per-site secret storage and legacy-migration probing.
- **Source:** [`delegates/site-delegate/`](delegates/site-delegate/)
- **Migration:** re-keys on any WASM change (code, a dependency bump, even a version-string bump); [`legacy_delegates.toml`](legacy_delegates.toml) records every prior generation.

## Notes for integrators

- Depend on `delta-core` for the wire types; you almost never need to compile or execute the contract/delegate WASM yourself to read or construct Delta-compatible data.
- Every contract/delegate here can re-key on any release — **a build-time-constant reference to a key will silently go stale.** Resolve a pointer instead; see below.

## Stable identity: resolve a pointer, do not pin a key

Delta publishes **pointer records** for the two artifacts that re-key. A pointer record is a contract at a **fixed address** whose state names the artifact's *current* code hash, signed by Delta's author key. You GET the pointer, read the code hash, and derive the key you actually wanted from that hash **plus your own params**. The address never changes, so your build-time constant never goes stale.

This implements the convention in [freenet-core#5194](https://github.com/freenet/freenet-core/issues/5194).

### The author verifying key — your trust anchor

```
river:v1:vk:54RwkjvbrXRzjyg4z2EbNCzw3Qwx5YpWQegpMyCTwibE
```

Pin **this 32-byte value** as a constant in your build. It is the entire trust anchor: take it from anywhere else and you may resolve a validly-signed pointer belonging to somebody else. You can check it without trusting this file — its raw bytes are `published-contract/webapp.parameters`, which is why the web container id `EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr` derives from it. (The `river:v1:vk:` prefix is just the shared encoding Delta reuses from River's `web-container-tool`; it says nothing about ownership.)

Two things we would rather you learned here than discovered later:

- **Delta does not keep this key offline.** It is the same key used on every UI publish. Delta has no separate author identity — its data model is per-*site* owner keys, so this is the only publisher identity it has.
- **Rotating it would move everything at once** — the web container's address and both pointer addresses. That would strand anyone who baked in the author vk, so it is a coordinated flag day, not routine key hygiene.

### The pointers

| `app_id` | Points at | Pointer key (fixed, GET this) |
|---|---|---|
| `delta.site-contract` | [`contracts/site-contract/`](contracts/site-contract/) | `6a8ZBaFft9wVFd1mAWVRRZepXXrnQNzRCD5tqM71hBm5` |
| `delta.site-delegate` | [`delegates/site-delegate/`](delegates/site-delegate/) | `ES2hnErmSh9Aip4862ZDKQCNvMeryfhj6b7FfpP5qmyZ` |

Both addresses are derivable offline from the pointer contract's frozen code hash `8wnAPaSRY1oYZCz723fdwK6BgzL6q8ozP3buVovXnt6v` and `(author_vk ‖ app_id)` — you do not have to trust the table.

Current records are in [`pointer-records.toml`](pointer-records.toml), which CI checks on every PR (`scripts/check-pointer-freshness.sh`): if a pointed-at WASM changes and no new record is signed, the build fails. That gate is the reason resolving is safer than pinning.

### How to resolve

Rust integrators should use the resolver rather than hand-rolling it — it carries the anti-rollback floor and the absence-vs-unreachability distinction, neither of which you get from decoding the record yourself:

```rust
use freenet_migrate::pointer::{resolve_app_pointer, PointerFloor, PointerOutcome};

let outcome = resolve_app_pointer(&mut io, &DELTA_AUTHOR_VK, b"delta.site-delegate", floor).await?;
```

Handle **every** arm. A bare `if let Some(r) = outcome.resolved()` silently does nothing on the outcomes that carry no record, which is how a withdrawal, a rollback attempt and a plain timeout all become "no output". Only `NeverPublished` permits falling back to a baked-in key. Persist `outcome.next_floor()`, keyed by `(author_vk, app_id)`.

Non-Rust implementers: the wire format, the four resolution steps and hex test vectors are in the [pointer contract's README](https://github.com/freenet/freenet-migrate/tree/main/contracts/pointer-contract).

### What a pointer does NOT tell you

**It solves addressing only.** It tells you which code hash is current. It says nothing about whether any state or any secret held under the previous key survived the re-key.

This matters most for `delta.site-delegate`. Delegate secrets move only when Delta's own UI has run on that user's node, so you can resolve the pointer perfectly, derive the right key, and still find an **empty namespace** — which looks like "this user has no data" rather than like an error. That is the specific confusion the pointer exists to remove, so please do not let it back in one level up: treat data survival as a separate question, and assume it is unsolved until you have verified it.
