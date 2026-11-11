//! Earshot's core: everything that doesn't need the game.
//!
//! Earshot is an independent project, not affiliated with Simple Voice Chat. The wire format here
//! was worked out from our own captures only; see `PROTOCOL.md` in the repository for the evidence
//! behind each field.
//!
//! - [`wire`]: big-endian integers, VarInts, UUIDs and strings as SVC encodes them
//! - [`plugin`]: the `voicechat:*` messages carried over the Minecraft connection
//! - [`udp`]: the encrypted voice packets
//! - [`crypto`]: AES-128-GCM, built on the `aes` block cipher
//! - [`codec`]: Opus encoding/decoding at SVC's settings (48 kHz mono, 20 ms)
//! - [`jitter`]: reorders and paces incoming voice frames
//! - [`spatial`]: distance falloff and stereo panning
//! - [`mixer`]: everyone we hear, decoded, placed and summed
//! - [`activation`], [`resample`]: push-to-talk / voice activation, sample-rate conversion
//! - [`session`]: the connection's state machine (no I/O)
//! - [`engine`]: the running voice client (UDP, threads) the game talks to
//! - [`server`]: an SVC-compatible voice server, for Horizon dedicated servers
//! - [`audio`]: microphone and speakers through cpal (feature `audio`)
//! - [`dsp`]: microphone clean-up (high-pass, adaptive noise gate)
//! - [`menu`]: the voice menu's screens; [`ui`]: the drawing primitive and icons

pub mod activation;
#[cfg(feature = "audio")]
pub mod audio;
pub mod chat;
pub mod codec;
pub mod crypto;
pub mod dsp;
pub mod engine;
pub mod jitter;
pub mod menu;
pub mod mixer;
pub mod plugin;
pub mod resample;
pub mod server;
pub mod session;
pub mod spatial;
pub mod udp;
pub mod ui;
pub mod wire;

/// opus-rs 0.1.41 (BSD-3-Clause, restsend.com and the libopus authors), vendored unchanged
/// apart from its `crate::` paths, because Horizon mod crates can't depend on crates outside
/// Horizon's own lock file.
#[allow(
    unexpected_cfgs,
    unsafe_op_in_unsafe_fn,
    clippy::all,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_assignments
)]
pub mod opus;
