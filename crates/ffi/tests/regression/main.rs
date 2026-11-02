//! Regression and journey tests, run offline through the same `Core` the
//! Mac app calls. `cargo test -p kite-ffi --test regression`.

mod fixture;

mod accounts;
mod compose;
mod mail;
mod search;
mod store;
