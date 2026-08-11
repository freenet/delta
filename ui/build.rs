use chrono::Utc;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;

/// `deny_unknown_fields` is load-bearing, not tidiness. `entry` carries
/// `#[serde(default)]`, so without it a file whose section was renamed (say
/// `[[entries]]`) deserializes to an EMPTY list and reports no error at all.
/// With it, the unknown top-level table is a hard deserialization failure that
/// names the offending key.
///
/// `LegacyEntry` deliberately does NOT need this: none of its fields have
/// defaults, so a missing or misspelled one already fails loudly.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDelegates {
    #[serde(default)]
    entry: Vec<LegacyEntry>,
}

#[derive(Deserialize)]
struct LegacyEntry {
    version: String,
    description: String,
    date: String,
    delegate_key: String,
    code_hash: String,
}

/// `deny_unknown_fields` for the same reason as [`LegacyDelegates`]: `entry` is
/// `#[serde(default)]`, so a renamed section would otherwise deserialize to an
/// empty list. The emptiness assert does catch that, but reports it as "zero
/// entries" and points the reader at the file's contents rather than at the
/// renamed key, which is the actual fault.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyContracts {
    #[serde(default)]
    entry: Vec<LegacyContractEntry>,
}

#[derive(Deserialize)]
struct LegacyContractEntry {
    version: String,
    description: String,
    date: String,
    code_hash: String,
}

fn main() {
    generate_build_info();
    generate_legacy_delegates();
    generate_legacy_contracts();
}

fn generate_build_info() {
    // Without these rerun-if-changed directives, Cargo caches the
    // build script output and the baked-in timestamp/commit drift
    // from what's actually deployed. A stale timestamp masks whether
    // the published webapp contains recent fixes. See AGENTS.md
    // "Publishing" and the April 2026 incident in delta/#9 follow-up.
    println!("cargo:rerun-if-changed=build.rs");
    emit_git_ref_rerun_paths();
    println!("cargo:rerun-if-env-changed=FORCE_REBUILD_TIMESTAMP");

    let now = Utc::now();
    println!(
        "cargo:rustc-env=BUILD_TIMESTAMP_ISO={}",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );

    // Check the exit status, not just that the process spawned. Building from
    // a source tarball WITH git installed spawns fine, exits non-zero, and
    // writes nothing to stdout — so without this, `GIT_COMMIT` is `""` rather
    // than `"unknown"`, and an empty commit in the footer reads as a bug in the
    // fingerprint rather than as "not built from a repo".
    let git_hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT={git_hash}");
}

