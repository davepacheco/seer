// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Schema fixture for [`seer::Session`].
//!
//! This is the tripwire described in `plan-sessions.md`.  The test
//! regenerates the JSON Schema for `Session` via [`schemars`] and
//! diffs it against the checked-in fixture; any accidental change
//! to the on-disk shape fails CI.  When the change is intentional,
//! bump [`seer::CURRENT_SESSION_VERSION`] and refresh the fixture
//! with `EXPECTORATE=overwrite cargo nextest run -p seer
//! session_schema_matches_fixture`.

use schemars::schema_for;
use seer::Session;

#[test]
fn session_schema_matches_fixture() {
    let schema = schema_for!(Session);
    let body = serde_json::to_string_pretty(&schema).unwrap() + "\n";
    expectorate::assert_contents(
        "tests/fixtures/session.schema.json",
        &body,
    );
}
