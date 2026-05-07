// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Library crate behind the `seer` and `seeit` binaries.

pub mod engine;
pub mod event;
pub mod filter;
pub mod session;
pub mod source;
pub mod stream;

#[cfg(test)]
mod test_util;

pub use engine::{Engine, EngineEvent, ResolvePosition};
pub use event::{Event, Hostname, Level, LoggerName, Pid, UnknownLevel};
pub use filter::{Filter, FilterParseError, Predicate};
pub use session::{
    Bookmark, BookmarkId, BookmarkName, CURRENT_SESSION_VERSION, Session, Tab,
};
pub use source::{FileSource, Source, SourceError, SourceId};
pub use stream::{LogStream, LogStreamId, LogStreamPosition};
