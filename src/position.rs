// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Position types shared by the engine, the session, and the streamview.
//!
//! These are the smallest, most persistent shapes in the codebase: a
//! [`Cursor`] is a `(source_id → byte_offset)` snapshot that the merge
//! stepper resumes from, the bookmarks list refers to, and the session
//! serializes to disk.  Keeping them in a top-level module (rather than
//! nested under `engine::merge`, which is a low-level merge
//! implementation) makes the layering match the data: the session and
//! the streamview can talk about positions without depending on the
//! merge implementation.

use crate::source::{ByteOffset, SourceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Merged-stream byte-offset position — one [`ByteOffset`] per source.
///
/// Wraps a `BTreeMap<SourceId, ByteOffset>` so callers can't accidentally
/// use it as a plain map.  Used as a serializable bookmark of where a
/// [`crate::engine::Stepper`] is in the merged stream and as the input
/// shape for restoring a [`crate::engine::Stepper`] later.  Sources
/// missing from the map resolve to [`ByteOffset::ZERO`] when used as
/// input to [`crate::engine::Engine::stepper`], so a default `Cursor`
/// walks each source from its beginning.
///
/// ## Absent vs. zero
///
/// For navigation, "source not in the map" and "source mapped to
/// [`ByteOffset::ZERO`]" mean the same thing — both place the stepper
/// at the start of that source.  The map shape is *not* normalized on
/// construction (we'd have to know the full engine source set to do
/// that, which the type doesn't), so two cursors that produce identical
/// navigation behavior can still differ as `BTreeMap`s and therefore
/// compare unequal under the derived [`PartialEq`].
///
/// Today nothing observable depends on this distinction: bookmark
/// dedup and session save/load all round-trip the exact map the
/// caller built.  But a future caller comparing two cursors for
/// "do they refer to the same logical position?" must walk the
/// shared key set with [`Self::get`] (which returns `None` →
/// `ByteOffset::ZERO`) rather than relying on `==`, or normalize both
/// against a shared source set first.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Cursor {
    offsets: BTreeMap<SourceId, ByteOffset>,
}

impl Cursor {
    /// Returns an empty cursor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a cursor from an iterator of (source id, byte offset)
    /// pairs.
    pub fn with(
        offsets: impl IntoIterator<Item = (SourceId, ByteOffset)>,
    ) -> Self {
        Self { offsets: offsets.into_iter().collect() }
    }

    /// Returns the byte offset stored for `source_id`, if any.
    pub fn get(&self, source_id: &SourceId) -> Option<ByteOffset> {
        self.offsets.get(source_id).copied()
    }

    /// Sets the byte offset for `source_id`, overwriting any previous
    /// entry.
    pub fn set(&mut self, source_id: SourceId, offset: ByteOffset) {
        self.offsets.insert(source_id, offset);
    }

    /// Iterates over (source id, byte offset) pairs in ascending source
    /// id order.
    pub fn iter(&self) -> impl Iterator<Item = (&SourceId, ByteOffset)> {
        self.offsets.iter().map(|(k, v)| (k, *v))
    }

    /// Returns the number of source-id entries.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    /// Returns true iff this cursor has no entries.
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
}

/// Position within a log stream — a stable anchor that survives filter
/// changes.
///
/// A position pins down a specific event by `(source, time,
/// ordinal_within_time)`.  Same-time tiebreaking happens via
/// `ordinal_within_time`: the first event with a given `(source, time)`
/// has ordinal 0, the next has 1, and so on.  This shape was chosen so
/// that adding/removing predicates from the active filter never moves
/// what a saved position refers to: only the row index that position
/// resolves to in a filtered view changes.
///
/// The fields are private so future representations (e.g. a content
/// fingerprint to survive file rewrites) can be added without breaking
/// callers.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    JsonSchema,
)]
pub struct LogStreamPosition {
    source: SourceId,
    time: DateTime<Utc>,
    /// 0-based count of events from the same `source` with this exact
    /// `time`.
    ordinal_within_time: u64,
}

impl LogStreamPosition {
    /// Builds a position from its component parts.
    pub fn new(
        source: SourceId,
        time: DateTime<Utc>,
        ordinal_within_time: u64,
    ) -> Self {
        Self { source, time, ordinal_within_time }
    }

    /// Returns the source this position refers to.
    pub fn source(&self) -> &SourceId {
        &self.source
    }

    /// Returns the timestamp of the event at this position.
    pub fn time(&self) -> DateTime<Utc> {
        self.time
    }

    /// Returns the within-source same-timestamp tiebreaker for this
    /// position.
    pub fn ordinal_within_time(&self) -> u64 {
        self.ordinal_within_time
    }
}
