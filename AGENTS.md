# Delta - Agent Guide

## Repository Structure

```
delta/
├── common/           # delta-core: shared state types, crypto, serialization
├── contracts/
│   └── site-contract/  # Freenet contract: validates state, handles updates
├── delegates/
│   └── site-delegate/  # Local agent: stores signing keys, signs pages
├── ui/               # Dioxus web UI (compiled to WASM)
├── published-contract/ # Committed web container WASM + params
├── legacy_delegates.toml  # Migration entries for delegate WASM changes
├── legacy_contracts.toml  # Migration entries for contract WASM changes
├── scripts/          # add-migration.sh, add-contract-migration.sh, sync-wasm.sh
└── Makefile.toml     # Build tasks
```

## Key Concepts

### Site Identity

A site is identified by a 10-character **prefix** derived from the owner's Ed25519 public key: `base58(pubkey)[..10]`. This prefix IS the contract parameters. The full contract key is `BLAKE3(BLAKE3(site_contract.wasm) || CBOR({prefix}))`.

Anyone who knows the prefix can compute the contract key because the WASM is public.

### State Design

```
SiteState {
    owner: VerifyingKey,           # Owner's public key
    config: SignedConfig,          # Site name/description (signed)
    pages: BTreeMap<PageId, Page>, # All pages
    next_page_id: PageId,          # Monotonic counter
    deleted_pages: BTreeMap<PageId, SignedPageDeletion>,  # Tombstones
}
```

All content is signed by the owner. Pages have stable IDs that don't change on rename.

### CRITICAL: All State Fields Must Be Authenticated

**Every field in the contract state MUST be covered by a cryptographic signature.** Freenet contracts run on untrusted peers who can modify state. The contract validates signatures, but only for fields included in the signing bytes. An unsigned field is world-writable.

When adding a new field to any signed struct (Page, SignedConfig, SignedPageDeletion):
1. Include it in the signing bytes immediately
2. If backwards compatibility is needed, use a versioned signature (try v2 first, fall back to v1)
3. Add a test verifying the new field is covered by the signature

Page signatures use v2 format (`delta:page:v2:`) which covers: page_id, title, content, updated_at, order. V1 fallback (without order) exists for pre-existing pages.

**`updated_at` must be strictly greater than the page's current `updated_at`.** `apply_delta` and `merge` in `delta-core` dominate equal timestamps with `>=`, so an UPDATE whose `updated_at` matches what's already in state is silently dropped on the network. Any UI path that produces a page UPDATE (`save_current_page`, `rename_page`, `swap_page_order`, …) MUST route through `next_page_updated_at` in `ui/src/state.rs`, which computes `max(now_secs(), existing + 1)`. Calling `now_secs()` directly is a recurrence of the reorder bug Ivvor reported on 2026-04-29 (silent same-second collisions).

**Page `order` invariants (for `swap_page_order` / `create_page` in `ui/src/state.rs`):**

1. `swap_page_order` MUST sign and propagate a fresh page-UPDATE for **every** page whose order changes — not just the two pages clicked. When ANY page on the site is still at `order == 0` (legacy v1-signed pages, or pages created before the order field existed), the swap also performs a one-time site-wide migration to explicit orders `(10, 20, 30, …)` sorted by `(current_order, page_id)` to match the sidebar. Skipping the propagation step is the bug Ivvor re-reported on 2026-05-03 — the local view looked correct but unmigrated pages remained at `order = 0` on the network and clumped to the front of the sidebar after a refresh.

2. `create_page` MUST assign `order = max(existing) + ORDER_STEP` via `next_create_order` (never `0`). Issuing `0` re-poisons a migrated site and re-introduces the front-of-sidebar clumping symptom.

3. The orchestration helper `plan_swap` derives `pages_to_sign` from the diff between current and new orders, so a regression that re-narrows the sign set (e.g. back to "just the two clicked pages") is caught by the unit tests — not just by the lower-level `compute_swap_orders` tests.

