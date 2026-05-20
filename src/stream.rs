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

use crate::filter::Filter;
use crate::render::{HostnameDisplay, RenderOpts};
use derive_more::{Display, From};
use iddqd::{IdOrdItem, id_upcast};
use schemars::JsonSchema;
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
    JsonSchema,
)]
#[serde(transparent)]
pub struct LogStreamId(Uuid);

impl LogStreamId {
    /// Returns a freshly-generated random id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

/// A log stream.
///
/// Owns its identity, display name, and active filter.  The set of
/// sources a stream draws from will join the struct once
/// per-stream source restrictions land; for now every stream sees every
/// source the engine knows about.
///
/// Filter ownership lives here (rather than on the display tab) so that
/// when a bookmark targets a stream that has no tab open, opening a
/// fresh tab for that stream restores the user's filter alongside the
/// position.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogStream {
    pub id: LogStreamId,
    pub name: String,
    #[serde(default)]
    pub filter: Filter,
    /// when true, render the structured fields beyond the bunyan header
    /// below each event; defaults to false because most triage starts
    /// from the header line and the user opts in (`F` in the TUI) when
    /// they need the details
    #[serde(default)]
    pub show_extras: bool,
    /// when true, the leading timestamp on each rendered line carries
    /// its `YYYY-MM-DD` prefix; when false, the date is dropped and
    /// only the wall-clock part is shown.  Defaults to true: most
    /// triage spans more than a single day, and you want the date in
    /// view by default.  Toggled with `D` in the TUI.
    #[serde(default = "default_true")]
    pub show_date: bool,
    /// how the bunyan `hostname` field is rendered: short (the
    /// default), full, or omitted.  Set via the `h` field-display
    /// dialog in the TUI.
    #[serde(default)]
    pub hostname_display: HostnameDisplay,
    /// when true, the bunyan `pid` field renders alongside `name` in
    /// the header.  Defaults to false: real Oxide processes restart
    /// often enough that the literal pid number is rarely useful for
    /// triage; users opt in via the `h` field-display dialog when they
    /// do want it.
    #[serde(default)]
    pub show_pid: bool,
    /// when true, the bunyan `name` field renders in the header.
    /// Defaults to true: across merged sources the logger name is the
    /// quickest way to tell which component a given event came from.
    #[serde(default = "default_true")]
    pub show_name: bool,
    /// when true, records render as the raw line bytes from the
    /// source rather than the formatted header/extras layout.  Useful
    /// for inspecting fields the parser dropped or normalized.
    /// Defaults to false; toggled with `R` in the TUI.
    #[serde(default)]
    pub show_raw: bool,
}

fn default_true() -> bool {
    true
}

impl LogStream {
    /// Returns a new log stream with a freshly-generated id, the given
    /// display name, an empty filter, and the same field-visibility
    /// defaults as [`RenderOpts::default`].
    pub fn new(name: String) -> Self {
        let opts = RenderOpts::default();
        Self {
            id: LogStreamId::new_v4(),
            name,
            filter: Filter::default(),
            show_extras: opts.show_extras,
            show_date: opts.show_date,
            hostname_display: opts.hostname,
            show_pid: opts.show_pid,
            show_name: opts.show_name,
            show_raw: opts.show_raw,
        }
    }

    /// Returns a [`RenderOpts`] snapshot of the stream's current
    /// field-visibility settings.  Callers in the TUI use this to
    /// install rendering options on a [`crate::streamview::StreamView`]
    /// when (re)building it.
    pub fn render_opts(&self) -> RenderOpts {
        // Destructure `self` so that adding a new field to `LogStream`
        // is a compile error here: the author of the new field has to
        // decide whether it belongs in `RenderOpts` (add it to the
        // literal below) or not (extend the `_` list).
        let LogStream {
            show_extras,
            show_date,
            hostname_display,
            show_pid,
            show_name,
            show_raw,
            id: _,
            name: _,
            filter: _,
        } = self;
        RenderOpts {
            show_extras: *show_extras,
            show_date: *show_date,
            hostname: *hostname_display,
            show_pid: *show_pid,
            show_name: *show_name,
            show_raw: *show_raw,
        }
    }

    /// Replaces the stream's field-visibility settings with the values
    /// in `opts`.  The persisted-on-disk fields stay individual (so the
    /// session schema can evolve one knob at a time without breaking
    /// older saves), but the TUI dialog mutates them as a bundle.
    pub fn set_render_opts(&mut self, opts: RenderOpts) {
        // Destructure so adding a new `RenderOpts` field fails to
        // compile here until the corresponding `LogStream` field is
        // written through.
        let RenderOpts {
            show_extras,
            show_date,
            hostname,
            show_pid,
            show_name,
            show_raw,
        } = opts;
        self.show_extras = show_extras;
        self.show_date = show_date;
        self.hostname_display = hostname;
        self.show_pid = show_pid;
        self.show_name = show_name;
        self.show_raw = show_raw;
    }
}

impl IdOrdItem for LogStream {
    type Key<'a> = LogStreamId;

    fn key(&self) -> Self::Key<'_> {
        self.id
    }

    id_upcast!();
}
