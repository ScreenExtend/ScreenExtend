// carries a TLS verifier that accepts any chain (the server's cert is self-signed), so it
// must never be linked into a shipping binary: `cargo test` and `--features probe` only
#[cfg(any(test, feature = "probe"))]
pub mod whep_selftest;

#[cfg(test)]
mod bitrate;

// counts UDP sockets via /dev/fd on macOS and the per-owner socket table on Windows
#[cfg(all(test, any(target_os = "macos", windows)))]
mod session_leak;
