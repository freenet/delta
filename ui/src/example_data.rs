use delta_core::{Page, SignedConfig, SiteConfig, SiteState};
use ed25519_dalek::Signature;
use std::collections::BTreeMap;

/// Create an example site with sample pages for demo/testing.
pub fn create_example_site() -> SiteState {
    let mut pages = BTreeMap::new();
    let placeholder_sig = Signature::from_bytes(&[0u8; 64]);
    let now = chrono::Utc::now().timestamp() as u64;

    pages.insert(
        1,
        Page {
            title: "Home".into(),
            content: r#"# Welcome to Delta

Delta is a decentralized website builder running on [Freenet](https://freenet.org).

## Features

- **Decentralized** — your site lives on the Freenet network
- **Markdown** — write pages in markdown, rendered beautifully
- **Signed** — all content is cryptographically signed by the site owner
- **Live updates** — subscribers see changes in real-time

## Getting Started

Check out the [[2|About]] page to learn more, or read the [[3|Markdown Guide]] for formatting help.
"#
            .into(),
            updated_at: now,
            signature: placeholder_sig,
        },
    );

    pages.insert(
        2,
        Page {
            title: "About".into(),
            content: r#"# About Delta

Delta is built on Freenet's decentralized key-value store. Each site is a **contract** whose state contains all its pages.

## How it works

1. The site owner creates a keypair
2. Pages are signed with the owner's private key
3. The contract verifies all updates are properly signed
4. Anyone can read the site; only the owner can edit

## Architecture

- **Contract** — validates and stores site state on the network
- **Delegate** — runs locally, holds the signing key, signs updates
- **UI** — this web interface you're looking at

Back to [[1|Home]].
"#
            .into(),
            updated_at: now - 3600,
            signature: placeholder_sig,
        },
    );

    pages.insert(
        3,
        Page {
            title: "Markdown Guide".into(),
            content: r#"# Markdown Guide

Delta pages are written in Markdown. Here's a quick reference.

## Text Formatting

**Bold text** and *italic text* and `inline code`.

## Lists

- Item one
- Item two
  - Nested item

1. First
2. Second
3. Third

## Code Blocks

```rust
fn main() {
    println!("Hello from Delta!");
}
```

## Blockquotes

> Decentralization is the future of the web.

## Tables

| Feature | Status |
|---------|--------|
| Pages | Done |
| Editor | Done |
| Sync | Coming soon |

## Links

Link to other pages: [[1|Home]] or [[2|About]].

Back to [[1|Home]].
"#
            .into(),
            updated_at: now - 7200,
            signature: placeholder_sig,
        },
    );

    SiteState {
        config: SignedConfig {
            config: SiteConfig {
                version: 1,
                name: "Example Delta Site".into(),
                description: "A demo site showcasing Delta's features".into(),
            },
            signature: placeholder_sig,
        },
        pages,
        next_page_id: 4,
    }
}
