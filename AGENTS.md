# Delta - Agent Guide

## Key Concepts

### Site Identity

A site is identified by a 10-character **prefix** derived from the owner's Ed25519 public key: `base58(pubkey)[..10]`. This prefix IS the contract parameters. The full contract key is `BLAKE3(BLAKE3(site_contract.wasm) || CBOR({prefix}))`. Anyone who knows the prefix can compute the contract key because the WASM is public.

### CRITICAL: All State Fields Must Be Authenticated

**Every field in the contract state MUST be covered by a cryptographic signature.** Freenet contracts run on untrusted peers who can modify state. The contract validates signatures, but only for fields included in the signing bytes. An unsigned field is world-writable.

When adding a new field to any signed struct (Page, SignedConfig, SignedPageDeletion):
1. Include it in the signing bytes immediately
2. If backwards compatibility is needed, use a versioned signature (try v2 first, fall back to v1)
3. Add a test verifying the new field is covered by the signature

Page signatures use v2 format (`delta:page:v2:`) which covers: page_id, title, content, updated_at, order. V1 fallback (without order) exists for pre-existing pages.

**`updated_at` must be strictly greater than the page's current `updated_at`.** `apply_delta` and `merge` in `delta-core` dominate equal timestamps with `>=`, so an UPDATE whose `updated_at` matches what's already in state is silently dropped on the network. Any UI path that produces a page UPDATE MUST route through `next_page_updated_at` in `ui/src/state.rs`, which computes `max(now_secs(), existing + 1)`. Calling `now_secs()` directly reintroduces silent same-second collisions.

**Page `order` invariants** (`swap_page_order` / `create_page` in `ui/src/state.rs`):

1. `swap_page_order` MUST sign and propagate a fresh page-UPDATE for **every** page whose order changes, not just the two clicked. When any page is still at `order == 0` (legacy/pre-order pages), the swap also performs a one-time site-wide migration to explicit orders `(10, 20, 30, …)`. Skipping propagation leaves unmigrated pages at `order = 0` and they clump to the front of the sidebar after refresh.
2. `create_page` MUST assign `order = max(existing) + ORDER_STEP` via `next_create_order`, never `0` — issuing `0` re-poisons a migrated site.
3. `plan_swap` derives `pages_to_sign` from the diff between current and new orders; unit tests catch a regression that re-narrows the sign set.

**Delegate-response routing MUST use signature verification, not `CURRENT_SITE`.** `handle_signed_page` / `handle_signed_deletion` / `handle_signed_config` in `ui/src/freenet_api/delegate.rs` look up the owning site by checking the signature against every known owner's pubkey (`find_owner_for_signed_*`). Keying by `(CURRENT_SITE, page_id)` instead drops concurrent UPDATEs for the same page and misroutes a signed page during a mid-flight site switch.

### Page Links

- `[[2]]` - renders as current page title, auto-updates on rename
- `[[2|custom text]]` - renders as "custom text", never changes
- `[[Page Title]]` - title lookup, renders as title

Autocomplete inserts `[[id]]` format.

### Delegate Storage

- **Signing keys**: `delta:signing_key:{prefix}` - per-site Ed25519 private keys (legacy: `delta:signing_key`)
- **Known sites**: `delta:known_sites` - list of sites with prefix, name, role, contract key
- **Site state backups**: `delta:site_state:{prefix}` - full state backup for network resilience

### Known-Sites Tombstone Convention

`delta:known_sites` doubles as the tombstone store for removed sites: a record with `name == TOMBSTONE_NAME_SENTINEL` (`"\0__delta_removed__"`, `common/src/state.rs`) blocks legacy delegates from resurrecting deleted sites after refresh.

**Every consumer of `KnownSites` responses MUST filter tombstones via `KnownSiteRecord::is_tombstone()`**, or a sentinel leaks into the UI as a ghost site. `restore_known_sites` has a `debug_assert!` for debug builds. Tombstones are cleared by `clear_tombstone(prefix)`, called by any path that adds a site (`create_new_site`, `import_site_key`, `visit_site`) — otherwise a re-added prefix is silently filtered out again.

**Tombstone-application rules** (`filter_applicable_tombstones` in `ui/src/freenet_api/delegate.rs`, pinned by its unit tests):

