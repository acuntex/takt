//! Audio output: the ring buffer between the decoder and the player, the child-process
//! pipeline, and the `syncspin_client::Sink` the daemon hands to the client.
//!
//! Task 6 wired the decoder/`pacat` around this bridge; Task 7 drives playback; the
//! syncspin switchover adds [`gst`], which exposes the pipeline to the client crate.

pub mod bridge;
pub mod gst;
pub mod process;
