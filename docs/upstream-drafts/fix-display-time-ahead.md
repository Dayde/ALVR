# fix(client_openxr): clamp far-future display times

Branch: `Dayde:upstream/fix-display-time-ahead` (base `alvr-org/ALVR` `master` @ `adaa16ba`)
Companion PR (streamer side, removes the cause): `Dayde:upstream/fix-pose-history-reconnect` — link: <!-- TODO(Dayde): link the other PR once opened -->

## Summary
<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (for you to rewrite): the client now refuses to hand the OpenXR runtime a frame
display time that is more than 100 ms after the predicted vsync. Such a timestamp can only come from
a stale streamer stamp (e.g. a previous session's clock); on the user's Monado setup, passing such a
timestamp was observed to block `xrEndFrame`, freezing the stream. This mirrors the existing clamp for timestamps more than
1 s in the past. The first replacement is logged with `warn!`, and the count is logged with `info!`
when the stream ends.

## Problem

After reconnecting a client to a running streamer, the stream sometimes froze right at start:
`xrEndFrame` blocked and no further frames were shown. Seen with Monado on Linux in roughly one
client start out of three during the first minute or two after `monado-service` was restarted, with
the headset lying still.

## Root cause

- The streamer stamps each frame by matching the head pose SteamVR rendered with against its own
  head pose history (`PoseHistory::GetBestPoseMatch`, `alvr/server_openvr/cpp/alvr_server/PoseHistory.cpp`),
  and `VideoSend` looks the stamp up in `HEAD_POSE_QUEUE` (`alvr/server_openvr/src/lib.rs`).
  Neither is cleared when a client connects.
- The stamps are the client's `poll_timestamp` (= tracking poll time + prediction offset), i.e. in the
  client runtime's `XrTime` clock, so a previous session's stamps are not guaranteed to be in the new
  session's time base.
- Until the new client's first tracking sample arrives, frames can only match the previous session's
  poses, so they are sent with the previous session's timestamp, which the client passes as
  `displayTime` to `xrEndFrame`.
- Observed on the user's setup: after restarting `monado-service`, the stale timestamps were far
  ahead of the new session's `XrTime`, and `xrEndFrame` blocked (seemingly until that display time),
  so the frame loop stopped.
  <!-- TODO(Dayde): confirm/cite in Monado source that XrTime is relative to monado-service start and
  that xrEndFrame waits for the display time; otherwise keep this as observed behaviour. -->
- On the original v20.14.1 base this was much more likely, because `GetBestPoseMatch` searched oldest
  first over the whole ~1.7 s buffer (oldest entry won ties, so a still headset kept matching old
  entries even after new tracking arrived). Master already has #3368 (newest first, 250 ms window,
  merged via #3398), which excludes stale entries once a new-session pose is present; the remaining
  window is "connect → first tracking sample". The companion PR closes that on the streamer side.

## Fix

In `StreamContext::render` (`alvr/client_openxr/src/stream.rs`), after the existing
`max(timestamp, vsync - 1 s)` clamp: if the display time is more than `MAX_DISPLAY_TIME_AHEAD`
(100 ms) after the predicted vsync, use the predicted vsync instead. A per-stream counter (field on
`StreamContext`, which is recreated for each stream) logs the first occurrence with `warn!` and the
total with `info!` in `Drop`. The frame-loop statistics the original fork commit hooked into don't
exist on master, so this is kept to a counter and two log lines.

Why 100 ms is safe for legitimate frames: the frame timestamp is `poll_time + get_total_prediction_offset()`,
and the prediction offset is `min(average total pipeline latency, max_prediction_ms)`. The frame is
displayed at a vsync after it was decoded, which is after `poll_time`. So the timestamp can exceed the
vsync by at most `max_prediction_ms` minus the real pipeline latency. With the default
`max_prediction_ms = 100` it can never exceed 100 ms. Only if a user raises `max_prediction_ms`
(slider goes to 200) *and* latency suddenly drops by >100 ms could a real frame be clamped, and then
the effect is just that the frame is submitted at the predicted vsync — which is exactly what the
client already does on every frame when `use_custom_reprojection` is on (YVR/PFDMR). High-latency
networks make the timestamp *earlier* than the vsync, never later, so they're unaffected.

Android/Quest: `client_openxr` is the Quest/Pico/etc. client too; the change is platform-independent
and only triggers on timestamps that are > 100 ms in the future, which the reasoning above shows
don't occur in normal operation.

### Why both this and the streamer fix

The streamer fix removes the known cause. This clamp protects clients against any streamer
(older streamer versions that clients still connect to, other streamer backends, future bugs) that
sends a display time far in the future — an otherwise unrecoverable freeze in some runtimes — at no
cost in normal operation.

## Testing

Run in a worktree on `adaa16ba` + this commit, `CARGO_TARGET_DIR` shared. Clippy and the build need
the openvr submodule (`git submodule update --init`), since `alvr_session`'s build script reads
`openvr/headers`.

- `cargo fmt --all -- --check` — pass
- `cargo xtask format` then `git diff --exit-code` — pass (no changes)
- `cargo xtask clippy --ci` — pass, zero diagnostics (0 `::warning`/`::error` lines)
- `cargo build -p alvr_client_openxr` — pass
- `cargo test -p alvr_client_openxr` — pass (crate has no tests, 0 run)
- `cargo test -p alvr_session` — pass (4 tests)
- `cargo check -p alvr_client_openxr --target aarch64-linux-android` — pass. `cargo clippy` for that
  target shows only pre-existing warnings in `alvr_client_core`/`alvr_system_info` Android code,
  none in `client_openxr`. (No NDK available, so not built/linked; `cargo xtask build-client` not run.)

Not tested on this rebased branch: running against a headset. The same logic ran on the
v20.14.1-based Linux client fork (Monado), where the freeze stopped reproducing. Not tested on Quest.

## Related issues/PRs

- #3368 (merged into `linux-direct-mode`, landed on master via #3398) — "match poses newest first within a
  bounded window": fixes the tie-breaking half of the root cause; does not clear history on connect.
- #3353, #3342 (closed, not merged) — alternative pose-matching fallbacks; no overlap with this change.
- #811 (closed, 2021) — "Reconnecting to Server with Oculus Quest causes the image to freeze": old,
  likely a different cause, listed for reference only.
- Searches for "display time", "xrEndFrame", "timestamp future", "Monado freeze", "freeze reconnect",
  "GetBestPoseMatch" found no existing fix or conflicting PR.

## Risks

- A legitimate frame is clamped only with `max_prediction_ms` > 100 plus a sudden latency drop; effect
  is submission at the predicted vsync (same as the custom-reprojection path). Low.
- The 100 ms constant mirrors the existing 1 s clamp for timestamps in the past and matches the default
  `max_prediction_ms`. It is not tied to that setting; if reviewers prefer, it could be raised to 200 ms
  (the slider maximum) or derived from `max_prediction_ms`.

## Checklist

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...

<!-- Pre-submit checklist (for Dayde, invisible when pasted):
- Live test on a headset with Monado (reconnect after restarting `monado-service`) with this branch
- Quick sanity run on Quest
-->
