use crate::state::{KnownSite, SiteRole};
use delta_core::{Page, SignedConfig, SiteConfig, SiteState};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use std::collections::BTreeMap;

/// Create a deterministic placeholder verifying key from a seed byte.
fn placeholder_vk(seed: u8) -> VerifyingKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    // Use as signing key seed to get a valid point on the curve
    let sk = SigningKey::from_bytes(&bytes);
    sk.verifying_key()
}

/// Create example sites for demo/testing.
pub fn create_example_sites() -> BTreeMap<String, KnownSite> {
    let placeholder_sig = Signature::from_bytes(&[0u8; 64]);
    let now = chrono::Utc::now().timestamp() as u64;
    let mut sites = BTreeMap::new();

    // --- Site 1: My Blog (owner) ---
    {
        let prefix = "7Xk9mNqR2p".to_string();
        let mut pages = BTreeMap::new();

        pages.insert(1, Page {
            title: "Home".into(),
            content: "# Welcome to My Blog\n\nThis is a personal blog built on Freenet using Delta.\n\n## Recent Posts\n\n- [[2|First Post]] — Getting started with Delta\n- [[3|About Me]] — Who I am\n".into(),
            updated_at: now,
            signature: placeholder_sig,
        });

        pages.insert(2, Page {
            title: "First Post".into(),
            content: "# Getting Started with Delta\n\nDelta makes it easy to publish content on Freenet. Just write in Markdown and hit save.\n\n## Why Decentralized?\n\nYour content lives on the network, not on a server you have to maintain. No hosting costs, no downtime, no censorship.\n\n## What's Next\n\nI'll be posting more about my experience building on Freenet. Stay tuned!\n\nBack to [[1|Home]].\n".into(),
            updated_at: now - 86400,
            signature: placeholder_sig,
        });

        pages.insert(3, Page {
            title: "About Me".into(),
            content: "# About Me\n\nI'm a developer interested in decentralized technology.\n\n## Contact\n\nYou can find me on the Freenet network.\n\nBack to [[1|Home]].\n".into(),
            updated_at: now - 172800,
            signature: placeholder_sig,
        });

        sites.insert(
            prefix.clone(),
            KnownSite {
                name: "My Blog".into(),
                prefix: prefix.clone(),
                role: SiteRole::Owner,
                state: SiteState {
                    owner: placeholder_vk(0),
                    config: SignedConfig {
                        config: SiteConfig {
                            version: 1,
                            name: "My Blog".into(),
                            description: "A personal blog on Freenet".into(),
                        },
                        signature: placeholder_sig,
                    },
                    pages,
                    next_page_id: 4,
                },
                owner_pubkey: [1u8; 32],
                contract_key: None,
            },
        );
    }

    // --- Site 2: Freenet Docs (visitor) ---
    {
        let prefix = "3Kj7mNxQw5".to_string();
        let mut pages = BTreeMap::new();

        pages.insert(1, Page {
            title: "Introduction".into(),
            content: "# Freenet Documentation\n\nFreenet is a decentralized platform for building censorship-resistant applications.\n\n## Getting Started\n\n- [[2|Architecture]] — How Freenet works\n- [[3|Building Apps]] — Create your first dApp\n".into(),
            updated_at: now - 3600,
            signature: placeholder_sig,
        });

        pages.insert(2, Page {
            title: "Architecture".into(),
            content: "# Architecture\n\nFreenet uses a distributed hash table (DHT) with small-world routing.\n\n## Key Concepts\n\n- **Contracts** — Define and validate shared state\n- **Delegates** — Local agents that hold secrets\n- **Key-Value Store** — Global, peer-to-peer data storage\n\n## The Ring\n\nPeers are arranged on a ring topology. Each peer has a location between 0 and 1. Contracts are stored near peers whose locations are closest to the contract's key.\n\nBack to [[1|Introduction]].\n".into(),
            updated_at: now - 7200,
            signature: placeholder_sig,
        });

        pages.insert(3, Page {
            title: "Building Apps".into(),
            content: "# Building Apps on Freenet\n\nEvery Freenet app has three components:\n\n1. **Contract** — the backend (runs on the network)\n2. **Delegate** — the middleware (runs locally)\n3. **UI** — the frontend (runs in the browser)\n\n## Example: Delta\n\nDelta itself is a Freenet app! It uses a site contract to store pages, a delegate to sign updates, and this Dioxus UI.\n\n```rust\nlet page = Page::new(id, title, content, timestamp, &signing_key);\nsite.upsert_page(id, page, &owner_pubkey)?;\n```\n\nBack to [[1|Introduction]].\n".into(),
            updated_at: now - 14400,
            signature: placeholder_sig,
        });

        sites.insert(
            prefix.clone(),
            KnownSite {
                name: "Freenet Docs".into(),
                prefix: prefix.clone(),
                role: SiteRole::Visitor,
                state: SiteState {
                    owner: placeholder_vk(0),
                    config: SignedConfig {
                        config: SiteConfig {
                            version: 1,
                            name: "Freenet Docs".into(),
                            description: "Official Freenet documentation".into(),
                        },
                        signature: placeholder_sig,
                    },
                    pages,
                    next_page_id: 4,
                },
                owner_pubkey: [2u8; 32],
                contract_key: None,
            },
        );
    }

    // --- Site 3: Community Wiki (visitor) ---
    {
        let prefix = "9wRtYpKm4D".to_string();
        let mut pages = BTreeMap::new();

        pages.insert(1, Page {
            title: "Welcome".into(),
            content: "# Community Wiki\n\nA collaborative knowledge base for the Freenet community.\n\n## Topics\n\n- [[2|FAQ]] — Frequently asked questions\n".into(),
            updated_at: now - 1800,
            signature: placeholder_sig,
        });

        pages.insert(2, Page {
            title: "FAQ".into(),
            content: "# Frequently Asked Questions\n\n## What is Freenet?\n\nFreenet is a peer-to-peer platform for censorship-resistant communication and collaboration.\n\n## How do I join?\n\nDownload the Freenet software and run a node. You'll automatically connect to the network.\n\n## Is it free?\n\nYes, Freenet is free and open-source software (LGPL-3.0).\n\nBack to [[1|Welcome]].\n".into(),
            updated_at: now - 3600,
            signature: placeholder_sig,
        });

        sites.insert(
            prefix.clone(),
            KnownSite {
                name: "Community Wiki".into(),
                prefix: prefix.clone(),
                role: SiteRole::Visitor,
                state: SiteState {
                    owner: placeholder_vk(0),
                    config: SignedConfig {
                        config: SiteConfig {
                            version: 1,
                            name: "Community Wiki".into(),
                            description: "Community knowledge base".into(),
                        },
                        signature: placeholder_sig,
                    },
                    pages,
                    next_page_id: 3,
                },
                owner_pubkey: [3u8; 32],
                contract_key: None,
            },
        );
    }

    sites
}
