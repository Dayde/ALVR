# fix(server_core): defer mic loopback check

Branch: `Dayde:upstream/fix-streamer-audio-handshake` → `alvr-org:master` (base `adaa16ba`)

Compare: https://github.com/alvr-org/ALVR/compare/master...Dayde:ALVR:upstream/fix-streamer-audio-handshake

## Summary

<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (rewrite before posting):

On Windows, with game audio enabled and the microphone set to VAC or VB-Cable, the streamer enumerated all output and input audio devices between accepting the client and sending `StreamConfigPacket`. That enumeration only fed the "Game audio and microphone cannot point to the same device!" check, not the stream config. This PR moves the check after the handshake, next to where the microphone pair is already opened, so the client gets its stream config sooner.

One commit: `fix(server_core): defer mic loopback check`.

## Problem

Between `ConnectionAccepted` and the stream config, the Windows streamer did three things (`alvr/server_core/src/connection.rs` L818-845 on `adaa16ba`):

1. `alvr_audio::new_output(game_audio_config.device)`: opens the game audio device. With a pinned device this enumerates the outputs and calls `name()` on each.
2. For VAC/VB-Cable microphones only: `new_virtual_microphone_pair(...)`, which enumerates outputs and inputs again, then `is_same_device(&game_audio_device, &sink)`.
3. `input_sample_rate(&game_audio_device)`, whose result is `game_audio_sample_rate` in the stream config.

The client waits a short time for the stream config (see `upstream/fix-stream-config-timeout`). When the audio drivers are slow to enumerate, the headset shows "os error 11" and the connection loops.

Step 2 is then repeated after the handshake anyway: the microphone setup calls `new_virtual_microphone_pair(config.devices)` again (L1047 on `adaa16ba`).

## Fix

- The pre-config block now only opens the game audio device and reads its sample rate (steps 1 and 3). Both feed the stream config, so they stay before it. Step 1 is the only enumeration left there; step 3 does not enumerate.
- The VAC/VB-Cable loopback check moves to the microphone setup, after `new_virtual_microphone_pair` (L1036-1052). It reuses the game audio `Device` opened for the sample rate, so it adds no enumeration.
- Same error message, same conditions: game audio enabled, microphone enabled with VAC or VB-Cable, and the sink is the game audio device. VoiceMeeter, Custom and Automatic are still not checked, as before.

**Behaviour change:** with that misconfiguration, the connection now fails right after the handshake (before the streaming state is set and before any thread streams) instead of before the stream config. The streamer logs the same error. The headset sees a disconnect after the handshake instead of a handshake failure. A missing VAC/VB-Cable device likewise now fails after the handshake, which master already did for VoiceMeeter, Custom and Automatic.

**Not changed:** the game audio thread reopens the game audio device after the handshake (it does that in a retry loop to follow device changes). I left that alone to keep this PR small.

### Compared with #3366

#3366 (closed, v20 branch) replaced the check with a name comparison against the cable names, so no enumeration was needed before the config. It also moved the whole microphone pair setup into the microphone thread, and added a handshake-loop change for wired clients. This PR keeps the exact `is_same_device` check rather than a name heuristic, keeps the microphone setup where it is on master, and leaves the handshake loop alone.

## Testing

Run on Linux x86_64 (WSL Debian), toolchain from `rust-toolchain.toml`:

- `cargo fmt --all -- --check`: PASS
- `cargo clippy -p alvr_server_core -p alvr_audio -- -D warnings`: PASS
- `cargo build -p alvr_server_core`: PASS
- `cargo clippy --target x86_64-pc-windows-gnu -p alvr_server_core -p alvr_audio -- -D warnings`: PASS (mingw, so it compiles the Windows-only code; it only approximates the MSVC CI)

**Not tested:** a real Windows streamer with a headset. I have not measured how long each step takes, so I make no claim about the time saved; the change removes one full output+input enumeration from the path before the stream config. Also untested: the loopback misconfiguration path on Windows.

<!-- TODO(Dayde): if you test on Windows, add: VB-Cable/VAC mic + game audio, connects; game audio pointed at the cable, error logged and the headset disconnects. -->

## Related issues/PRs

- #3365 "handshake error when mic is active" (open). This may help the slow-enumeration variant.
- #3381 "cannot reconnect due to os error" (open). Windows + VB-Cable, possibly the same variant.
- #3366 (closed, not merged). Earlier streamer-side approach on v20; see above.
- `upstream/fix-stream-config-timeout` (Dayde). Client-side socket timeout fix plus a 15 s stream config wait. The two are independent; if this lands, the client's 15 s wait could be shortened or dropped.

## Checklist

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...

🤖 Generated with [Claude Code](https://claude.com/claude-code)
