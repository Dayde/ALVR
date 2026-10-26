# fix(sockets, client_core): wait for the stream config instead of timing out after 500 ms

Branch: `Dayde:upstream/fix-stream-config-timeout` → `alvr-org:master` (base `adaa16ba`)

## Summary

<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (rewrite before posting):

The core change is in `alvr_sockets`. `framed_recv`, which every control-socket `recv()` uses, now waits for the whole `timeout` it is given. Before, it returned `TryAgain` as soon as one `peek`/`read` hit the socket read timeout. That read timeout is 500 ms on the client, so every client handshake wait gave up after 500 ms, whatever timeout it asked for.

On top of that, the client waits up to 15 s for the stream config, as a pragmatic tolerance for a streamer that is slow to answer (Windows audio device probing). This wait must not freeze the headset when it is taken off, so the client now:

- waits in 500 ms steps that check the lifecycle state, and
- no longer holds the connection state lock during the handshake.

Commits (each builds, passes clippy and its tests on its own):

1. `fix(client_core): don't block pause() on the handshake`: take `ctx.state.write()` only once the stream socket is connected, and check the lifecycle state while holding it. This fixes a pre-existing render-loop freeze of up to about 2 s on master, and must come before the longer waits.
2. `fix(sockets): make framed_recv wait for the whole timeout`: loop until the deadline; clamp the last wait with a small RAII guard that restores the socket read timeout; report a 0-byte read (peer closed) as an error instead of spinning.
3. `fix(sockets): keep partial packets in ProtoControlSocket between recv calls`: the buffer and cursor live in the socket, as in `ControlSocketReceiver`, and are handed over on `split()`. Needed for the stepped wait in commit 4, and it also fixes a latent desync.
4. `fix(client_core): wait up to 15 s for the stream config`: the 15 s wait, in 500 ms steps; returns early when the lifecycle leaves `Resumed`.

**Splitting:** commit 1 (pause) only relies on the lifecycle re-check under the lock and works on its own on master. If maintainers prefer, it can be split into its own PR, so that a discussion about the 15 s value does not hold up the lock and socket fixes (commits 1-3 are useful without commit 4).

## Problem

On Windows, the streamer can be slow to answer while it probes the audio devices. This happens when game audio and/or the microphone are pinned to a device (VB-Cable, VAC, a named device). The headset then shows "os error 11" (`Resource temporarily unavailable`, EAGAIN) and never starts streaming. The streamer logs a handshake failure (10053/10054 because the client closed the socket), and the cycle repeats.

## Root cause

1. **The streamer does slow work between `ConnectionAccepted` and `StreamConfigPacket` (Windows only).** In `alvr/server_core/src/connection.rs` on master (`adaa16ba`):
   - L818-845 (`#[cfg(target_os = "windows")] let game_audio_sample_rate = ...`):
     - `alvr_audio::new_output(game_audio_config.device…)` (L821-822)
     - `alvr_audio::new_virtual_microphone_pair(...)`, used only for the VAC/VB-Cable loopback check (L831-838)
     - `alvr_audio::input_sample_rate(&game_audio_device)` (L842)
   - L850 builds `StreamConfigPacket::new(...)` with that `game_audio_sample_rate`. L895 `SocketConnection::from_client_connection(...)` then sends it (`alvr/sockets/src/lib.rs` L166), followed right away by `StartStream` (L168-170).
   - On Linux/macOS the sample rate is a constant (L815-816) and audio is opened only after the handshake, so this delay is Windows-only.
2. **`framed_recv` never waited longer than the socket read timeout (all platforms).** `framed_recv` (`alvr/sockets/src/control_socket.rs` L102-149) did `socket.peek(..).handle_try_again()?` (L116), so it gave up on the first timed-out peek. The payload `read` loop had the same flaw. The socket read timeouts are:
   - client: 500 ms. `ProtoControlSocket::connect_to(SOCKET_INIT_RETRY_INTERVAL, PeerType::Server(..))` (client `connection.rs` L145-148) → `accept_from_server` L51. Then `split(STREAMING_RECV_TIMEOUT)` (L209 → `control_socket.rs` L224) sets 500 ms again.
   - streamer: 1 s. `connect_to_client(.., Duration::from_secs(1))` (server `connection.rs` L513-516) → `control_socket.rs` L77.

   So the client's `recv::<StreamConfigPacket>(HANDSHAKE_ACTION_TIMEOUT /* 2 s */)` really waited 500 ms.

