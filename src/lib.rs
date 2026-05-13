// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Library crate behind the `seer` and `seeit` binaries.

pub mod engine;
pub mod event;
pub mod filter;
pub mod position;
pub mod render;
pub mod save_policy;
pub mod session;
pub mod session_store;
pub mod source;
pub mod stream;
pub mod streamview;
pub mod summary;
pub mod view_target;

#[cfg(any(test, feature = "test-fixtures"))]
pub mod test_fixtures;

pub use engine::{
    Cursor, Engine, EngineEvent, EventStream, MergeError, MergeRecord, Stepper,
};
pub use event::{Event, Hostname, Level, LoggerName, Pid, UnknownLevel};
pub use filter::{
    EventPredicate, Filter, FilterParseError, Form, Predicate, SourcePredicate,
    TimeOp,
};
pub use position::{ByteLen, ByteOffset, LogStreamPosition, SourceId};
pub use render::{
    HostnameDisplay, RenderOpts, ShowDate, format_event, format_time,
    short_hostname,
};
pub use save_policy::{Cadence, SavePolicy};
pub use session::{
    Bookmark, BookmarkId, BookmarkName, CURRENT_SESSION_VERSION, Session,
    SessionId, SessionIdParseError, SessionSource, Tab, TabKind,
};
pub use session_store::{
    MatchKind, STATE_DIR_ENV, SessionMatch, SessionStore, StoreError,
};
pub use source::{
    Direction, FileSource, QueryRecord, Source, SourceError, SourceMetadata,
};
pub use stream::{LogStream, LogStreamId};
pub use streamview::{
    EventIdx, LineIdx, Materialized, ParseStats, RecordKey, RenderedLine, Row,
    SEARCH_BUDGET, SearchAnchor, SearchDir, SearchOutcome, StreamView,
    WindowFillStatus,
};
pub use summary::{
    FieldSummary, Summary, SummaryBuilder, TimeSummary, format_summary,
    summarize,
};
pub use view_target::{
    BookmarkChoice, ResolveError, ResolvedMode, ResolvedTarget, Selector,
    build_seeit_command, resolve, resolve_in_session,
};