1. Once `CURRENT_SITES_LOADED` is true, legacy-delegate tombstones are dropped — the current delegate is authoritative for the removal set.
2. A tombstone whose prefix is currently live in `SITES` is always dropped — live user intent beats any stale removal record.

**Ordering invariant: the legacy sweep must not start until the current delegate's KnownSites response has arrived** (`fire_legacy_migration` is called only from that arm). A legacy reply applied first inverts both rules above: it still lists a site the user removed under the current delegate, with no tombstone yet known to suppress it.

#52 proposed dispatching the sweep earlier to shave latency, with legacy responses buffered for ordering. **Do not do this without new evidence** — a first attempt buffered on a 2s deadline, and review found that a slow current delegate could still lose the race and overwrite the delegate's stored record without the current tombstones, permanently resurrecting a removed site. The measured latency saving was marginal against total time-to-site. The user-visible half of #52 is handled by `state::SiteDiscovery` instead, which never touches stored data.

## Reproducible WASM Builds

The repo pins rustc via `rust-toolchain.toml` (currently `1.94.1`). This is **load-bearing for the migration system**: the delegate/contract key is `BLAKE3(BLAKE3(wasm) || params)`, so any WASM byte change — including one from an LLVM upgrade in a newer rustc — produces a new key and orphans every user's stored data unless a migration entry is recorded first.

CI's "Delegate migration safety" job runs `scripts/check-migration.sh` on each PR: it rebuilds the WASMs from source and refuses to merge if committed hashes don't match, or if a changed WASM's predecessor hash isn't recorded in `legacy_delegates.toml` / `legacy_contracts.toml`.

`river` and `freenet-core` pin the same way — don't let the pin drift past those sibling repos, or a shared dependency will hash differently across the ecosystem.

### Upgrading the pinned rustc

Bumping `channel` in `rust-toolchain.toml` is a **data-migration gesture**: predecessor hashes recorded first, WASM regenerated, single-commit PR, post-merge republish, browser verification. The full canonical procedure lives at the top of `rust-toolchain.toml`. Skipping any step silently breaks data continuity for every existing user with no automatic recovery.

## Contract Upgrade / State Migration

When `site_contract.wasm` changes (code, dependency, or `common/` changes), ALL site contract keys change. Migration is **permissionless** since all state is owner-signed: any node can GET from the old key and PUT to the new key, and the new contract validates and accepts it.

**How Delta handles it automatically:** the delegate stores each site's contract key (`KnownSiteRecord.contract_key_b58`); on startup the UI recomputes the key from the prefix and the embedded WASM; a mismatch triggers GET-old/PUT-new and updates the stored key.

**Multi-hop fallback:** when a restored record has no `contract_key_b58` (legacy delegates predating b82d3bc) or the stored key is no longer on the network, the UI probes every previous contract WASM hash in `legacy_contracts.toml`. Every candidate generation is reconciled via the tombstone-aware `reconcile_into` merge (keeps newest, preserves deletions, order-independent) — legacy sibling probes are deliberately left running (`operations.rs:44-53`, `:205-220`, `:255-270`) since a slower generation may still hold the newest data.

The sweep runs on `freenet-migrate`'s `ProbeDriver` with `SelectionPolicy::FoldAll` (`operations.rs:939-949`, adopted delta#36): it bounds the sweep to one decision and one forward PUT once every candidate resolves (`finalize_migration_sweep`), rather than re-PUTting on every legacy response. `FoldAllAck` (`i_understand_fold_all_resurrects_without_tombstones`) is a deliberate, loud acknowledgement, not a formality: folding every generation can RESURRECT a page deleted before tombstones existed (generation C1). That residual is open as delta#38 - do not read the policy as unqualified.

**Recording contract WASM hashes is part of the release process.** Any commit that changes `site_contract.wasm` — including an incidental rebuild from touching `common/` — must first run `./scripts/add-contract-migration.sh`. `scripts/check-migration.sh` enforces this by walking git history of the WASM file and refusing to publish unless every committed generation other than the current one is recorded (delegate gated identically against `legacy_delegates.toml`).

### Delegate WASM Migration

