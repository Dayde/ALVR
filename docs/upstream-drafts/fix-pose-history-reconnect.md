# fix(server_openvr): clear the head pose history when a client connects

Branch: `Dayde:upstream/fix-pose-history-reconnect` (base `alvr-org/ALVR` `master` @ `adaa16ba`)
Companion PR (client-side safety clamp): `Dayde:upstream/fix-display-time-ahead` — link: <!-- TODO(Dayde): link the other PR once opened -->

## Summary
<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (for you to rewrite): when a client connects, the streamer now clears both head
pose histories used to stamp video frames — the C++ `PoseHistory` (in `Hmd::StartStreaming`) and the
Rust `HEAD_POSE_QUEUE` (in the `ClientConnected` handler). Previously, until the new client's first
tracking sample arrived, frames were matched against the previous session's poses and sent with the
previous session's timestamps, which are in a different client clock.

## Problem

After a reconnect, the first frames of a new session could carry a timestamp from the previous
session, which may be in a different client clock. Observed on the user's Monado setup: after
restarting `monado-service`, such timestamps were far ahead of the new session's clock, `xrEndFrame`
blocked and the stream froze.
<!-- TODO(Dayde): confirm/cite in Monado source how XrTime is based and that xrEndFrame waits for the display time. -->

## Root cause

Findings on master (`adaa16ba`):

