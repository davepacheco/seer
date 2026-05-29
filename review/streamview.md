# Distinct operations the merged-event iterator must support

Working from what the existing code already does (`StreamView`,
`summary::summarize`, `SearchOp`, `SeekOp`, `SummaryOp`,
`cursor_for_position`), the distinct operations split into roughly
three groups.

## Iteration primitives

1. **Step forward** — return next event after the current position.
2. **Step backward** — return previous event before the current
   position.  Symmetric with (1); the merge has to maintain reverse
   lookbehind.
3. **Bounded step** — same as (1)/(2) but capped at *N* records walked
   or *M* bytes scanned, so a single tick can return "budget exhausted,
   here's where I got to" rather than freezing the UI under a selective
   filter.

## Positioning

4. **Start at a cursor** — physical position (per-source byte offsets).
   This is the bookmark-seek primitive and the natural unit of
   persistence.  `Bookmark` stores a `Cursor` directly; no callsite in
   the tree starts iteration from a `LogStreamPosition`.
   (`Engine::cursor_for_position` exists but has no live callers and
   should be removed.)
5. **Snapshot current cursor** — hand back a cursor for persistence or
   to spawn another iterator on the same spot (used when reconstructing
   a stepper per long-op tick).
6. **Advance to a wall-clock time** — jump forward or backward to the
   first event at-or-after / at-or-before a target `DateTime`.  Today
   this is `<` / `>` on top of `step_forward`/`step_backward`, but it's
   a distinct conceptual op.

## Filter and lifecycle

7. **Swap the event-level filter in place** — retain position, change
   which events pass.  Source-set filter changes require a fresh
   iterator.
8. **Exhaustion check per direction** — distinguish "ran out of budget"
   from "true end of stream", needed to terminate scrolling and
   searches correctly.
9. **Search step** — repeated step + predicate match until hit, with
   bounded variant for long-op chunking.  Arguably composed of (3) +
   a predicate, but it's the operation the UI thinks in.
10. **Full-stream fold** — drive an iterator to EOF without budget,
    for summary and count.  Single forward pass; no buffering needed
    beyond what step provides.

## Telemetry

11. **Bytes / records walked since construction** — progress
    reporting.  Currently lives on both `EventStream` (`bytes_read`,
    `records_parsed`) and `Stepper` (`walked_bytes`); the TODO calls
    out adding `records_parsed`/`bytes_read` on `Stepper` so the
    merge is the single source of truth.

## Observation

One thing worth naming explicitly: (3), (6), (9), and (10) are all
variations on stepping with different stop conditions (budget, time
target, predicate match, EOF).  A sound abstraction would express the
budget and stop condition as parameters of a single step call rather
than four separate methods.  That would also be where `EventStream`
finally disappears — it's just "step with EOF as the stop condition,
no budget" today.

## dap's notes on replacement

Interface is something like:

```rust
struct Viewport { .. }

impl Viewport {
    pub fn set_filter(&mut self, filter: &Filter) { todo!(); }
    pub fn set_render_options(&mut self, render_options: &RenderOptions) { todo!(); }
    pub fn scroll_forward(&mut self, n: usize) { todo!(); };
    pub fn scroll_backward(&mut self, n: usize) { todo!(); };
    pub fn seek_to_start(&mut self) { todo!(); };
    pub fn seek_to_end(&mut self) { todo!(); };
    pub fn search(&mut self, search: &Search) { todo!(); }
    pub fn save_cursor(&self) -> Cursor { todo!(); }
    pub fn seek_to_cursor(&mut self, cursor: &Cursor) { todo!(); }
}
```

EXCEPT that all of these need to have a budget and stop doing stuff.

Also, if the user cancels, then we don't actually want to have changed anything.  I guess we can always save a cursor before we start any of those operations and seek back to it after, but that will still wind up dumping the records and going back to where we were.  That seems like not really what we want.  It seems like what we want is almost to create a new Viewport oriented around the new thing and switch to it?

Let's work through these cases:

