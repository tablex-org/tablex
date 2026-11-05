//! `TABLEX_BENCH_DYLIB=<path> cargo test -p engine-driver-host --release --test verify_bench -- --ignored --nocapture`
//! Times the install-time verification of a real driver library: size,
//! SHA-256, image header and the native signature check (Developer policy, so
//! an ad-hoc signed build passes).

use engine_driver_host::{DriverTarget, DriverVerifier, SignerConfig, TrustPolicy};
use std::time::Instant;

#[test]
#[ignore = "needs TABLEX_BENCH_DYLIB; prints timings"]
fn verify_real_driver_library() {
    let path =
        std::path::PathBuf::from(std::env::var("TABLEX_BENCH_DYLIB").expect("TABLEX_BENCH_DYLIB"));
    let size = std::fs::metadata(&path).expect("metadata").len();
    let verifier = DriverVerifier::new(DriverTarget::current());
    let signers = SignerConfig::default();
    let mut samples = Vec::new();
    for _ in 0..10 {
        let start = Instant::now();
        verifier.verify_size(&path, size).expect("size");
        let _ = verifier.verify_checksum(&path, "0");
        verifier.verify_architecture(&path).expect("header");
        verifier
            .verify_native_signature(&path, &signers, TrustPolicy::Developer)
            .expect("signature");
        samples.push(start.elapsed());
    }
    samples.sort();
    println!(
        "verify {} bytes: min {:?} median {:?} max {:?}",
        size,
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() - 1]
    );
}