### Why fix the client and not (only) the streamer?

An obvious alternative is to send the stream config before the slow audio probing. That is what the closed #3366 did for the v20 branch; it was declined for v20, and its master equivalent was never submitted. On master that is only partly possible:

- The stream config **depends on the probe result**: `game_audio_sample_rate` comes from `input_sample_rate(&game_audio_device)` (L842) and is part of `ClientNegotiatedStreamingConfig`. Opening the game audio output device therefore has to happen before the config can be sent.
- The VB-Cable/VAC loopback check (`new_virtual_microphone_pair` + `is_same_device`, L824-839) does not feed the config. It could move after the handshake or into the microphone thread. That would be a good follow-up streamer-side PR, but it only removes part of the delay.
- The socket fix is needed either way: without it, *every* client-side handshake timeout is really 500 ms, so a streamer that takes 0.6 s fails just the same. The 15 s is a tolerance on top of that, and it keeps newer clients working with streamers that are still slow (older versions, slow drivers).

## Fix

### Sockets (commits 2-3)

- **Wait until the deadline.** `ConnectionError::TryAgain` from `peek`/`read` now means "no data yet", and `framed_recv` loops until its deadline.
- **No busy spin.**
  - Each retry is a blocking `peek`/`read` bounded by the socket read timeout. Every control socket has one: `bind`, `accept_from_server`, `connect_to_client` and `split` all set it, and none of them is non-blocking.
  - A 0-byte `peek`/`read` (peer closed) now returns `ConnectionError::Other("Connection closed by peer")`. Before, it spun until the deadline. With a 15 s wait that would be 15 s of 100 % CPU on the headset whenever the streamer drops the socket, which it does on protocol mismatch or standby (server `connection.rs` L585-600).
  - The only remaining spin is when 1-3 bytes of the 4-byte length prefix have arrived. It is unchanged from before and lasts until the next TCP segment arrives or the deadline passes.
- **Deadline honoured.** After a read times out with time still left, the `ReadDeadline` guard reads the socket read timeout (once) and, if the time left is shorter, sets the read timeout to the time left. The guard restores the original value on drop.
  - This only happens on the idle path, after a read already blocked, so a packet that is already waiting costs no extra syscalls.
  - I chose this over "overshoot by at most one socket timeout" so that a read that wakes slightly early cannot cost another full socket timeout.
  - Limitation: if `timeout` is shorter than the socket read timeout, the first read still blocks for the full socket timeout, as before. This is the case for the streamer's streaming loop (500 ms recv on a 1 s socket).
- **Concurrency.** Each control socket is shared with its `try_clone()`s (sender halves), but only one thread ever reads it and the sender halves never read. Changing the read timeout for a moment therefore affects no other reader.
- **Partial packets (commit 3).** `ProtoControlSocket` keeps its receive buffer and cursor, so a `recv()` that times out mid-payload resumes on the next call. Before, the rest of the payload would have been parsed as a new length prefix.

### Client (commits 1 and 4)