- user hits `j`, `^D`, `k`, `^U`
  - usually, we'll have buffered the next few items and this should be fast
  - if it's not fast because we ran out, we have a few options if the user hits
    ^C:
    - ignore it: keep populating.
      - Are they currently locked out from other key presses?  If so, this is
	pretty bad.  If there are no more matching lines and a lot of data, this
	could take a *long* time.
    - scroll back to where they were, with the screen fully populated
      - This seems potentially okay but hardly seems ideal.  Seems a little
	jarring.
    - stop populating, leaving the screen half-populated
      - You'd want some kind of marker indicating that the population stopped,
	right?
      - It would not be clear how you'd reload or get it to try populating
	again.
  - current behavior is that if you ^C, it stops populating and becomes
    responsive again, but it's not clear how you would get it to resume
  - I also note that if you have a selective filter, just navigating with
    "space" a bunch can cause it to get stuck for a while
  - I'm using: `level>=error` and then filtering out some really common ones,
    e.g., msg!="slog-async: logger dropped messages due to channel overflow" msg!="fault management analysis failed!"
- search, advance by time:
  - if you ^C, we should just leave things where they were
- seek to bookmark, front, or back: seems like the navigation cases
  - it would potentially be okay to go back to where you were?  but you might
    want to just stay where the bookmark was, even with an incomplete window
  - in an ideal world, I think we'd still be populating the window, but you
    could still navigate, and that would basically take over whatever population
    was going on in the background.


Note: in testing this, I see that it goes super unresponsive when:
- creating new tab with selective filter
- seeking with selective filter
- stepping ahead large time intervals

I think what I'm realizing is that there are a few different things going on here:

- advancement of the anchor to a specific position
  - when it's a physical position (e.g., bookmark, start, end), this is always
    fast
  - when it's unbounded (search, advance by time, seek forward/backward outside
    what's loaded), this could take a while -- need feedback and ^C, and if you
    ^C, it's probably okay to go back
- population of the window *around* the new anchor
  - this is where it's hard to decide what to do because you've got at least one
    record, maybe more, and that's useful to show.  Could allow ^C to interrupt what it's doing.  Maybe better would just be to allow you to continue to do other things while it populates?  e.g., you can continue to move the anchor around by searching, advancing, etc.

So: what's the abstraction here?

- Viewport
  - has an anchor, similar to today
  - search(&self, search: Search):
    - If a search result is found, returns a new Viewport anchored at the search
      result, somehow sharing the rendered data?
    - If budget is exhausted, existing viewport is unchanged (but state is
      returned/adjusted so that we can update the progress bar and try again)
  - seek(&self, seek: Seek) // by time, up, down, whatever
    - same as above
    - If next anchor is reached within budget, returns new Viewport
    - Otherwise, existing viewport is unchanged but state is returned/adjusted
  - populate(&self) // tries to fill out any gaps in the window

Want a better name for this.  Maybe StreamView isn't the worst?  Window?  It's a window within a very large stream of records.  Is there a difference between what's cached and what's displayed?  (Yes)

There's a few different responsibilities here:

- maintaining an anchor point and moving it through the stream
  (like what Stepper does)
- caching records read from the stream
- caching rendered state for the cached records
- maintaining the UI state
  - are we in a long op?
  - what if the user cancels it

On paper I've been working through some ideas of, say, a sort of multi-stepper.  The idea would be something like the Stepper, but it maintains multiple cursors that it can walk through somewhat independently.  The goal is to be able to have Stepper as it exists today, but then add the ability to iterate the full window of cached records.  This isn't easy today because that iteration works by mutating data structures that we don't want to mutate in this case.  But we could make it work efficiently by instead having basically three cursors:

- the front
- the anchor
- the back

and have records move from back -> anchor -> front -> drop (when scanning forward) and the reverse (when scanning backward).  Actually, that doesn't quite help because iterating the window still requires mutating the structures.  What you need is to be able to instantiate a new cursor-like thing at the front with its own copies of the data structures (presumably using references to the underlying events).  This seems probably doable.  The thinking is that the Viewport or whatever we call this thing would use the stepper to scan toward where it wants, then rebuild the rendered version of the window using the raw records in the Stepper.  This would re-use whatever rendered objects were still relevant and recompute the rest.

