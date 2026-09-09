//! Linux encode backend — not implemented.
//!
//! There is deliberately nothing here. This used to hold `VaapiBackend`, a `bail!`-only
//! implementor of a `streamer::platform::EncoderBackend` trait that no longer exists: the trait
//! modelled none of the surface the pipeline actually calls — no texture input, no repeat, no
//! prescale hint, no device accessor — and its `BackendConfig` could not even carry the profile
//! or the intra-refresh flag. Every real backend (NVENC, Quick Sync, AMF, x264) is a variant of
//! the `Backend` enum in `windows_utils/streamer/pipeline.rs` instead.
//!
//! Keeping the stub around would have invited the Linux implementation to satisfy an interface
//! the pipeline cannot call, which is the trap the AMD backend had to step over. When Linux gets
//! a real encoder it should be shaped like the others: a concrete encoder type plus a variant on
//! whatever the Linux pipeline's backend enum turns out to be.
