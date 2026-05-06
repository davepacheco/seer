// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Log streams.
//!
//! A "log stream" is the unit a tab views.  Conceptually it is a filter
//! over the events in some set of sources; today the type carries only
//! an id, so its observable behavior is "produces every event from every
//! source the engine knows about" — the same as `Engine::query_events`.
//! Filters and source-set restrictions land here next.

use derive_more::{Display, From};
use iddqd::{IdOrdItem, id_upcast};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for a [`LogStream`].
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Display,
    From,
)]
#[serde(transparent)]
pub struct LogStreamId(Uuid);

impl LogStreamId {
    /// Returns a freshly-generated random id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Position within a log stream.
///
/// For now this is just the 0-based ordinal of the event in the stream.
/// We will likely need a richer representation once filters and re-parses
/// can shift event ordinals; the type is opaque here so callers don't
/// depend on the current shape.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Display,
    From,
)]
#[serde(transparent)]
pub struct LogStreamPosition(u64);

/// A log stream.
///
/// Today it just carries its id; eventually it will own a filter and
/// the set of sources it draws from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogStream {
    pub id: LogStreamId,
}

impl LogStream {
    /// Returns a new log stream with a freshly-generated id.
    // No `Default` impl: each call mints a distinct id, so a default
    // value would silently produce non-equal objects.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { id: LogStreamId::new_v4() }
    }
}

impl IdOrdItem for LogStream {
    type Key<'a> = LogStreamId;

    fn key(&self) -> Self::Key<'_> {
        self.id
    }

    id_upcast!();
}