- **Stream config wait (commit 4).** `STREAM_CONFIG_TIMEOUT = 15 s`, waited in `SOCKET_INIT_RETRY_INTERVAL` (500 ms) steps. Between steps the client returns `Ok(())` if the lifecycle is no longer `Resumed` (pause or shutdown). Thanks to commit 3, a step that times out mid-packet does not lose data.
- **StartStream is unchanged.** It keeps `HANDSHAKE_ACTION_TIMEOUT`: the streamer sends it right after the config. *Deviation from the original fork commit*, which used 15 s for it too.
- **Connection state lock (commit 1).**
  - Before, `connection_pipeline` took `ctx.state.write()` right after the control socket connected (master `connection.rs` L155) and held it until `wait_rwlock` (L565). `ClientCoreContext::pause()` (`lib.rs` L145-159) takes that lock *before* setting the lifecycle state. `client_openxr/src/lib.rs` ~L439 calls `pause()` on the render-loop thread on `SessionState::STOPPING`, so taking the headset off during a handshake blocked the render loop and delayed `xr_session.end()` until the handshake finished or timed out.
  - **This is pre-existing:** up to about 2 s on master. The lock covers the stream config receive and the StartStream receive, each cut to 0.5 s by the control socket read timeout (bug 2), and, with TCP, accepting the stream socket (`listen_for_server(Duration::from_secs(1), ..)`, accept bounded by the 1 s listener timeout). Without commit 1, the longer waits of commits 2 and 4 would grow it to about 18 s; that is why commit 1 comes first.
  - The handshake does not need the lock. The streaming threads, which read the connection state through `is_streaming()`, are not running yet. The only other writer, `report_fatal_decoder_error()` (`lib.rs` L283), may set `Disconnecting` during the handshake; the pipeline then overwrites it with `Connecting`, which is harmless because there is no stream to restart yet. So the lock is now taken only after the stream socket is connected, right before the streaming threads are spawned, and the lifecycle state is checked while holding it. If that check fails, the pipeline logs "Connection abandoned because the client was paused or is shutting down" and returns.
  - `pause()` still sets `Idle` while holding the lock, so there are only two orders, and both are safe:
    1. `pause()` went first. The pipeline sees `Idle` and returns.
    2. The pipeline went first. `pause()` waits for the stream to stop, exactly as before.
  - During the handshake, `pause()` finds the state `Disconnected` and returns at once. The pipeline then stops at the next 500 ms step, or at the lock check.
- **Rejected alternative: reorder `pause()`** to set `Idle` before taking the lock. I did not do it: it would let the streaming threads see `Idle` and `notify_one()` the `disconnect_notif` before the pipeline waits on it. `alvr_common::wait_rwlock` unlocks first and then waits on a private mutex, so such a wake-up can be lost, and both the pipeline and `pause()` would hang.

### Callers of `framed_recv` (shared code)

| Caller | Timeout vs socket read timeout | Effect |
|---|---|---|
| client stream config wait | 500 ms steps vs 500 ms, up to 15 s | waits up to 15 s, interruptible |
| client StartStream `recv(HANDSHAKE_ACTION_TIMEOUT)` | 2 s vs 500 ms | really waits 2 s now |
| client streaming `control_receiver.recv(STREAMING_RECV_TIMEOUT)` | 500 ms vs 500 ms | unchanged |
| streamer `connect_to_client` → `recv(timeout)` | 1 s vs 1 s | unchanged |
| streamer StreamReady `recv(HANDSHAKE_ACTION_TIMEOUT)` (`lib.rs` L172) | 2 s vs 1 s | really waits 2 s now (was 1 s) |
| streamer streaming `control_receiver.recv(STREAMING_RECV_TIMEOUT)` | 500 ms vs 1 s | unchanged (blocks up to 1 s, as before) |
| `SocketConnection::from_server_connection` (`lib.rs` L201) | timeout vs caller's socket | waits the full timeout; unused by the client on master |
| any caller, peer closed | — | immediate `Other` error instead of a CPU spin followed by `TryAgain`; the streaming loops already treat `Other` as "disconnected" |

## Testing

Run in the worktree, on Linux x86_64, with the toolchain from `rust-toolchain.toml`:

- `cargo fmt --all -- --check`: PASS. `cargo xtask format` then `git diff --exit-code`: PASS (no changes).
- `cargo xtask clippy --ci`: PASS (exit 0, 0 warnings, 0 errors)
- `cargo test -p alvr_sockets`: PASS (4 passed). New tests, all on a real 127.0.0.1 TCP pair:
  - `framed_recv_waits_past_socket_read_timeout`: read timeout 100 ms, packet sent after 300 ms. `recv(2 s)` succeeds and the read timeout is still 100 ms afterwards.
  - `framed_recv_times_out_at_deadline`: read timeout 300 ms, `recv(400 ms)` returns `TryAgain` after at least 400 ms and before 550 ms; without clamping it would take 600 ms. I checked that the test fails when clamping is disabled. The read timeout is restored.
  - `framed_recv_fails_fast_when_peer_closes`: `recv(5 s)` returns `Other` in under 1 s.
  - `proto_control_socket_keeps_partial_packet_between_recv_calls`: half a packet, then `recv` times out; the rest arrives, then `recv` returns the packet.
  - The suite runs in about 0.4 s and passed 15 consecutive runs.
