// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Library crate behind the `seer` and `seeit` binaries.

pub mod engine;
pub mod event;
pub mod filter;
pub mod render;
pub mod save_policy;
pub mod seeit_target;
pub mod session;
pub mod session_store;
pub mod source;
pub mod stream;
pub mod streamview;
pub mod summary;

#[cfg(test)]
mod test_util;

pub use engine::{
    Cursor, Engine, EngineEvent, EventStream, MergeError, MergeRecord, Stepper,
};
pub use event::{Event, Hostname, Level, LoggerName, Pid, UnknownLevel};
pub use filter::{Filter, FilterParseError, Predicate, TimeOp};
pub use render::{
    HostnameDisplay, RenderOpts, format_event, format_time, short_hostname,
};
pub use save_policy::{Cadence, SavePolicy};
pub use seeit_target::{
    BookmarkChoice, ResolveError, ResolvedMode, ResolvedTarget, Selector,
    resolve, resolve_in_session,
};
pub use session::{
    Bookmark, BookmarkId, BookmarkName, CURRENT_SESSION_VERSION, Session,
    SessionSource, Tab, TabKind,
};
pub use session_store::{
    MatchKind, STATE_DIR_ENV, SessionId, SessionIdParseError, SessionMatch,
    SessionStore, StoreError,
};
pub use source::{
    ByteOffset, Direction, FileSource, QueryRecord, Source, SourceError,
    SourceId, SourceMetadata,
};
pub use stream::{LogStream, LogStreamId, LogStreamPosition};
pub use streamview::{
    ParseStats as StreamViewParseStats, RecordKey, RenderedLine, SEARCH_BUDGET,
    SearchDir, SearchOutcome, StreamView, WindowFillStatus,
};
pub use summary::{
    FieldSummary, Summary, SummaryBuilder, TimeSummary, format_summary,
    summarize,
};