/// Emit `rerun-if-changed` for the two git files that decide `GIT_COMMIT`:
/// `HEAD` (which commit / branch) and `index` (staging, which moves on commit).
///
/// These were hardcoded as `../.git/HEAD` and `../.git/index`, which is only
/// correct in a normal checkout. **In a git worktree `.git` is a FILE** holding
/// `gitdir: /path/to/repo/.git/worktrees/<name>`, so those paths do not resolve
/// at all — and Cargo treats an unresolvable `rerun-if-changed` target as
/// permanently dirty. The build script therefore re-ran on every single build
/// in a worktree, and cached as designed only in the main checkout.
///
/// That asymmetry is worth more attention than the wasted CPU, because it made
/// a data-loss bug invisible from the standard vantage point. The repo's own
/// convention is to work in worktrees, where the script never caches and the
/// generated migration tables are always fresh. The one place caching was live
/// is the main checkout, which is exactly where `cargo make publish-delta`
/// runs. A missing `rerun-if-changed` on `legacy_delegates.toml` (fixed in the
/// PR below this one) could therefore ship a stale migration table to users
/// while being impossible to reproduce for anyone following the convention.
///
/// `git rev-parse --git-path` resolves correctly in both layouts: it returns
/// `../.git/HEAD` in a normal checkout and the absolute per-worktree path
/// inside `.git/worktrees/<name>/` otherwise.
fn emit_git_ref_rerun_paths() {
    for spec in ["HEAD", "index"] {
        if let Some(path) = git_ref_path(spec) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Resolve a git metadata file, or `None` if it cannot be resolved.
///
/// Returning `None` (and so emitting no directive) is deliberate for the
/// no-git case, e.g. building from a source tarball. Emitting a path that does
/// not exist is precisely the bug this replaces, and it degrades to the
/// pre-existing cosmetic issue: `GIT_COMMIT` is already `"unknown"` whenever git
/// cannot resolve HEAD — whether the binary is absent or it runs outside a
/// repository — so there is nothing for a stale fingerprint to misreport.
fn git_ref_path(spec: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    // The existence check is the actual guard. `--git-path` is documented to
    // return a path whether or not the file is present, so a repo state
    // without one of these (e.g. no index yet) would otherwise re-introduce
    // the always-dirty behaviour this function exists to remove.
    if path.is_empty() || !Path::new(&path).exists() {
        return None;
    }
    Some(path)
}

fn generate_legacy_delegates() {
    let toml_path = Path::new("..").join("legacy_delegates.toml");
    // Load-bearing, and it was missing while the equivalent line in
    // `generate_legacy_contracts` was present. Without it, Cargo re-runs this
    // script only when build.rs or the git HEAD/index changes — and
    // `add-migration.sh` edits this file WITHOUT staging it, so the very act
    // that records a new migration entry does not invalidate the build script.
    // The bundle then ships a STALE legacy-delegate table: the outgoing
    // delegate is missing from it, the sweep never asks the delegate that
    // holds the user's data, and every returning user gets a permanently
    // empty "Welcome to Delta" screen. That is a far better fit for the
    // "minutes, or never" recovery reported in #52 than any latency in the
    // startup sequence — a bundle built this way was observed doing exactly
    // that, with `LEGACY_DELEGATES` baked in as an empty list.
    println!("cargo:rerun-if-changed=../legacy_delegates.toml");

    // An unreadable file used to warn and emit an empty table. `cargo:warning`
    // is not a gate — it scrolls past in a release build — and the outcome is
    // identical to every other route to an empty table: the sweep asks nobody
    // and every returning user is orphaned. Fail instead.
    let toml_content = fs::read_to_string(&toml_path).unwrap_or_else(|e| {
        panic!(
            "legacy_delegates.toml could not be read ({e}). The migration table \
             would ship EMPTY, the startup sweep would ask no delegate at all, \
             and every returning user's sites would be unrecoverable."
        )
    });

    let parsed: LegacyDelegates =
        toml::from_str(&toml_content).expect("Failed to parse legacy_delegates.toml");

    // This registry is append-only and can never legitimately be empty: Delta
    // re-keys its delegate on essentially every release, so predecessors always
    // exist and must always be retained. An empty table is therefore
    // unconditionally wrong, whatever produced it, and shipping one is silent
    // total migration failure — the sweep asks nobody and every returning user
    // gets a permanently empty "Welcome to Delta".
    //
    // Assert on emptiness ITSELF. Do NOT weaken this to a substring check such
    // as `is_empty() && content.contains("[[entry]]")`: that form passes for
    // the two cases most likely to arise in practice — a renamed section
    // (`[[entries]]`, where the substring is simply absent) and a truncated or
    // emptied file — which is to say it permits precisely what it purports to
    // forbid. Verified: with that form, renaming the section built cleanly and
    // emitted `LEGACY_DELEGATES = &[]`.
    assert!(
        !parsed.entry.is_empty(),
        "legacy_delegates.toml produced ZERO entries. The migration table would \
         ship EMPTY, the startup sweep would ask no delegate at all, and every \
         returning user's sites would be unrecoverable. This registry is \
         append-only and must never be empty — check the file against \
         `struct LegacyEntry` (a renamed or restructured section deserializes \
         to nothing)."
    );

    let mut code = String::new();
    code.push_str(
        "// AUTO-GENERATED from legacy_delegates.toml — do not edit.\n\n\
         pub const LEGACY_DELEGATES: &[([u8; 32], [u8; 32])] = &[\n",
    );

    for entry in &parsed.entry {
        let dk_bytes = hex_to_byte_array(&entry.delegate_key, &entry.version, "delegate_key");
        let ch_bytes = hex_to_byte_array(&entry.code_hash, &entry.version, "code_hash");

        // Cross-check the relationship documented at the top of
        // legacy_delegates.toml: `delegate_key = BLAKE3(code_hash_bytes)`.
        // Catches typos / hand-computed-wrong / a future drift in the
        // freenet-stdlib formula at build time, before the UI ships.
        let computed_dk: [u8; 32] = *blake3::hash(&ch_bytes).as_bytes();
        assert_eq!(
            dk_bytes, computed_dk,
            "{}: delegate_key in legacy_delegates.toml does not match BLAKE3(code_hash). \
             stored = {}, computed = {}. Did the formula change in freenet-stdlib, or is one of the fields a typo?",
            entry.version,
            bytes_to_hex(&dk_bytes),
            bytes_to_hex(&computed_dk),
        );

        code.push_str(&format!(
            "    // {}: {} ({})\n",
            entry.version, entry.description, entry.date
        ));
        code.push_str("    (\n");
        code.push_str(&format_byte_array(&dk_bytes, "        "));
        code.push_str(&format_byte_array(&ch_bytes, "        "));
        code.push_str("    ),\n");
    }

    code.push_str("];\n");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("legacy_delegates.rs");
    let existing = fs::read_to_string(&dest).unwrap_or_default();
    if existing != code {
        fs::write(&dest, &code).unwrap();
    }
}

fn generate_legacy_contracts() {
    let toml_path = Path::new("..").join("legacy_contracts.toml");
    println!("cargo:rerun-if-changed=../legacy_contracts.toml");

    // Same reasoning as `generate_legacy_delegates`: `cargo:warning` is not a
    // gate, and it is trivially lost in `dx build --release` output. An empty
    // contract table silently disables the multi-hop state-migration fallback.
    let toml_content = fs::read_to_string(&toml_path).unwrap_or_else(|e| {
        panic!(
            "legacy_contracts.toml could not be read ({e}). The contract \
             migration table would ship EMPTY and older generations of site \
             state would silently stop being found."
        )
    });

    let parsed: LegacyContracts =
        toml::from_str(&toml_content).expect("Failed to parse legacy_contracts.toml");

    // This registry is append-only and can never legitimately be empty.
    assert!(
        !parsed.entry.is_empty(),
        "legacy_contracts.toml produced ZERO entries. The contract migration \
         table would ship EMPTY and the multi-hop state-migration fallback \
         would silently stop finding older generations. Check the file against \
         `struct LegacyContractEntry`."
    );

    let mut code = String::new();
    code.push_str(
        "// AUTO-GENERATED from legacy_contracts.toml — do not edit.\n\n\
         pub const LEGACY_CONTRACT_HASHES: &[[u8; 32]] = &[\n",
    );

    for entry in &parsed.entry {
        let ch_bytes = hex_to_byte_array(&entry.code_hash, &entry.version, "code_hash");
        code.push_str(&format!(
            "    // {}: {} ({})\n",
            entry.version, entry.description, entry.date
        ));
        code.push_str(&format_byte_array(&ch_bytes, "    "));
    }

    code.push_str("];\n");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("legacy_contracts.rs");
    let existing = fs::read_to_string(&dest).unwrap_or_default();
    if existing != code {
        fs::write(&dest, &code).unwrap();
    }
}

fn bytes_to_hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_to_byte_array(hex: &str, version: &str, field: &str) -> [u8; 32] {
    assert!(
        hex.len() == 64,
        "{version} {field} hex has {} chars, expected 64",
        hex.len()
    );
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    arr
}

fn format_byte_array(arr: &[u8; 32], indent: &str) -> String {
    let mut s = format!("{indent}[\n");
    for chunk in arr.chunks(10) {
        s.push_str(&format!("{indent}    "));
        for (i, b) in chunk.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("{b}"));
        }
        s.push_str(",\n");
    }
    s.push_str(&format!("{indent}],\n"));
    s
}
