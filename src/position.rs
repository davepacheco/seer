// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Position types shared by the engine, the session, and the streamview.
//!
//! These are the smallest, most persistent shapes in the codebase: a
//! [`Cursor`] is a `(source_id → byte_offset)` snapshot that the merge
//! stepper resumes from, the bookmarks list refers to, and the session
//! serializes to disk.

use chrono::{DateTime, Utc};
use derive_more::{AsRef, Display, From};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Add, AddAssign, Sub};

/// Identifier for a source.
///
/// Wraps a string so different `Source` impls can choose the most useful
/// shape for their identifier (canonicalized path, archive entry name,
/// URL, etc.) without forcing a single representation on the type.
///
/// Implements `Serialize`/`Deserialize` so it can ride inside a
/// [`LogStreamPosition`] in persisted session state.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Display,
    From,
    AsRef,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[as_ref(forward)]
#[serde(transparent)]
pub struct SourceId(String);

/// Byte offset into a source's underlying bytes.
///
/// A newtype around `u64` so an offset can't be silently confused
/// with a length, count, or any other unsigned quantity that turns up
/// in adjacent code.
///
/// Convention: an offset always names the byte at which the *next*
/// record would start when scanning forward — equivalently, the byte
/// just past the end of the previous record.  Backward scans honor
/// the same convention: an offset of `N` reads the record whose end
/// is at `N`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Display,
    From,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
pub struct ByteOffset(u64);

impl ByteOffset {
    /// Byte offset zero — the start of any source.
    pub const ZERO: Self = Self(0);

    /// Returns the offset as a raw `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Byte length: a span of bytes within a source.
///
/// Newtype companion to [`ByteOffset`].  The pair encodes the
/// arithmetic the engine repeatedly performs on byte counts: an
/// offset plus a length yields another offset, two lengths add to a
/// length, an offset minus a length yields an offset.  Mixing the
/// two in any other shape is a compile error, which catches the
/// otherwise-invisible bug of (e.g.) adding two offsets.
///
/// `#[serde(transparent)]` so persisted shapes are bare `u64`s on disk.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Display,
    From,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
pub struct ByteLen(u64);

impl ByteLen {
    /// Length zero.
    pub const ZERO: Self = Self(0);

    /// Returns the length as a raw `u64`.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Saturating subtraction: `a - b`, clamped at [`Self::ZERO`].
    /// Used by the long-op drivers' "bytes since op start" math
    /// where wall-clock skew between the snapshot and the latest
    /// reading could otherwise underflow.
    pub fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl Add for ByteLen {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for ByteLen {
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl Sub for ByteLen {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self(self.0 - other.0)
    }
}

impl std::iter::Sum for ByteLen {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        Self(iter.map(|b| b.0).sum())
    }
}

impl Add<ByteLen> for ByteOffset {
    type Output = ByteOffset;
    fn add(self, len: ByteLen) -> ByteOffset {
        ByteOffset(self.0 + len.0)
    }
}

impl AddAssign<ByteLen> for ByteOffset {
    fn add_assign(&mut self, len: ByteLen) {
        self.0 += len.0;
    }
}

impl Sub<ByteLen> for ByteOffset {
    type Output = ByteOffset;
    fn sub(self, len: ByteLen) -> ByteOffset {
        ByteOffset(self.0 - len.0)
    }
}

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
/// that, which the type doesn't), so two cursors that describe the
/// same logical position can differ as `BTreeMap`s.  [`PartialEq`]
/// and [`PartialOrd`] honor that equivalence: missing entries are
/// treated as [`ByteOffset::ZERO`] on both sides, so `{s: 0}` and
/// `{}` compare equal.
///
/// [`PartialOrd`] is a *partial* order — the product order on the
/// union of source ids.  Two cursors are comparable only when every
/// source agrees on the direction; mixed directions (A ahead of B on
/// one source and behind on another) return `None`.  That is also why
/// there is no `Ord` impl: cursors don't have a meaningful total
/// order across distinct streams.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
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
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Returns the cumulative sum of bytes in the underlying stream *prior* to
    /// this position
    pub fn byte_offset(&self) -> ByteOffset {
        ByteOffset(self.iter().map(|(_, off)| off.get()).sum())
    }
}

impl PartialEq for Cursor {
    fn eq(&self, other: &Self) -> bool {
        self.partial_cmp(other) == Some(Ordering::Equal)
    }
}

impl Eq for Cursor {}

impl PartialOrd for Cursor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        // Product order on the union of source ids, with missing
        // entries treated as `ByteOffset::ZERO`.  Two cursors are
        // comparable only when every source agrees on the direction;
        // mixed directions yield `None`.
        let keys: BTreeSet<&SourceId> =
            self.offsets.keys().chain(other.offsets.keys()).collect();
        let mut result = Ordering::Equal;
        for k in keys {
            let a = self.get(k).unwrap_or(ByteOffset::ZERO);
            let b = other.get(k).unwrap_or(ByteOffset::ZERO);
            match (result, a.cmp(&b)) {
                (_, Ordering::Equal) => {}
                (Ordering::Equal, c) => result = c,
                (Ordering::Less, Ordering::Less)
                | (Ordering::Greater, Ordering::Greater) => {}
                (Ordering::Less, Ordering::Greater)
                | (Ordering::Greater, Ordering::Less) => return None,
            }
        }
        Some(result)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SourceId {
        SourceId::from(s.to_string())
    }

