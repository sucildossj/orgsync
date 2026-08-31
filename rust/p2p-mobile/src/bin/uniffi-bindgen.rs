//! Binding generator entry point.
//!
//! Run via `cargo run -p p2p-mobile --bin uniffi-bindgen -- generate …`,
//! which is what `scripts/build-ios.sh` and `scripts/build-android.sh` do.
fn main() {
    uniffi::uniffi_bindgen_main()
}
