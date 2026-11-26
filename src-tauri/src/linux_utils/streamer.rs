//! Linux encode backend: not implemented, deliberately empty.
//!
//! A real encoder should be shaped like the others (see the `Backend` enum in
//! `windows_utils/streamer/pipeline.rs`): a concrete encoder type plus a variant on the Linux
//! pipeline's backend enum, not an implementor of a generic encoder trait the pipeline cannot call.