**Delegate-response routing MUST use signature verification, not `CURRENT_SITE`.** `handle_signed_page` / `handle_signed_deletion` / `handle_signed_config` in `ui/src/freenet_api/delegate.rs` look up the owning site by checking the signature against every known owner's pubkey (`find_owner_for_signed_*`). Earlier code keyed `PENDING_UPDATES` by `(CURRENT_SITE, page_id)` and consumed the entry on first response; concurrent requests for the same page silently dropped subsequent UPDATEs, and a mid-flight site switch routed a signed page into the wrong site's local state. Verification-based routing handles both correctly without a delegate WASM change.

### Page Links

- `[[2]]` - renders as current page title, auto-updates on rename
- `[[2|custom text]]` - renders as "custom text", never changes
- `[[Page Title]]` - title lookup, renders as title

Autocomplete inserts `[[id]]` format.

### Delegate Storage

The delegate stores:
- **Signing keys**: `delta:signing_key:{prefix}` - per-site Ed25519 private keys (legacy: `delta:signing_key`)
- **Known sites**: `delta:known_sites` - list of sites with prefix, name, role, contract key
- **Site state backups**: `delta:site_state:{prefix}` - full state backup for network resilience

### Known-Sites Tombstone Convention

The `delta:known_sites` `Vec<KnownSiteRecord>` doubles as the persistent
tombstone store for removed sites. A record with
`name == TOMBSTONE_NAME_SENTINEL` (`"\0__delta_removed__"`, defined in
`common/src/state.rs`) is a tombstone, not a real site. Tombstones exist
to block legacy delegates from resurrecting deleted sites after a page
refresh.

**Every code path that consumes `KnownSites` responses MUST filter
tombstones via `KnownSiteRecord::is_tombstone()`** before iterating.
Forgetting this will leak sentinel entries into the UI and display a
ghost site named `"\0__delta_removed__"`. `restore_known_sites` has a
`debug_assert!` that catches this in debug builds.

Tombstones are cleared by `clear_tombstone(prefix)` in `ui/src/state.rs`,
which must be called by any path that adds a site (`create_new_site`,
`import_site_key`, `visit_site`) — otherwise a previously-removed prefix
would be silently filtered out of `restore_known_sites` and the re-add
would appear to fail.

**Tombstone-application rules** (enforced by
`filter_applicable_tombstones` in `ui/src/freenet_api/delegate.rs`):

1. Once the current delegate has responded (`CURRENT_SITES_LOADED` is
   true), legacy-delegate tombstones are dropped. The current delegate
   is authoritative for the removal set; a stale legacy tombstone must
   not override it. Without this rule, deleting a site and then
   re-visiting it would make the site briefly appear and then vanish
   when a legacy delegate's KnownSites response arrived later.

2. A tombstone whose prefix is currently live in `SITES` is always
   dropped, regardless of source. Live user intent (via `visit_site` /
   `create_new_site` / `import_site_key`) beats any stale removal
   record. This is the guardrail for ordering races between
   `save_known_sites` and a `load_known_sites` response already in
   flight.

Any change to the KnownSites response handler must preserve both rules;
the `filter_applicable_tombstones` unit tests pin them.

**Ordering invariant: the legacy sweep is not started until the current
delegate's KnownSites response has arrived** (`fire_legacy_migration` is
called from that arm, and nowhere else). Both rules above are stated in
terms of "once the current delegate has responded", so a legacy reply
applied first inverts them: the legacy delegate still lists a site the
user removed under the current delegate as a live record, and no
tombstone is known yet to suppress it.

#52 proposed dispatching the sweep at registration instead, to take it
off the critical path, with legacy responses buffered to preserve the
ordering. **Do not do this without new evidence.** Measured on a live
node across 7 re-keys, the gap it removes is 188-355 ms (cold wasmtime
compilation of the freshly re-keyed delegate; a warm reload is 66 ms),
the node answers delegate ops serially so early-dispatched probes queue
behind that same compile, and the ordering rule means their results
cannot be applied any sooner anyway — a measured saving of about 50 ms,
against total time-to-site of well under 1.1 s.

