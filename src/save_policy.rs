// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! When to flush an in-memory [`crate::Session`] to disk.
//!
//! Splits session mutations into two cadences and offers a tiny
//! predicate the TUI can poll from its event loop.  No I/O, no
//! filesystem, no [`crate::SessionStore`] — just a dirty bit, a
//! timestamp, and a debounce window.  The TUI is the source of
//! truth for which mutations are inline-cadence and which are
//! debounced; the policy only bookkeeps and answers "save now?".
//!
//! ## Contract
//!
//! - On every session-affecting mutation, the TUI calls
//!   [`SavePolicy::record`] with the appropriate [`Cadence`].  The
//!   return value tells the caller whether to flush immediately.
//! - At the top of each frame (or each input event), the TUI calls
//!   [`SavePolicy::due`] with the current [`Instant`].  Returning
//!   `true` means the debounce window has elapsed and pending
//!   changes should be flushed.
//! - On exit, the TUI consults [`SavePolicy::dirty`] to decide
//!   whether to do a final save.
//! - After a successful flush, the TUI calls
//!   [`SavePolicy::mark_saved`].  A failed save is *not* reported
//!   back: the dirty bit stays set, and the next opportunity will
//!   try again.

use std::time::{Duration, Instant};

/// Cadence at which a session-affecting mutation should be flushed.
///
/// The two variants correspond directly to the two bullet lists in
/// `plan-sessions.md`: inline-cadence mutations are user-visible and
/// rare; debounced-cadence mutations are frequent and per-pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cadence {
    /// User-visible, low-frequency change — bookmark create / rename
    /// / delete, tab open / close, filter changes, field show /
    /// hide.  Flush right away so a crash never loses work the user
    /// took an explicit action to produce.
    Inline,
    /// High-frequency, low-value change — cursor scrolling, viewport
    /// resize.  Mark dirty; the next debounce tick will flush.
    Debounced,
}

/// Tracks when the TUI should flush the in-memory session to disk.
///
/// Pure state — no I/O, no clock of its own.  The caller passes
/// [`Instant`] values in so tests can advance time deterministically.
#[derive(Debug, Clone)]
pub struct SavePolicy {
    dirty: bool,
    last_saved_at: Option<Instant>,
    debounce: Duration,
}

impl SavePolicy {
    /// Default debounce window between flushes of debounced-cadence
    /// changes.
    pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(10);

    /// Returns a fresh policy.  Starts clean; `last_saved_at` is
    /// unset, which means the very first debounced mutation will be
    /// reported due on the next tick.  In normal use the TUI flushes
    /// the initial session at startup and calls
    /// [`Self::mark_saved`] then, so this only matters when a
    /// debounced mutation arrives before any flush has happened.
    pub fn new(debounce: Duration) -> Self {
        Self { dirty: false, last_saved_at: None, debounce }
    }

    /// Records a session-affecting mutation.  Returns `true` for
    /// inline-cadence mutations, telling the caller to flush now.
    /// Either way, the dirty bit is set.
    pub fn record(&mut self, cadence: Cadence) -> bool {
        self.dirty = true;
        matches!(cadence, Cadence::Inline)
    }

    /// Returns `true` if there are unsaved changes and the debounce
    /// window has elapsed since the last successful flush.  Until
    /// the first flush, any pending change is reported due.
    pub fn due(&self, now: Instant) -> bool {
        if !self.dirty {
            return false;
        }
        match self.last_saved_at {
            None => true,
            Some(t) => now.duration_since(t) >= self.debounce,
        }
    }

    /// Returns `true` if there are unsaved changes.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Records that a flush completed successfully: clears the
    /// dirty bit and starts a new debounce window from `now`.
    pub fn mark_saved(&mut self, now: Instant) {
        self.dirty = false;
        self.last_saved_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Instant`s can't be constructed out of thin air, but they can
    /// be advanced.  Fixing `t0` once per test and deriving every
    /// other instant by addition keeps the tests deterministic and
    /// independent of wall-clock skew.
    fn t0() -> Instant {
        Instant::now()
    }

    const DEBOUNCE: Duration = Duration::from_secs(10);

    #[test]
    fn fresh_policy_is_clean_and_not_due() {
        let p = SavePolicy::new(DEBOUNCE);
        assert!(!p.dirty());
        assert!(!p.due(t0()));
    }

    #[test]
    fn inline_mutation_returns_save_now_and_sets_dirty() {
        let mut p = SavePolicy::new(DEBOUNCE);
        assert!(p.record(Cadence::Inline));
        assert!(p.dirty(), "dirty stays set until mark_saved");
    }

    #[test]
    fn debounced_mutation_does_not_save_now_but_sets_dirty() {
        let mut p = SavePolicy::new(DEBOUNCE);
        assert!(!p.record(Cadence::Debounced));
        assert!(p.dirty());
    }

    #[test]
    fn due_is_true_when_dirty_and_never_saved() {
        let mut p = SavePolicy::new(DEBOUNCE);
        p.record(Cadence::Debounced);
        // No prior mark_saved -> due immediately, regardless of "now".
        assert!(p.due(t0()));
        assert!(p.due(t0() + Duration::from_secs(3600)));
    }

    #[test]
    fn due_is_false_within_debounce_window() {
        let mut p = SavePolicy::new(DEBOUNCE);
        let t = t0();
        p.mark_saved(t);
        p.record(Cadence::Debounced);
        assert!(!p.due(t + Duration::from_secs(0)));
        assert!(!p.due(t + Duration::from_secs(5)));
        assert!(!p.due(t + Duration::from_secs(9)));
    }

    #[test]
    fn due_is_true_at_and_past_debounce_boundary() {
        let mut p = SavePolicy::new(DEBOUNCE);
        let t = t0();
        p.mark_saved(t);
        p.record(Cadence::Debounced);
        assert!(p.due(t + DEBOUNCE), "exactly at the boundary");
        assert!(p.due(t + DEBOUNCE + Duration::from_secs(1)));
    }

    #[test]
    fn mark_saved_clears_dirty_and_resets_window() {
        let mut p = SavePolicy::new(DEBOUNCE);
        let t = t0();
        p.record(Cadence::Debounced);
        assert!(p.dirty());
        p.mark_saved(t);
        assert!(!p.dirty());
        // Even past the boundary, "due" is false until the next
        // mutation re-dirties.
        assert!(!p.due(t + DEBOUNCE * 2));
    }

    #[test]
    fn debounced_after_inline_re_dirties_and_waits_for_window() {
        // Inline -> save -> Debounced.  The debounced change should
        // set dirty again and only come due once the debounce has
        // elapsed from the inline-save moment.
        let mut p = SavePolicy::new(DEBOUNCE);
        let t = t0();
        assert!(p.record(Cadence::Inline));
        p.mark_saved(t);

        assert!(!p.record(Cadence::Debounced));
        assert!(p.dirty());
        assert!(!p.due(t + Duration::from_secs(1)));
        assert!(p.due(t + DEBOUNCE));
    }

    #[test]
    fn inline_after_debounced_still_returns_save_now() {
        // A debounced change is pending when an inline change
        // arrives.  The inline call must report "save now"; the
        // following mark_saved flushes both.
        let mut p = SavePolicy::new(DEBOUNCE);
        let t = t0();
        p.mark_saved(t); // make this a non-trivial starting state
        p.record(Cadence::Debounced);
        assert!(p.dirty());
        assert!(p.record(Cadence::Inline));
        p.mark_saved(t + Duration::from_secs(1));
        assert!(!p.dirty());
        assert!(!p.due(t + DEBOUNCE * 2));
    }
}