When `site_delegate.wasm` changes, stored secrets (signing keys, known sites, site state backups) become inaccessible under the old key. Two mechanisms run side by side (both armed from the same match arm, `delegate.rs:882-883`, neither behind a feature flag), both idempotent and never-clobber:

- **Hand-rolled sweep**, `fire_legacy_migration()` (`delegate.rs:1407-1472`): owns UI-state restoration (site list reconciliation, contract GETs, hash-route replay). Once the current delegate's KnownSites response has arrived, it sends GetPublicKey/GetKnownSites/GetSigningKey to each legacy delegate. **Send order is load-bearing**: the first non-empty legacy reply latches `CURRENT_SITES_LOADED` and calls `save_known_sites()`, after which `skip_older_legacy` discards every remaining generation but the newest.
- **Crate walk**, `start_delegate_secret_migration()` (`delegate.rs:1182`) via `freenet_migrate::migrate_delegate_secrets`: owns walk order, durable per-predecessor markers, never-clobber writes.

Running both is deliberate staged rollout, not an oversight; retiring the hand-rolled path is a follow-up once the walk is field-validated (see the `freenet-migrate-adoption` skill).

**`legacy_delegates.toml` is baked into the UI at build time** by `ui/build.rs` (`cargo:rerun-if-changed`). `add-migration.sh` edits the file without staging it — without the rerun directive, Cargo reuses cached build output and ships a **stale** migration table, silently orphaning every returning user's data on a "Welcome to Delta" screen. A build assertion now fails the build if `[[entry]]` sections exist but none deserialize (the `entry` field is `#[serde(default)]`, so a structural mismatch otherwise yields an empty table with no error).

**A worktree is the WRONG instrument for reproducing a build-caching or publish problem.** In a git worktree, `.git` is a file pointing at `.git/worktrees/<name>`. An unresolvable `rerun-if-changed` target used to leave Cargo treating the build script as permanently dirty, so it always re-ran - which is exactly what hid the missing directive above from every agent who investigated inside a worktree. `ui/build.rs` now resolves those paths with `git rev-parse --git-path` and both layouts cache identically, but the general lesson outlives the fix: reproduce build-caching and publish issues in a clean clone or the main checkout.

**Every delegate storage key type must be migrated.** If a new storage op is added to the delegate (e.g. `StoreFoo`/`GetFoo`), the corresponding `GetFoo` MUST be added to `fire_legacy_migration()` and the KnownSites handler, or that data is lost silently on upgrade. April 2026: `GetSiteState` was missing, causing sites to vanish when network state had been GC'd. **Defense in depth:** `request_site_state_backup()` (NotFound handler) queries the current delegate AND all legacy delegates, catching prefixes the proactive KnownSites-time fetch misses.

### Upgrade Workflow

```bash
# 1. Record old delegate + contract WASM hashes (BEFORE any code change).
#    Changes to `common/` touch BOTH WASMs.
./scripts/add-migration.sh V2 "Before adding deleted_pages field"
./scripts/add-contract-migration.sh C3 "Before adding deleted_pages field"

# 2. Make code changes; 3. Rebuild WASMs
./scripts/sync-wasm.sh

# 3b. Re-sign pointer records against the new WASMs (carries THIRD PARTIES
#     forward, not our own users — see "Stable identity" below). CI's
#     pointer-freshness job fails the PR if you skip it.
./scripts/sign-pointer-records.sh

# 4. Build and publish. `publish-delta` depends on `preflight`, which runs
#    check-migration.sh and aborts before anything is signed/published if a
#    predecessor hash is missing.
cargo make publish-delta

# 5. Commit everything, including the bumped version counter.
git add legacy_delegates.toml legacy_contracts.toml pointer-records.toml \
    ui/public/contracts/ common/ contracts/ published-contract/contract-version.txt
git commit -m "fix: description with delegate migration"
git push
# published-contract/contract-version.txt is NOT updated by the migration
# workflow — `cargo make sign-webapp` (run by publish-delta) bumps it.

# 6. AFTER the PR merges, from main: publish the re-signed pointer records.
#    Signing is offline and belongs in the PR; the network write does not.
./scripts/publish-pointer-records.sh --node-port <your node, NOT 7509> \
    --pointer-wasm <path to the COMMITTED pointer-v1.wasm>
```

