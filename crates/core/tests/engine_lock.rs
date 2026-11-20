//! Two engines on one data folder would both send the outbox, so a
//! recipient could get a message twice: at most one runs at a time.

use std::sync::Arc;

use jolta_core::Jolta;
use jolta_core::engine::{Engine, Sink};

#[test]
fn a_second_engine_waits_until_the_first_is_gone() {
    jolta_core::secrets::use_memory_store();
    let dir = std::env::temp_dir().join(format!("jolta-engine-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sink: Sink = Arc::new(|_| {});
    let start = |runtime: &tokio::runtime::Runtime| Engine::start(Jolta::open(&dir).unwrap(), sink.clone(), runtime.handle().clone());

    let (app, other) = (tokio::runtime::Runtime::new().unwrap(), tokio::runtime::Runtime::new().unwrap());
    let first = start(&app).expect("the first engine starts");
    assert!(start(&other).is_err(), "a second engine must not start on the same data folder while the first runs");

    drop(first);
    drop(app);
    start(&other).expect("the engine starts once the first one is gone");
    let _ = std::fs::remove_dir_all(&dir);
}
