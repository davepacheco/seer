// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Library crate behind the `seer` and `seeit` binaries.

pub mod engine;
pub mod event;
pub mod source;

#[cfg(test)]
mod test_util;

pub use engine::Engine;
pub use event::{Event, Hostname, Level, LoggerName, Pid, UnknownLevel};
pub use source::{FileSource, Source, SourceError, SourceId};