- Each commit checked on its own: `cargo test -p alvr_sockets`, `cargo clippy -p alvr_sockets -p alvr_client_core --all-targets` (no warnings) and `cargo fmt --check`.
- `cargo test -p alvr_client_core`: PASS (no tests in crate; compiles)
- `cargo test -p alvr_session`: PASS (4 passed)
- `cargo build -p alvr_client_openxr`: PASS
- `cargo build -p alvr_server_core`: PASS
- `cargo check --target x86_64-pc-windows-gnu -p alvr_sockets` / `-p alvr_client_core`: PASS for both (mingw only approximates the MSVC CI)
- `cargo check --target aarch64-linux-android -p alvr_sockets` / `-p alvr_client_core`: PASS for both (`cargo check` only, nothing linked)

**Not tested:** a real Windows streamer with pinned audio devices plus a headset. The original fork commit (`be47952e` on v20.14.1) was tested on hardware; this re-implementation on master was not, and neither were the pause/resume behaviour on a headset and the client lifecycle changes in commits 1 and 4. Also not tested: MSVC build, APK build (`cargo xtask build-client`, the NDK is not available here) and macOS.

## Related issues/PRs

- #3365 "handshake error when mic is active" (open). This may fix the slow-enumeration variant. A commenter traced at least one case to VB-Cable device naming instead: `new_virtual_microphone_pair` fails and the streamer drops the socket. This PR does not fix that case; the client now just fails fast instead of timing out.
- #3381 "cannot reconnect due to os error" (open). Windows + VB-Cable; it may be the same slow-enumeration variant, unconfirmed. The reporter says it persists until the PC is restarted and mentions 10053/10054, which hints at a streamer-side problem rather than (only) a slow handshake.
- #3221, #3231, #3047, #2900, #2491, #2296: older "os error 11" handshake reports, mostly closed as stale. Same symptom; the causes may differ.
- **#3366 "fix(audio): prevent handshake timeouts and duplicate client connections"** (closed, not merged). A streamer-side approach (move virtual-mic enumeration out of the handshake) on the v20 branch, which the maintainer declined for v20. It does not conflict with this PR and would complement it; see "Why fix the client".
- #2944 "fix: bump cpal+rodio, fix os error 11 timeout" (merged 2025). It made device enumeration faster; the timeout behaviour stayed the same.
- #3321 "unified socket abstraction" (merged). This PR builds on it.
- #2718 "unified sockets (1): server" (open draft). It touches the same area and could conflict if revived.
- Searches for "framed_recv", "StreamConfigPacket", "stream config timeout", "stuck connecting" and "Resource temporarily unavailable" found nothing relevant.

## Risks

- **Shared code.** `framed_recv` is used by client and streamer. Some handshake waits really last as long as their nominal timeout now: client StartStream 2 s, streamer StreamReady 2 s. A peer close is now an immediate error instead of a delayed `TryAgain`.
- **Render thread on pause (commit 1).** During the handshake, `pause()` no longer waits; it returns immediately. The handshake thread may run up to one more 500 ms step, or finish the StartStream/StreamReady/stream-socket steps (≤ 2 s each), before it notices and drops the connection. Meanwhile the render thread continues to `xr_session.end()`. If `pause()` comes after the lock is taken (streaming threads starting or running), it waits for the stream to stop, as on master.
- **Lost wake-up in `wait_rwlock`.** `alvr_common::wait_rwlock` can lose a wake-up: it unlocks, then waits on a private mutex. This is pre-existing and not changed here, and is why `pause()` is not reordered.
- **Shutdown.** `ClientCoreContext` shutdown (`Drop` → `ShuttingDown` + join) returns within one 500 ms step of the stream config wait, or within the remaining ≤ 2 s handshake steps.
- **Winsock socket state.** On Windows, Winsock documents the socket state as "indeterminate" after an `SO_RCVTIMEO` timeout. ALVR already retries after timeouts in every streaming loop, so this PR adds no new exposure, but it now also retries during the handshake.

## Checklist

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...