1. **History is not cleared on a new connection/stream start.** `PoseHistory` (`alvr/server_openvr/cpp/alvr_server/PoseHistory.{h,cpp}`)
   is created once in the `Hmd` constructor and only ever appended to / popped at 360 entries. On the
   Rust side, `HEAD_POSE_QUEUE` (`alvr/server_openvr/src/lib.rs`) is a process-wide static; the
   `ClientConnected` handler clears `FOVEATION_CENTER_QUEUE` but not it. (The Rust `TrackingManager` in
   `server_core` *is* per-connection, so it's not affected.)
2. **Tie-breaking is already fixed on master.** #3368 (via #3398) made `GetBestPoseMatch` search
   newest first and stop at a 250 ms window relative to the newest entry. Once one new-session pose is
   in the buffer, older-session entries are excluded: if their timestamps are larger, the unsigned
   `newest - entry` wraps to a huge value; if smaller, they're > 250 ms older. So the remaining hole is
   only between connect and the first tracking sample of the new client — but in that interval the
   buffer holds *only* old-session poses, so every frame matches one (the reprojection path's
   `GetLatestPose` on Linux likewise returns an old-session pose).
3. **Timestamp domain.** Stamps are the client's `poll_timestamp`, which the client sets to
   `target_time = xr_now + prediction offset` (`client_openxr/src/stream.rs`, `stream_input_loop`), i.e.
   the client runtime's `XrTime`. There's no server-side time sync, so a previous session's entries are
   in an unrelated time base whenever the runtime restarted.
   `VideoSend` then finds the stale stamp in the also-stale `HEAD_POSE_QUEUE` and sends the frame.

## Fix

- `PoseHistory::Clear()` (new, takes the mutex, clears the buffer); called from `Hmd::StartStreaming()`,
  which `InitializeStreaming()` calls on every client connect, on both Windows and Linux. No FFI change.
- `HEAD_POSE_QUEUE.lock().clear()` in the `ClientConnected` handler, next to the existing
  `FOVEATION_CENTER_QUEUE` clear.

On master, the Rust `HEAD_POSE_QUEUE` clear alone already makes previous-session frames
undeliverable: their stamps are no longer in the queue, so `VideoSend` drops them. The C++ `Clear()`
keeps the C++ history consistent with it, so pose matching and the Linux reprojection path
(`GetLatestPose`) don't depend on the 250 ms window / unsigned wrap-around to skip previous-session
poses. Both changes are kept. Order: the Rust queue is cleared first (event loop thread), then the C++
history inside `InitializeStreaming`; a frame SteamVR submits in between can still match an old C++
entry, but `VideoSend` drops it. Tracking for the new client is delivered through the same event loop,
after `ClientConnected`, so new-session poses are never cleared.

Behaviour until the first tracking sample: `GetBestPoseMatch` returns nothing → stamp 0 → `VideoSend`
drops the frame ("can't submit the frame without its pose"), exactly what happens on the very first
connection after SteamVR starts. The client asks for an IDR when it gets non-IDR frames while waiting
for one, so the stream starts normally. A Linux encoder job that was already in flight still encodes,
but its frame is dropped by `VideoSend` because its stamp is not in the cleared queue.

Tie-breaking: already "newest wins" on master (#3368), so no change needed.

### Why both this and the client clamp

This PR removes the cause on the streamer. The client PR (clamp display times more than 100 ms after
the predicted vsync) protects clients against any streamer version that still has the bug (or any
future bug that produces a far-future stamp), since the failure mode in some runtimes is an
unrecoverable freeze.

## Testing

Run in a worktree on `adaa16ba` + this commit, `CARGO_TARGET_DIR` shared, `CARGO_INCREMENTAL=0`:

- `cargo fmt --all -- --check` — pass
- `cargo xtask format` (includes clang-format for C++) then `git diff --exit-code` — pass
- `cargo xtask clippy --ci` — pass, zero diagnostics
- `cargo build -p alvr_server_openvr` — pass (only pre-existing C++ warnings, e.g. unused `originalIPD` in `HMD.cpp`, unused `set_hwframe_ctx` in `EncodePipelineVAAPI.cpp`)
- `cargo test -p alvr_server_openvr` — pass, 0 tests (the test binary needs `LD_LIBRARY_PATH=openvr/lib/linux64` to load `libopenvr_api.so`; that's environmental, same on master)
- `cargo test -p alvr_server_core` — pass, 0 tests; `cargo test -p alvr_session` — pass (4 tests)
- `cargo check -p alvr_server_openvr --target x86_64-pc-windows-gnu` — **not checked**: the build script
  passes MSVC's `/permissive-` to mingw g++, which fails on master too ("cannot specify '-o' with '-c'
  ... with multiple files"). CI uses MSVC; mingw is only an approximation anyway.
  `cargo check -p alvr_server_core --target x86_64-pc-windows-gnu` — pass (unchanged crate, sanity).
- The repo has no C++ test infrastructure, so no test is committed. A throwaway standalone program
  compiled `PoseHistory.cpp` with g++ and checked: (a) a buffer of only previous-session poses (clock
  far ahead) matches a stale entry; (b) after `Clear()`, `GetBestPoseMatch`/`GetLatestPose` return
  nothing; (c) new-session poses then match, newest wins on ties; (d) without `Clear()`, once one
  new-session pose is present the 250 ms window already excludes the stale entries. All passed.

Not tested: a live reconnect against a headset with this rebased branch (the original freeze was seen
on the v20.14.1-based fork, which lacked #3368).

## Related issues/PRs

- #3368 (merged into `linux-direct-mode`, on master via #3398) — newest-first match within a window;
  this PR builds on it and covers the remaining connect → first-tracking gap. No conflict.
- #3353 (closed, not merged) — present-time fallback for pose matching; touched `PoseHistory` but not
  history clearing. #3342 (closed, not merged) — latest-pose fallback when matching fails. If something
  like these lands later, it should also see an empty history after connect (which this PR provides).
- #811 (closed, 2021) — reconnect image freeze on Quest; old, probably unrelated, for reference.
- Searches ("PoseHistory", "GetBestPoseMatch", "xrEndFrame", "display time", "timestamp future",
  "Monado freeze", "freeze reconnect") found no existing fix for clearing on reconnect.

## Risks

- First frames after a reconnect are dropped until the first tracking sample (tens of ms), as on the
  very first connection. Previously they were sent with a possibly wrong stamp.
- `Hmd::StartStreaming()` is also the hook for any future stream-start re-entry; clearing there on
  every call is intentional.

## Checklist

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...

<!-- Pre-submit checklist (for Dayde, invisible when pasted):
- Live reconnect test on a Windows streamer
- Live reconnect test on a Linux streamer
-->
