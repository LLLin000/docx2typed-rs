//! Issue #60: secured review collaboration lane for the Rust tracer.
//!
//! - `security`: the issue #31 transport-security primitives (capability,
//!   Host allowlist, origin gates, throttle, security headers).
//! - `queue`: the file-backed review event queue (`.review/inbox`).
//! - `collab`: the document/session state machine, settlement, and the
//!   external-write guard.
//! - `history`: the generation-bound review trail (`.review/history.jsonl`).
//! - `frame`: the atomic review frame (`docx2typed-review-frame-1`) — one
//!   store pin yields one consistent read model (Core document projection +
//!   queue + session + history) for the browser console.

pub mod collab;
pub mod draft;
pub mod facade;
pub mod format_lane;
pub mod frame;
pub mod history;
pub mod queue;
pub mod security;
pub mod server;

pub use collab::{CollaborationError, PATCH_SCHEMA, SNAPSHOT_SCHEMA};
pub use frame::{FRAME_SCHEMA, POSITION_CONTRACT};

/// The sidecar holding the format requests a workdir has accumulated.
pub const FORMAT_INTENTS_FILE: &str = "format_intents.json";
/// The sidecar holding the plan derived from them — what the build executes.
pub const FORMAT_PLAN_FILE: &str = "format_plan.json";
