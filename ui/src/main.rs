mod components;
mod freenet_api;
mod state;

/// Test-only pins guarding the build-time legacy-delegate registry.
#[cfg(test)]
mod migration_registry_pin;

use components::App;
use dioxus::prelude::*;

fn main() {
    launch(App);
}
