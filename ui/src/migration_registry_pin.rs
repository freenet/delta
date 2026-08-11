//! Pins for the build-time legacy-delegate registry.
//!
//! `ui/build.rs` turns `legacy_delegates.toml` into the `LEGACY_DELEGATES`
//! constant that the startup sweep uses to find the delegate holding a
//! returning user's signing keys and site list. If that table ships stale or
//! empty, the sweep asks the wrong delegates (or nobody), and the user gets a
//! permanently empty "Welcome to Delta" screen with no error anywhere — the
//! failure mode behind freenet/delta#52.
//!
//! Two distinct pins live here, and they fail under different conditions:
//!
//! 1. [`the_baked_registry_matches_the_file_on_disk`] is a real consistency
//!    check, not a source pin. It compares the constant Cargo baked in against
//!    the file read via `include_str!`. `include_str!` lands in rustc's
//!    dep-info, so the test's copy is ALWAYS current; the constant is only
//!    current if the build script actually re-ran. That asymmetry is the whole
//!    point — it is what goes red on a stale INCREMENTAL build.
//!
//!    Honest limitation: a cold build (CI, a fresh clone) always runs the build
//!    script, so there this test cannot be stale and cannot fail. Its
//!    discriminating power is on incremental local builds, which is exactly
//!    where the bug bites — a developer edits the registry and runs
//!    `cargo make publish-delta` without a clean rebuild.
//!
//! 2. [`every_toml_the_build_script_reads_is_declared_to_cargo`] covers the
//!    cold-build case. It IS a source pin — it scrapes `ui/build.rs` text — and
//!    is labelled as one rather than dressed up as behaviour. It closes the
//!    class instead of the instance: any future file the build script reads
//!    must also be declared, or this fails.

use crate::freenet_api::delegate::LEGACY_DELEGATES;
use std::collections::BTreeSet;

/// The registry file, read at compile time of the TEST binary.
///
/// Deliberately not routed through the `toml` crate: this is the independent
/// oracle for what the build script produced, so it must not share the build
/// script's parser. A structural change that makes `#[serde(default)] entry`
/// silently deserialize to an empty list would be invisible to a `toml`-based
/// check (both sides would agree on "empty") but is caught here.
const REGISTRY_TOML: &str = include_str!("../../legacy_delegates.toml");

/// The build script's own source, for the source pin below.
const BUILD_RS: &str = include_str!("../build.rs");

/// Extract `(delegate_key, code_hash)` pairs by walking the file line by line.
fn entries_in_registry_file() -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut current: Option<(Option<String>, Option<String>)> = None;

    let flush = |slot: &mut Option<(Option<String>, Option<String>)>,
                 out: &mut Vec<(String, String)>| {
        if let Some((Some(dk), Some(ch))) = slot.take() {
            out.push((dk, ch));
        }
    };

    for raw in REGISTRY_TOML.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        if line == "[[entry]]" {
            flush(&mut current, &mut entries);
            current = Some((None, None));
            continue;
        }
        let Some(slot) = current.as_mut() else {
            continue;
        };
        if let Some(v) = quoted_value(line, "delegate_key") {
            slot.0 = Some(v);
        } else if let Some(v) = quoted_value(line, "code_hash") {
            slot.1 = Some(v);
        }
    }
    flush(&mut current, &mut entries);
    entries
}

/// Pull `value` out of a `key = "value"` line, if the line is that key.
fn quoted_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

#[test]
fn the_baked_registry_matches_the_file_on_disk() {
    let from_file = entries_in_registry_file();

    // The parser is itself a thing that can silently break. If it stops
    // finding entries, every comparison below becomes vacuous, so assert it
    // found something before trusting it.
    assert!(
        from_file.len() >= 2,
        "parsed only {} entries out of legacy_delegates.toml — the test's own \
         parser has broken, so this pin is not checking anything. Fix the \
         parser before trusting a green run.",
        from_file.len()
    );

    let baked: Vec<(String, String)> = LEGACY_DELEGATES
        .iter()
        .map(|(dk, ch)| (hex::encode(dk), hex::encode(ch)))
        .collect();

    assert_eq!(
        baked,
        from_file,
        "The LEGACY_DELEGATES table compiled into this binary does not match \
         legacy_delegates.toml on disk ({} baked vs {} in the file).\n\n\
         This almost certainly means ui/build.rs did not re-run after the file \
         changed, so the bundle is carrying a STALE migration table. A bundle \
         shipped in this state cannot find the delegate holding a returning \
         user's signing keys: they get an empty \"Welcome to Delta\" screen and \
         their sites never appear, with no error logged anywhere.\n\n\
         Check that generate_legacy_delegates() in ui/build.rs still declares \
         `cargo:rerun-if-changed=../legacy_delegates.toml`. See freenet/delta#52.",
        baked.len(),
        from_file.len()
    );
}

#[test]
fn every_toml_the_build_script_reads_is_declared_to_cargo() {
    // SOURCE PIN (scrapes ui/build.rs text, does not execute it).
    //
    // Cargo's rule: once a build script prints ANY `rerun-if-changed`, the
    // implicit "re-run when anything changes" fallback is switched off and
    // ONLY the declared paths are watched. So a file the script reads via
    // fs::read_to_string but never declares is invisible to Cargo — editing it
    // does not invalidate anything, and the generated table silently rots.
    let mut referenced: BTreeSet<&str> = BTreeSet::new();
    for (idx, _) in BUILD_RS.match_indices('"') {
        let rest = &BUILD_RS[idx + 1..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let literal = &rest[..end];
        if !literal.ends_with(".toml") {
            continue;
        }
        // Take the last path/whitespace-separated token so prose literals like
        // "Failed to parse legacy_delegates.toml" resolve to the file name.
        if let Some(name) = literal.split(['/', ' ']).next_back() {
            referenced.insert(name);
        }
    }

    assert!(
        referenced.len() >= 2,
        "found {} .toml file(s) referenced in ui/build.rs; expected at least 2 \
         (legacy_delegates.toml and legacy_contracts.toml). The scrape pattern \
         has stopped matching, so this pin would pass no matter what the build \
         script does. Fix the scrape.",
        referenced.len()
    );

    for name in &referenced {
        let needle = format!("cargo:rerun-if-changed=../{name}");
        // Require the directive in an actual println!, so a build script that
        // merely mentions the path in a comment does not satisfy the pin.
        //
        // This must anchor at the START of the trimmed line. `contains`
        // was satisfied by the single input the clause above exists to
        // reject -- a commented-out directive holds both `println!` and the
        // needle, so the pin stayed green while the directive was dead, and
        // the resulting staleness only surfaced later, from an unrelated
        // commit. A guard defeated by its own stated counter-example is the
        // shape this whole PR is about.
        let declared = BUILD_RS
            .lines()
            .any(|l| l.trim_start().starts_with("println!") && l.contains(&needle));
        assert!(
            declared,
            "ui/build.rs reads {name} but never prints \
             `{needle}`.\n\n\
             Cargo therefore will not re-run the build script when {name} \
             changes, and whatever the script bakes in from that file ships \
             stale. For legacy_delegates.toml this silently breaks delegate \
             migration for every returning user (freenet/delta#52).",
        );
    }
}
