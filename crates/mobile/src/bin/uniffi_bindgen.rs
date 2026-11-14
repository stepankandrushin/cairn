//! The bindings' generator, built from the same uniffi as the library, so the two always agree:
//! `cargo run -p cairn-mobile --bin uniffi-bindgen -- generate --library <lib> --language swift`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
