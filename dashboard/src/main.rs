use dioxus::prelude::*;

mod app;
mod api;
mod routes;
mod components;

fn main() {
    dioxus::launch(app::App);
}
