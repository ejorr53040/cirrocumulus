//! guest-init's config, as the Node agent sends it over vsock: whatever
//! arrives, parsing it must return, never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(json) = std::str::from_utf8(data) {
        let _ = cirro_guest_init::config::parse_config(json);
    }
});