A first attempt also released the buffer on a 2 s deadline, so a slow
current delegate could not hold the sites hostage. Review found that
unsafe: `save_known_sites()` has seven call sites, including one the
contract-migration sweep reaches with no user action at all, and
`StoreKnownSites` is a full overwrite of the delegate's stored record —
so any of them firing inside such a window writes the merged list back
WITHOUT the current delegate's tombstones and permanently resurrects a
removed site. The user-visible half of #52 is handled by
`state::SiteDiscovery` instead, which never touches stored data.

## Reproducible WASM Builds

The repo pins rustc via `rust-toolchain.toml` (currently `1.94.1`). This is **load-bearing for the migration system**: the delegate key is `BLAKE3(BLAKE3(wasm) || params)`, so any change in WASM bytes — including bytes produced by an LLVM upgrade in a newer rustc — produces a new delegate key and orphans every user's stored data unless a migration entry is recorded first.

The migration-safety job in `.github/workflows/ci.yml` runs `scripts/check-migration.sh` on each PR: it rebuilds the WASMs from source and refuses to merge if the committed hashes don't match, and separately refuses if a changed WASM's predecessor hash was never recorded in `legacy_delegates.toml` / `legacy_contracts.toml` (see "The migration gate"). With a pinned toolchain CI and local always agree, so the gate provides real signal.

The same pattern is used in `freenet/river` and `freenet/freenet-core`. Don't let the pin drift past those sibling repos without coordinating, since a Freenet dApp ecosystem with mismatched toolchain pins will silently produce different hashes for shared dependencies.

### Upgrading the pinned rustc

Bumping `channel` in `rust-toolchain.toml` is a **data-migration gesture**, not a routine maintenance task. Treat it the same way you treat changing delegate or contract code: predecessor hashes recorded first, WASM regenerated, single-commit PR, post-merge republish, browser verification. The full canonical procedure lives at the top of `rust-toolchain.toml`; the summary:

1. `rustup install <new-channel>` (and add `rustfmt`, `clippy`, `wasm32-unknown-unknown` to it).
2. `./scripts/add-migration.sh V_N "rustc X.Y.Z -> X.Y.Z+1"` — records the current delegate WASM hash in `legacy_delegates.toml` BEFORE the bump.
3. `./scripts/add-contract-migration.sh C_N "rustc X.Y.Z -> X.Y.Z+1"` — same for the contract WASM.
4. Bump `channel` in `rust-toolchain.toml`.
5. `./scripts/sync-wasm.sh` — rebuilds with the new channel and copies the new bytes into `ui/public/contracts/`. The hash must differ from the one just recorded.
6. `./scripts/check-migration.sh` — must print "Safe to publish."
7. Single commit, single PR: `rust-toolchain.toml`, both `legacy_*.toml`, both `ui/public/contracts/*.wasm`. Reviewers see the whole atomic change in one diff.
8. After merge: `cargo make publish-delta`.
9. Browser verification on the live deploy: existing site still listed, at least one page loads (legacy migration of state backup worked), an edit saves and round-trips (new key functional). **Don't consider the bump done until step 9 passes.**

Skipping any step silently breaks data continuity for every existing user; there is no automatic recovery. The procedure is in the toml file rather than a separate runbook so the person about to run `vim rust-toolchain.toml` has it directly in front of them.

## Contract Upgrade / State Migration

### When Contract WASM Changes

When `site_contract.wasm` changes (code changes, dependency updates, `common/` changes), ALL site contract keys change because `contract_key = BLAKE3(BLAKE3(wasm) || params)`.

**Migration is permissionless** - since all state is signed by the owner, ANY node can:
1. GET state from the old contract key
2. PUT state to the new contract key (with new WASM + same params)

The new contract validates all signatures and accepts the state.

### How Delta Handles WASM Upgrades

1. The delegate stores each site's contract key (base58) in `KnownSiteRecord.contract_key_b58`
2. On startup, the UI computes the fresh contract key from the prefix using the current embedded WASM
3. If stored key != computed key, a WASM upgrade happened
4. GET state from old key, PUT to new key with the new contract container
5. Update the stored key in the delegate

This happens automatically - no user action needed. That is the
single-hop case; the fallback below covers the rest, and both are
folded into one decision by the driver described under it.

