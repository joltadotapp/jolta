// A sandboxed command-line tool needs its own Info.plist to get a sandbox
// identity when an agent (not Kite.app) launches it.
fn main() {
    let plist = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}", plist.display());
}
