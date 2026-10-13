#![no_main]
//! Error bodies come from servers, proxies and captive portals: anything must classify.
use libfuzzer_sys::fuzz_target;
use svn_protocol::{Headers, error_from_response};

fuzz_target!(|data: &[u8]| {
    let status = 100 + u16::from(data.first().copied().unwrap_or(0)) * 2;
    let _ = error_from_response(status, &Headers::new(), data);
});
