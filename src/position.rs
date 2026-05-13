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
