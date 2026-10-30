pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn greeting() -> String {
    format!("Kite core {} is running", version())
}
