uniffi::setup_scaffolding!();

#[uniffi::export]
pub fn core_greeting() -> String {
    kite_core::greeting()
}
