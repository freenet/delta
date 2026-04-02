mod components;
mod example_data;
mod freenet_api;
mod state;

use components::App;
use dioxus::prelude::*;

fn main() {
    launch(App);
}
