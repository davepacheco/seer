// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XXX-dap TODO-doc

// use crate::engine::{
//     Cursor, Engine, EngineEvent, FETCH_BATCH_SIZE, MergeRecord, StepperOptions,
// };
// use crate::event::Event;
// use crate::filter::Filter;
// use crate::position::{ByteLen, ByteOffset, LogStreamPosition, SourceId};
// use crate::render::{RenderOpts, format_event};
// use crate::source::Direction;
// use chrono::{DateTime, Duration, Utc};
// use regex::Regex;
// use std::collections::{HashMap, VecDeque};
// use std::time::{Duration as StdDuration, Instant};
//
// struct StreamWindow {
//     // defines what records we're looking at and what we're displaying
//     filter: Filter,
//     render_options: RenderOpts,
//
//     // stores the UI/operational state about whether we're in the middle of an
//     // operation that will move the anchor
//     long_seek_op: Option<LongSeekOperation>,
//
//     materialized: Materialized,
// }