**Multi-hop migration fallback:** when a restored `KnownSiteRecord`
has no `contract_key_b58` (legacy delegates from before b82d3bc) or
when the stored key itself refers to a hash no longer on the network,
the UI additionally probes every previous contract WASM hash recorded
in `legacy_contracts.toml`. Every candidate generation is reconciled
into local state by the tombstone-aware `reconcile_into` merge, which
keeps the newest data and preserves deletions regardless of arrival
order. There is no "first response wins" and no drop-late-arrival
behaviour, and legacy sibling probes are deliberately left running
(`ui/src/freenet_api/operations.rs:49-53`, `:213-215`, `:263-267`):
a slower generation may still be the newest one, which is also the
self-heal path for a user whose current key holds stale state from an
earlier broken migration.

**The contract sweep runs on `freenet-migrate`'s decision driver**
(adopted in delta#36). `ProbeDriver` with `SelectionPolicy::FoldAll`
(`operations.rs:928-933`) bounds the sweep into ONE decision and one
forward PUT once every candidate has resolved
(`finalize_migration_sweep`), replacing the earlier re-PUT on every
legacy response that changed local state. The driver validates and
pins the fold while `reconcile_into` keeps ownership of the production
data decisions, so no second merge implementation exists that could
drift from the one the tests pin. The driver header at
`operations.rs:778-800` records two honest bounds on that soundness,
both pre-existing rather than introduced by the adoption.

**Recording contract WASM hashes is part of the release process.**
Any commit that changes `site_contract.wasm` — including an incidental
rebuild caused by touching `common/`, even if the contract's own
source is unchanged — must first record the currently-committed
contract WASM hash via `./scripts/add-contract-migration.sh`.
`scripts/check-migration.sh` enforces this: it walks the git history
of `site_contract.wasm` and refuses to publish unless EVERY committed
state other than the one shipping now appears in
`legacy_contracts.toml`. The delegate is gated identically against
`legacy_delegates.toml`. See "The migration gate" below for where it
runs and what it cannot do.

### Delegate WASM Migration

When `site_delegate.wasm` changes, the delegate key changes and stored secrets (signing keys, known sites, **site state backups**) become inaccessible under the old key.

Migration entries in `legacy_delegates.toml` allow the UI to read from old delegate keys:
1. Before changing delegate code: `./scripts/add-migration.sh VERSION "description"`
2. Rebuild: `./scripts/sync-wasm.sh`
3. Once the current delegate's KnownSites response arrives (see the ordering invariant under "Known-Sites Tombstone Convention"), the UI sends GetPublicKey, GetKnownSites, GetSigningKey to each legacy delegate. **The send order is load-bearing, not cosmetic:** with an empty current delegate the first non-empty legacy reply latches `CURRENT_SITES_LOADED` and calls `save_known_sites()`, after which `skip_older_legacy` discards every remaining generation bar the newest — so send order decides whose view is persisted. Reordering the sweep is a change to stored data and needs its own tests.
4. When legacy KnownSites arrives, GetSiteState is also requested for each prefix from that legacy delegate
5. If an old delegate responds, signing keys, known sites, and site state backups are migrated to the current delegate

**Two mechanisms run side by side, and steps 3-5 describe only the
first.** Since delta#61 Delta runs both:

- The **hand-rolled sweep**, `fire_legacy_migration()`
  (`ui/src/freenet_api/delegate.rs:1388-1453`), is what steps 3-5
  above describe. It owns the UI-state restoration the library does
  not migrate: site list reconciliation, contract GETs and hash-route
  replay. Its send order is load-bearing, as step 3 explains.
- The **crate walk**, `start_delegate_secret_migration()`
  (`delegate.rs:1163`), calls `freenet_migrate::migrate_delegate_secrets`
  through `run_delegate_migration` (`delegate_migration.rs:922`). It
  owns walk order, durable per-predecessor marker bookkeeping,
  never-clobber writes and per-predecessor classification.

Both are armed from the same match arm and nowhere else
(`delegate.rs:873-874`), so both inherit the ordering invariant under
"Known-Sites Tombstone Convention": the current delegate has already
answered, so a predecessor's reply can no longer be applied ahead of
it and resurrect a site the user removed. Neither is behind a feature
flag.

Running both is deliberate staging rather than an oversight. Both
paths are idempotent and never-clobber, so running both is safe;
retiring the hand-rolled secret probing is a follow-up once the walk
is field-validated. The rationale is on the doc comment for
`start_delegate_secret_migration` (`delegate.rs:1150-1162`), and the
`freenet-migrate-adoption` skill covers the call-site swap, the
dual-running period and the parity test.

`ui/src/freenet_api/delegate_migration_differential.rs` is a
**test-only** harness, declared behind `#[cfg(test)]` at
`ui/src/freenet_api.rs:8-9`. It compares a transcribed model of the
shipped sweep against the library-driven walk under `cargo test`. It
is not a runtime check, so do not read it as a third mechanism.

**`legacy_delegates.toml` is baked into the UI at build time** by
`ui/build.rs`, which declares `cargo:rerun-if-changed` on it. That
directive is load-bearing and was once missing: `add-migration.sh` edits
the file without staging it, so without it Cargo reuses the cached build
script output and the bundle ships a STALE migration table — the outgoing
delegate absent from it, the sweep never asking the delegate that holds
the user's data, and every returning user landing on a permanently empty
"Welcome to Delta". The `entry` field is `#[serde(default)]`, so a
structural mismatch yields an EMPTY table with no error at all; a build
assertion now fails the build if the file has `[[entry]]` sections and
none deserialize.

**A worktree is the WRONG instrument for reproducing a build-caching or
publish-path problem.** `ui/build.rs` is only cached as designed in a
normal checkout. In a git worktree `.git` is a FILE pointing at
`.git/worktrees/<name>`, so a hardcoded `../.git/HEAD` does not resolve,
and Cargo treats an unresolvable `rerun-if-changed` target as
permanently dirty — the script re-ran on every build and every generated
table was always fresh. The paths are now resolved via `git rev-parse
--git-path` so both layouts cache identically, but the general point
outlives that fix: `cargo make publish-delta` runs from the main
checkout, so any staleness bug lives there. The repo's own convention of
always working in a worktree structurally concealed the missing
`legacy_delegates.toml` directive above from every agent who
investigated it. If you are asked to reproduce something about build
caching, generated files, or publishing, reproduce it in a clean clone
or the main checkout — following the worktree convention guarantees you
cannot see it.

**CRITICAL: Every delegate storage key type must be migrated.** The
legacy migration in `fire_legacy_migration()` and the KnownSites handler
must cover ALL storage operations the delegate supports. If a new storage
operation is added to the delegate (e.g. `StoreFoo` / `GetFoo`), the
corresponding `GetFoo` MUST be added to the legacy migration path.
Omitting it means that data is lost silently when the delegate WASM
upgrades. April 2026 incident: `GetSiteState` was missing from legacy
migration, causing sites to vanish when network state had been GC'd.

**Defense in depth:** `request_site_state_backup()` (called from the
NotFound handler) queries both the current delegate AND all legacy
delegates, so even if the proactive fetch during KnownSites processing
misses a prefix, the NotFound fallback catches it.

### Upgrade Workflow

```bash
# 1. Record old delegate + contract WASM hashes (BEFORE any code change).
#    Run whichever applies — delegate-only changes don't need the
#    contract entry, and vice-versa. Changes to `common/` touch BOTH
#    WASMs because the contract and delegate both depend on delta-core.
./scripts/add-migration.sh V2 "Before adding deleted_pages field"
./scripts/add-contract-migration.sh C3 "Before adding deleted_pages field"

# 2. Make code changes

# 3. Rebuild WASMs
./scripts/sync-wasm.sh

# 4. Build and publish. `publish-delta` depends on `preflight`, which runs
#    scripts/check-migration.sh and aborts the whole chain — before anything
#    is signed or published — if either WASM's predecessor hash is missing.
cargo make publish-delta

# 5. Commit everything
git add legacy_delegates.toml legacy_contracts.toml ui/public/contracts/ common/ contracts/
git commit -m "fix: description with delegate migration"
git push
```

Steps 4 and 5 may also be done in the other order (commit and merge first,
publish from `main` afterwards, as the rustc-bump procedure does). The gate
finds the predecessor by walking git history, so it works either way.

### The migration gate

`scripts/check-migration.sh` is the **only** implementation of the gate. It
runs from `cargo make publish-delta` (via `preflight` → `check-migration`) and
from the "Delegate migration safety" job in `.github/workflows/ci.yml`. For
each of the delegate and the contract it refuses to report success unless:

- the committed WASM is byte-identical to what this toolchain builds from
  source;
- **every** committed generation of that WASM other than the one shipping now
  has its hash recorded in the matching `legacy_*.toml`; and
- conversely, every hash the table records is visible in that WASM's git
  history.

It checks every generation rather than only the immediate predecessor because
single-generation checking is sound only if every earlier generation was
itself checked when it shipped — which assumes a gate that always worked and
a branch protection rule that was always on. Neither held: three April 2026
contract generations were unrecorded on `main`, and a predecessor-only gate
is structurally incapable of noticing them. The converse rule is what catches
a **renamed or relocated** WASM path, where `git log -- <new path>` reports a
single commit and `--follow` does not bridge a rename whose content changed
in the same commit; without it, relocating `ui/public/contracts/` silently
re-baselines the gate to one generation and passes.

It also refuses whenever it cannot answer the question rather than reading
silence as safety: a shallow clone, an untracked WASM, an unborn HEAD, an
orphan branch, a non-git checkout, or an unreadable git object. This is why
the CI job sets `fetch-depth: 0`.

**Do not add a second copy of this check.** `Makefile.toml` used to carry an
inline near-duplicate that only compared the committed WASM against a fresh
build. That inline copy, not the script, was what the publish path actually
ran, so the gate the docs promised had never once refused a publish
(delta#45, delta#46). `scripts/tests/check-migration-test.sh` runs in CI and
in `preflight`, ahead of the gate itself, in three layers: source scrapes that
the Makefile task delegates to the script and that CI invokes it with full
history (the *wiring*, which is what was actually broken); end-to-end runs of
the real script against synthetic repos with a stubbed `build-wasm.sh`, so
deleting a check from `main()` fails the suite; and unit cases for each
history shape. An earlier version tested only the helper function and passed
11/11 with the gate deleted from `main()` entirely — if you add a case, make
sure it fails when the thing it describes is removed.

What the gate does **not** cover: it verifies that a hash is recorded, not
that the recorded entry is correct or that the migration actually restores
data. A wrong `delegate_key` for a right `code_hash` is caught separately by
the build assertion in `ui/build.rs`; that the sweep reaches the data at all
is only ever proven by the browser check (step 9 of the rustc-bump
procedure). It also cannot see a state that was published but never
committed.

## Publishing

```bash
# Full build + publish. This is the supported route: it runs the migration
# gate via `preflight` and aborts before anything is built or uploaded if a
# predecessor hash is missing, and it refuses to publish a bundle that is not
# self-consistent.
cargo make publish-delta
```

**Do not assemble a publish by hand.** There are now TWO gates on this path,
and a hand-assembled publish skips both:

- `scripts/check-migration.sh` (via `preflight`) — the only thing standing
  between a WASM change and every returning user losing their sites.
- `scripts/check-webapp-bundle.sh` (inside `bundle-webapp`, after the tar) —
  refuses an archive carrying stale copies from earlier builds. `dx` writes
  content-hashed filenames, so without the clean-and-check the bundle grew a
  full ~2MB wasm on every publish and shipped several builds of the app at
  once (delta#70).

An earlier version of this section listed raw `dx build` / tar / sign /
`fdev publish` steps, which documented exactly that gate-free route. If you
need the archive without publishing, build it with the task — it runs both
gates and stops at the tar:

```bash
cargo make bundle-webapp     # -> target/webapp/webapp.tar.xz, gated, no publish
```

Do not sign or publish an archive produced any other way.

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

## People

- **Ian Clarke** - project lead. GitHub: sanity

## Testing

Run Playwright tests via SSH on technic:
```bash
scp test.mjs technic:/tmp/
ssh technic "cd /tmp && npm install playwright && node test.mjs"
```

Technic has one owned site and several visited sites for testing.
