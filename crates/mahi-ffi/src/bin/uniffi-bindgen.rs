//! Thin entry point so `scripts/build-xcframework.sh` can run
//! `cargo run --bin uniffi-bindgen -- generate ...` to emit the Swift bindings.

fn main() {
    uniffi::uniffi_bindgen_main()
}
