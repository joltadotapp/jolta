//! Regression and journey tests, run offline through the same `Core` the
//! Mac app calls. `cargo test -p jolta-ffi --test regression`.

mod fixture;
mod server;

mod accounts;
mod compose;
mod mail;
mod search;
mod store;
mod sync;
