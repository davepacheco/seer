// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Persistent session state for the TUI.
//!
//! A `Session` captures everything needed to re-open the user's view of
//! their investigation: the open tabs and where each is scrolled to, the
//! set of log streams that exist (whether or not a tab is currently
//! showing them), and the user's named/unnamed bookmarks indexed by
//! stream.  Sources, named filter groups, and the set of fields shown by
//! each stream will join this struct as those concepts land.

use crate::stream::{LogStream, LogStreamId, LogStreamPosition};
use derive_more::{Display, From};
use iddqd::IdOrdMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// User-supplied name for a bookmark.
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
    Display,
    From,
)]
#[serde(transparent)]
pub struct BookmarkName(String);

/// A position within a log stream, optionally given a name by the user.
///
/// Anonymous bookmarks (`name == None`) are how tabs remember where the
/// user has scrolled to; named bookmarks are saved deliberately and
/// listed in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub position: LogStreamPosition,
    pub name: Option<BookmarkName>,
}

/// A tab in the TUI.
///
/// A tab is a view onto exactly one [`LogStream`] (referenced by id; the
/// stream lives in [`Session::streams`]).  `cursor` is an anonymous
/// bookmark recording where the tab is currently scrolled to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub stream: LogStreamId,
    pub cursor: Bookmark,
}

/// Top-level session state.
///
/// Designed to be the unit of persistence: serialize this and you've
/// captured enough to put the user back where they left off.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub tabs: Vec<Tab>,
    pub streams: IdOrdMap<LogStream>,
    pub user_bookmarks: BTreeMap<LogStreamId, Vec<Bookmark>>,
}

impl Session {
    /// Returns an empty session — no streams, no tabs, no bookmarks.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_round_trips_through_serde() {
        let s = Session::new();
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert!(back.tabs.is_empty());
        assert!(back.streams.is_empty());
        assert!(back.user_bookmarks.is_empty());
    }

    #[test]
    fn populated_session_round_trips_through_serde() {
        let stream = LogStream::new();
        let stream_id = stream.id;

        let mut s = Session::new();
        s.streams.insert_unique(stream).expect("unique id");
        s.tabs.push(Tab {
            stream: stream_id,
            cursor: Bookmark {
                position: LogStreamPosition::from(42),
                name: None,
            },
        });
        s.user_bookmarks.insert(
            stream_id,
            vec![
                Bookmark {
                    position: LogStreamPosition::from(0),
                    name: Some(BookmarkName::from("start".to_string())),
                },
                Bookmark {
                    position: LogStreamPosition::from(100),
                    name: None,
                },
            ],
        );

        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tabs[0].stream, stream_id);
        assert_eq!(
            back.tabs[0].cursor.position,
            LogStreamPosition::from(42)
        );
        assert!(back.tabs[0].cursor.name.is_none());

        assert_eq!(back.streams.len(), 1);
        assert!(back.streams.get(&stream_id).is_some());

        let bms = back.user_bookmarks.get(&stream_id).unwrap();
        assert_eq!(bms.len(), 2);
        assert_eq!(
            bms[0].name.as_ref().map(|n| n.to_string()),
            Some("start".to_string())
        );
        assert_eq!(bms[1].name, None);
    }
}
