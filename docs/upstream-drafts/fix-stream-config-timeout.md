# fix(client_core): unblock pause() during handshake

Branch: `Dayde:upstream/fix-stream-config-timeout` → `alvr-org:master` (base `adaa16ba`)

## Summary

<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (rewrite before posting):

Taking the headset off during the connection handshake froze the client's render loop until the handshake finished or timed out. The handshake held the connection state lock, and `pause()` waits for that lock. This PR takes the lock only once the handshake is done, and checks the lifecycle state while holding it.

One commit: `fix(client_core): unblock pause() during handshake`.

## Problem

- `connection_pipeline` took `ctx.state.write()` right after the control socket connected (`alvr/client_core/src/connection.rs` L155 on `adaa16ba`). It held the lock through the whole handshake, until `wait_rwlock` (L565).
- `ClientCoreContext::pause()` (`lib.rs` L145-159) takes the same lock before it sets the lifecycle state to `Idle`.
- The OpenXR client calls `pause()` on the render-loop thread on `SessionState::STOPPING` (`client_openxr/src/lib.rs` ~L434). Taking the headset off during a handshake therefore blocked the render loop, and delayed `xr_session.end()`, until the handshake finished or failed.
- On master this lasts up to about 2 s: the stream config and StartStream receives each give up after the 500 ms control socket read timeout, and with TCP, accepting the stream socket is bounded by a 1 s listener timeout. Any change that makes the handshake wait longer makes the freeze longer.

## Fix

- The handshake no longer holds the lock. It doesn't need it: the streaming threads, which read the state through `is_streaming()`, are not running yet.
- The pipeline takes the lock once the stream socket is connected, right before it spawns the streaming threads, and checks the lifecycle state while holding it. If the client was paused or is shutting down, it logs "Connection abandoned because the client was paused or is shutting down" and returns.
- `pause()` is unchanged and still sets `Idle` while holding the lock, so there are only two orders, and both are safe:
  1. `pause()` goes first: the pipeline sees `Idle` and returns.
  2. The pipeline goes first: `pause()` waits for the stream to stop, as before.
- During the handshake the state stays `Disconnected`, so `pause()` returns at once. The handshake thread finishes or times out its current step in the background, then stops at the lock check.
- The only other writer, `report_fatal_decoder_error()` (`lib.rs` L283), may set `Disconnecting` during the handshake. The pipeline then overwrites it with `Connecting`, which is harmless because there is no stream to restart yet.

### Rejected alternative

Reordering `pause()` to set `Idle` before taking the lock. The streaming threads could then see `Idle` and `notify_one()` the disconnect condvar before the pipeline waits on it. `alvr_common::wait_rwlock` unlocks first and then waits on a private mutex, so that wake-up can be lost, and both the pipeline and `pause()` would hang.

## Testing

Run in WSL Debian (Linux x86_64) with the toolchain from `rust-toolchain.toml` (1.97.1):

- `cargo fmt --all -- --check`: PASS
- `cargo clippy --workspace --exclude alvr_server_openvr --all-targets -- -D warnings`: PASS (`alvr_server_openvr` needs `prepare-deps`, which was not set up; this PR doesn't touch it)
- `cargo test -p alvr_client_core`: PASS (no tests in the crate; it compiles)
- `cargo build -p alvr_client_openxr`: PASS

**Not tested:** pause/resume on a real headset during a handshake, the MSVC build, and the APK build.

## Related

- A follow-up streamer-side PR will make the Windows streamer send the stream config sooner (the audio device probing that makes the handshake slow). This PR is independent of it.

## Checklist

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...
