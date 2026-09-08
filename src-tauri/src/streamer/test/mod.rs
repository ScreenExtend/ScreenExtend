// The self-test opens a TLS connection to a server presenting a runtime-generated self-signed
// certificate, so it carries a verifier that accepts any chain. That must never be linked into
// a shipping binary, hence the feature gate: it exists under `cargo test` and under
// `--features probe`, and nowhere else.
#[cfg(any(test, feature = "probe"))]
pub mod whep_selftest;

#[cfg(test)]
mod bitrate;
