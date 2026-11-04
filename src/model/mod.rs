//! Domain model — the durable vocabulary every layer agrees on.
//!
//! *One authority per fact:* these types are the shared language of the CLI,
//! the sidecar, and the event log. Identifiers ([`ids`]), the persisted
//! session record ([`meta`]), the run [`state`] machine and its
//! [`transition`]s, the launch [`spec`], recorded [`event`]s, PTY control
//! ([`pty`], and the run-bound input controller in [`pty_control`]),
//! dependency-failure ([`dep_fail`]), [`ready`] checks, and
//! [`boundary`]/[`provenance`] metadata.

pub mod boundary;
pub mod dep_fail;
pub mod event;
pub mod ids;
pub mod meta;
pub mod provenance;
pub mod pty;
pub mod pty_control;
pub mod ready;
pub mod spec;
pub mod state;
pub mod transition;