The problem with this approach is that if you're seeking along and the user wants to ^C and go back to where they were, you've thrown out all the raw data.  That suggests that this seek process needs to be sort of speculative.  We keep everything we have while we're seeking.  But we don't want to create a whole new parallel state of the world, either.  When there's _not_ a selective filter and the user just hit `j`, this would be a huge waste.

This suggests to me that we need to decouple the raw storage from the seeker, at least somewhat.  Intuitively: we want to create a new Stepper that refers to the data from the first stepper initially.  But it's not mutating its data structures as it seeks -- it's got its own copies.  I'm not sure how to organize this data structure, though.  An obvious step would be to have Stepper operate on *references* to data.  But then where does the actual data live?  How do we know when data can be dropped?  Maybe the Stepper operates on Arcs of records.  This way, when all the existing steppers that refer to a record are gone, then the data is dropped.  If data is shared between two steppers, we don't make any copies.  In fact, when a new stepper that was created for seeking has had to extend the window and has some records in common with the previous one, but not all: then whether the seek is interrupted or completes, the set of records we want to drop are precisely those that are referenced by whichever stepper we're _not_ keeping and _not_ referenced by the one we are keeping.  Sure sounds like an Arc!

So let's say that we have `Stepper`, basically just like it works today, except that:

- the VecDeques contain `Arc<BufferedRecord>`
- `MergeRecord`:
  - contains an `Arc<BufferedRecord>` rather than the same fields
  - uses accessors to return references, rather than exposing the fields
    directly
- it's cloneable
  - deriving would just work
  - refers to all the same buffered data, but has its own copies of the queues,
    etc.
- side note: stores sources as Arc<Source> and access source_id by reference?

Then we have something like:

```rust
struct StreamWindow {
    // defines what records we're looking at and what we're displaying
    filter: Filter,
    render_options: RenderOptions,

    // stores the actual data as well as our current position
    data: Stepper,

    // stores the UI/operational state about whether we're in the middle of an
    // operation that will move the anchor
    long_seek_op: Option<LongSeekOperation>,

    materialized: Materialized,
}

struct LongSeekOperation {
    // cloned from the original one, but moved as we've started seeking
    stepper: Stepper,
    direction: Direction,
    stats: Stats, // maybe stored in the stepper
    end_condition: enum { Time(timestamp), Search(regex) }
}

impl StreamWindow {
    fn new() -> Self { .. }
    fn set_filter(&mut self, ..) -> Self { .. }
    fn set_render_options(&mut self, ..) -> Self { .. }
    fn begin_seek_to_time(&mut self, when) -> {
	// use typestates in caller?  StreamWindowIdle vs. StreamWindowSeeking
    	assert(self.long_seek_op.is_none());

	self.long_seek_op = Some(...)
    }

    fn seek_work(&mut self) -> bool /* is it done? */ {
	// use typestates?
	let Some(seek_op) = self.long_seek_op.unwrap();
    	while have_budget {
	    let next = seek_op.stepper.step(direction);
	    // check end condition
	    if done {
		self.data = stepper;
	    	self.seek_op = None;
		return true;
	    }
	}

	return false;
    }

    fn interrupt(&mut self) {
	// use typestates in caller?  StreamWindowIdle vs. StreamWindowSeeking
    	assert(self.long_seek_op.is_none());
	self.long_seek_op = None;
    }
}

// In seer.rs:

// user starts a seek operation
self.window.begin_seek_...(...)

// each UI tick
if self.window.is_seeking() {
    if self.window.seek_work() {
	assert!(self.window.is_seeking());
	self.populate_window_work();
    }
    // don't work on populating the current window if we're seeking
} else {
    self.populate_window_work();
}
```

