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

This happens automatically - no user action needed.

**Multi-hop migration fallback:** when a restored `KnownSiteRecord`
has no `contract_key_b58` (legacy delegates from before b82d3bc) or
when the stored key itself refers to a hash no longer on the network,
the UI additionally probes every previous contract WASM hash recorded
in `legacy_contracts.toml`. The first GET that returns non-empty
state wins, and concurrent probes for the same prefix are cancelled
so a slower response from an older hash can't overwrite fresh state.

**Recording contract WASM hashes is part of the release process.**
Any commit that changes `site_contract.wasm` — including an incidental
rebuild caused by touching `common/`, even if the contract's own
source is unchanged — must first record the currently-committed
contract WASM hash via `./scripts/add-contract-migration.sh`. The
`check-migration.sh` preflight script enforces this by comparing the
previous git-tracked hash against the entries in
`legacy_contracts.toml` and refusing to publish if the predecessor
is missing.

### Delegate WASM Migration

When `site_delegate.wasm` changes, the delegate key changes and stored secrets (signing keys, known sites) become inaccessible under the old key.

Migration entries in `legacy_delegates.toml` allow the UI to read from old delegate keys:
1. Before changing delegate code: `./scripts/add-migration.sh VERSION "description"`
2. Rebuild: `./scripts/sync-wasm.sh`
3. On startup, the UI sends GetPublicKey to each legacy delegate key
4. If an old delegate responds, the signing key is migrated to the current delegate

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

# 4. Build and publish (preflight check-migration.sh runs automatically
#    and refuses to publish if a previous hash was not recorded)
cargo make publish-delta

# 5. Commit everything
git add legacy_delegates.toml legacy_contracts.toml ui/public/contracts/ common/ contracts/
git commit -m "fix: description with delegate migration"
git push
```

## Publishing

```bash
# Full build + publish
cargo make publish-delta

# Or manual steps:
cd ui && npm run build:css && dx build --release
# Copy CSS, tar, sign, fdev publish (see Makefile.toml)
```

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