    fn off(n: u64) -> ByteOffset {
        ByteOffset::from(n)
    }

    #[test]
    fn cursor_eq_empty() {
        assert_eq!(Cursor::new(), Cursor::default());
        assert_eq!(Cursor::new(), Cursor::with([]));
    }

    #[test]
    fn cursor_eq_absent_is_zero() {
        // A source mapped to ZERO is equivalent to that source being
        // absent from the map.
        let with_zero = Cursor::with([(sid("a"), ByteOffset::ZERO)]);
        assert_eq!(with_zero, Cursor::new());
        assert_eq!(Cursor::new(), with_zero);

        let mixed =
            Cursor::with([(sid("a"), off(5)), (sid("b"), ByteOffset::ZERO)]);
        let just_a = Cursor::with([(sid("a"), off(5))]);
        assert_eq!(mixed, just_a);
    }

    #[test]
    fn cursor_eq_distinct_offsets() {
        let a = Cursor::with([(sid("a"), off(5))]);
        let b = Cursor::with([(sid("a"), off(6))]);
        assert_ne!(a, b);

        let with_extra = Cursor::with([(sid("a"), off(5)), (sid("b"), off(1))]);
        assert_ne!(a, with_extra);
    }

    #[test]
    fn cursor_partial_cmp_equal() {
        let a = Cursor::with([(sid("a"), off(5)), (sid("b"), off(7))]);
        let b = a.clone();
        assert_eq!(a.partial_cmp(&b), Some(Ordering::Equal));

        // Equivalent under absent-vs-zero.
        let c = Cursor::with([
            (sid("a"), off(5)),
            (sid("b"), off(7)),
            (sid("c"), ByteOffset::ZERO),
        ]);
        assert_eq!(a.partial_cmp(&c), Some(Ordering::Equal));
    }

    #[test]
    fn cursor_partial_cmp_less_and_greater() {
        let a = Cursor::with([(sid("a"), off(1)), (sid("b"), off(2))]);
        let b = Cursor::with([(sid("a"), off(3)), (sid("b"), off(4))]);
        assert_eq!(a.partial_cmp(&b), Some(Ordering::Less));
        assert_eq!(b.partial_cmp(&a), Some(Ordering::Greater));

        // Equal on one source, less on another → Less overall.
        let c = Cursor::with([(sid("a"), off(1)), (sid("b"), off(4))]);
        assert_eq!(a.partial_cmp(&c), Some(Ordering::Less));
    }

    #[test]
    fn cursor_partial_cmp_incomparable() {
        // Mixed directions across sources → None.
        let a = Cursor::with([(sid("a"), off(1)), (sid("b"), off(5))]);
        let b = Cursor::with([(sid("a"), off(2)), (sid("b"), off(4))]);
        assert_eq!(a.partial_cmp(&b), None);
        assert_eq!(b.partial_cmp(&a), None);
    }

    #[test]
    fn cursor_partial_cmp_missing_keys() {
        // A source missing on one side counts as ZERO there.
        let empty = Cursor::new();
        let one = Cursor::with([(sid("a"), off(5))]);
        assert_eq!(empty.partial_cmp(&one), Some(Ordering::Less));
        assert_eq!(one.partial_cmp(&empty), Some(Ordering::Greater));

        // Less on the shared key, missing (i.e. ZERO) vs non-zero on
        // another: still ordered, in the same direction.
        let a = Cursor::with([(sid("a"), off(1))]);
        let b = Cursor::with([(sid("a"), off(3)), (sid("b"), off(2))]);
        assert_eq!(a.partial_cmp(&b), Some(Ordering::Less));

        // Less on the shared key but greater on the missing-vs-present
        // key → incomparable.
        let c = Cursor::with([(sid("a"), off(1)), (sid("b"), off(2))]);
        let d = Cursor::with([(sid("a"), off(3))]);
        assert_eq!(c.partial_cmp(&d), None);
    }

    #[test]
    fn cursor_eq_matches_partial_cmp() {
        // `eq` and `partial_cmp == Some(Equal)` must agree, per the
        // PartialEq/PartialOrd contract.
        let cases = [
            (Cursor::new(), Cursor::new()),
            (
                Cursor::with([(sid("a"), off(5))]),
                Cursor::with([
                    (sid("a"), off(5)),
                    (sid("b"), ByteOffset::ZERO),
                ]),
            ),
            (
                Cursor::with([(sid("a"), off(5))]),
                Cursor::with([(sid("a"), off(6))]),
            ),
        ];
        for (a, b) in &cases {
            assert_eq!(
                a == b,
                a.partial_cmp(b) == Some(Ordering::Equal),
                "eq/partial_cmp disagree on {:?} vs {:?}",
                a,
                b,
            );
        }
    }
}