Now, how does `populate_window_work()` work?
- has to be fast if nothing's changed
- goal: walk the whole window of cached records associated with the stepper
  (described above how to do that -- but maybe now, it's just clone the stepper, walk backwards N, then walk forwards M?)
- for each one, check if we've got a rendered version
  - if not, render it and add it
- throw away any that we don't want any more
- there's no need to ^C this because it doesn't change anything
  - you can still attempt to seek

Problems with all of this:
- Is Stepper going to try to do too much work with each step if you have a very
  selective filter?  YES -- this is a big problem.
- Can we wind up with duplicate copies of the same record at the same time?
  This would require:
  - start with Stepper A
  - clone it to Stepper B
  - step both A and B -- they will both wind up loading up the same records
  But will this happen in practice?  We never actually wind up stepping from the
  main stepper, `data`.  We always clone it, and we only ever allow one clone at
  a time for seeking.

  We could have a second clone for iterating its data, but we don't actually want that clone to ever load any more data.
  So I think this will be okay in practice?

- A forward seek of one item -- what does this do?
  - Ideally, we're usually only rendering a subset of what we have, so this
    doesn't kick in any of the above machinery
  - When we do seek beyond the window of what we have, we'll create a clone of
    the Stepper, step forward, replace the Stepper, etc. but that's not every time
  - But I don't know if this is self-consistent: we said that `data` was
    supposed to be a Stepper anchored at the current, well, anchor.  That means if you hit `j`, that *does* need to either step that anchor or do the whole dance above.

Another idea (of course?) is to have each source contain a cache of records.  I really don't like this because it's hard to know how to size it and how to evict from it.  If it owns all the data, this is especially hard because if it's too small somehow then you simply can't do the things you need to do.  If it just caches queries, that's better.  But it still feels like we should be able to do better since we understand the lifecycle of these events.

The above problems seem kind of real.  They suggest that the Stepper needs a seek-with-budget, too.  Maybe that's a small extension from what it does now.

But what do we do about the fact that, say, with no filter, aren't we doing kind of a lot of work with each step?  (I'm mostly thinking of the work to clone the Stepper and then drop it.  They're all _references_ to events, but it still seems like a lot.)  Maybe this will be okay?  The alternative is to really maintain a separate window.  It seems worth fleshing this out to better understand the problem and what it would involve.  In this world, you have:

- current "Stepper" level:
  - a SourceWindow with events around the current cursor's spot in this source
- a level above:
  - a MergedWindow containing rendered representations of events whose data is
    currently sitting in the Steppers
- a Viewport that points into some subset of the MergedWindow
  - seeking in the Viewport moves you within the MergedWindow but doesn't move
    the MergedWindow -- and so doesn't move the Stepper -- until you get close
    to the end of the window
Aha!  This gets at why a StreamView today doesn't maintain a stepper.  In this world, the stepper no longer represents the user's anchor, or any other anchor.  In fact, when we have to move the MergedWindow, *that's* when we might clone the Stepper and step a ways

But then if you really are searching or seeking, you kind of want to:
- create a new Seeker around the current anchor
- start seeking it
- if you finish before getting interrupted, create a new MergedWindow around
  the new spot, possibly filling in data from the old MergedWindow?

Something like that.  (Time for a break.)

Maybe the way to think of it is:

Stepper: source + cache of records
MergeWindow:
- can be constituted with a stepper -- steps forward/back to get all the data
RenderedWindow (maybe the same as MergeWindow)
- rendered data associated with each record



Current status:
- I've made a bunch of the prerequisite changes above.
- I've implemented a RenderedWindow that does the populate() bit
- I've implemented a Viewport that contains a RenderedWindow and supports seeking
- In principle, I may need to rethink some of how seer stores its state since I may have moved some of it (the long ops, etc.) into the new Viewport
- What remains is to identify the gaps between what this supports and what seer needs
  - cursor_at_anchor() is used to:
    - compute byte offset into the merged stream
    - save session state
    - refresh tab after changing filter?  will this go away?
  - materialized() -- trivial
  - advance_time() -- I think that's easy?
  - cursor_before_record(): given index -- used to create a bookmark.  I think this is doable?
  - to figure out:
    - scroll_lines()
    - done: seek_to_cursor()
    - Tab::seek_active_to_end() / seek_active_to_start() / seek_active_to_cursor()
  - to re-review:
    - Seer's Tab::refresh() -- it depends on how this gets called.  This may get eliminated?
    - Seer's Tab::rerender() -- will this go away too?
    - Seer's Tab::resync_from_streamview() -- WTF is this
    - advance_seek_op() / advance_search_op() / finalize_seek_op()