Steps 4 and 5 may run in either order (publish then commit, or commit/merge then publish from `main`) — the gate finds the predecessor by walking git history either way.

### The migration gate

`scripts/check-migration.sh` is the **only** implementation of the gate — do not add a second copy. (`Makefile.toml` used to carry an inline near-duplicate that the publish path actually ran while the script never fired; delta#45/#46.) It runs from `cargo make publish-delta` (via `preflight`) and from CI's migration-safety job, and for each of delegate/contract refuses success unless:

- the committed WASM is byte-identical to a from-source rebuild;
- **every** committed generation other than the one shipping now has its hash in the matching `legacy_*.toml`; and
- conversely every recorded hash is visible in that WASM's git history (catches a renamed/relocated WASM path re-baselining the gate to one generation).

It also refuses whenever it cannot answer the question (shallow clone, untracked WASM, unborn HEAD, orphan branch, non-git checkout) rather than reading silence as safety — this is why CI sets `fetch-depth: 0`.

It does **not** verify that a recorded entry is correct, or that the migration actually restores data — a wrong `delegate_key` for a right `code_hash` is caught by the `ui/build.rs` assertion instead, and that the sweep actually reaches the data is only proven by the browser check (step 9 of the rustc-bump procedure).

## Stable identity: pointer records

`legacy_*.toml` carries **our own users'** data across a re-key; it does nothing for a **third party** whose reference to our contract/delegate key is a build-time constant that goes stale silently (a stale key just looks like "this user has nothing stored"). `pointer-records.toml` is the other half: a record at a fixed address naming each artifact's current code hash, signed by Delta's author key, which integrators resolve instead of pinning.

CI's `pointer-freshness` job fails the PR if a pointed-at WASM changed and no new record was signed — so whenever you run `add-migration.sh`, also run `sign-pointer-records.sh`.

See `FREENET.md` for the integrator-facing side, including the scope boundary: **a pointer solves addressing only**, and says nothing about whether secrets survived the re-key.

## Publishing

```bash
# Full build + publish. Runs the migration gate via `preflight` (aborts before
# building/uploading if a predecessor hash is missing) and refuses to publish
# a bundle that isn't self-consistent.
cargo make publish-delta
```

**Do not assemble a publish by hand** — there are two gates on this path and a hand-assembled publish skips both:

- `scripts/check-migration.sh` (via `preflight`)
- `scripts/check-webapp-bundle.sh` (inside `bundle-webapp`, after the tar) — refuses an archive carrying stale copies from earlier builds. `dx` writes content-hashed filenames, so without this the bundle grew ~2MB of stale wasm and shipped several app builds at once (delta#70).

```bash
cargo make bundle-webapp     # -> target/webapp/webapp.tar.xz, gated, no publish
```

**Version counter**: `published-contract/contract-version.txt` is the source of truth for the web-container version. `cargo make sign-webapp` (run by `publish-delta`) reads, bumps, and writes it back each publish. Do not derive the version from wall-clock time (delta#71). Commit the bumped counter alongside other publish artifacts, **and open a pull request for all changes before pushing** - `main` has no branch protection here, so nothing enforces this but the convention.

Contract ID: `EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr`

## Gateway Iframe Constraints

Delta runs inside the Freenet gateway's sandboxed iframe:
- `sandbox="allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox"` (NO allow-same-origin)
- **No Clipboard API** - use `document.execCommand('copy')` via textarea
- **No autofocus** - blocked in cross-origin subframes
- **No `fixed` positioning** - use `absolute` with inline styles
- **No `window.location.set_hash`** - use `history.replaceState`
- **Tailwind group-hover** - doesn't work reliably, use plain CSS `.parent:hover .child`
- **Hash forwarding**: shell sends `__freenet_shell__` postMessage with `type: 'hash'`; Delta listens and navigates

## Testing

No dedicated remote browser-test rig exists for this repo. The old instructions here (SSH to `technic`) are dead — technic died in a hardware failure on 2026-06-27, and there is no replacement host with pre-seeded test sites. For ad hoc browser testing against a locally- or gateway-served Delta instance, use the general-purpose `playwright-skill` (auto-detects dev servers / takes a URL) rather than any repo-specific script.
