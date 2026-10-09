//! The file-level mirror engine — the main feature.
//!
//! One job points at a source path (usually a drive root, sometimes a
//! subfolder) and a destination path, and makes the destination match the
//! source. Everything else in this module exists to make that safe, resumable,
//! and faithful:
//!
//! ```text
//!   walk    scan both trees            -> Tree (entries + errors + skips)
//!   plan    diff them                  -> Plan (an ordered list of Actions)
//!   state   persist the plan           -> resumable across a crash or reboot
//!   copy    execute one action         -> data + metadata + ACLs + streams
//!   mirror  drive the whole thing      -> workers, progress, checkpoints
//! ```
//!
//! The split exists so the interesting decisions (what to copy, what to delete,
//! what is unchanged) are pure functions over data — testable without touching
//! a disk — while the messy platform work is isolated in [`copy`] and [`sys`].

pub mod attic;
pub mod copy;
pub mod filter;
pub mod meta;
pub mod mirror;
pub mod path;
pub mod plan;
pub mod state;
pub mod sys;
pub mod verify;
pub mod walk;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cooperative cancellation token, shared between the UI and the workers.
///
/// A backup of a multi-terabyte drive runs for hours, so "stop" has to work
/// promptly and without corrupting anything. Every loop that could run long
/// checks this, and because the engine checkpoints as it goes, a cancelled run
/// resumes rather than restarts.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Cancel(Arc::new(AtomicBool::new(false)))
    }

    /// A token that is never cancelled. For tests and one-shot calls.
    pub fn none() -> Self {
        Cancel::new()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_is_shared_between_clones() {
        let a = Cancel::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled(), "a clone must observe the cancellation");
    }
}
