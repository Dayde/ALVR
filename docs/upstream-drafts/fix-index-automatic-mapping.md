# fix(server_core): map every Index controller input in automatic button mapping

Branch: `Dayde:upstream/fix-index-automatic-mapping` → `alvr-org:master` (base `adaa16ba`)

## Summary

<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (rewrite it): automatic button mapping (`alvr/server_core/src/input_mapping.rs`) was written for Quest-style source controllers. When a client reports the Valve Index controller profile (`/interaction_profiles/valve/index_controller`), several inputs are dropped or bound wrongly: the left A/B buttons, both trackpads, grip force, the system touch, and, with Quest emulation, the left system button meant to act as menu. This PR fixes each of these in its own commit. The bindings for every client profile that upstream clients use today (Quest, Pico 4/4S/Neo 3, Focus 3, YVR) stay exactly the same. The new tests lock them, and a full dump comparison against master confirms it.

## Problem

With an Index controller source (for example a desktop OpenXR client running on Monado with Lighthouse controllers):

- **Left A/B:** never bound. The code only knew left X/Y.
- **Trackpads:** x/y/touch/force never bound, even with Index emulation. Trackpads were only used as a *fallback destination* (thumbstick, thumbrest, X/A on Vive wands).
- **Grip force:** `map_button_pair_automatic` ignored the source `force` input, so `squeeze/force` was dropped.
- **System touch:** never bound. Instead, the system click drove the destination touch, also on emulated controllers that do not register system touch.
- **Left system button with Quest emulation (left hand only):** the emulated Quest left controller has a menu button but no system button. That is the only case where the "system button, destination has only a menu" branch (added with PSVR2 emulation, #2871) runs. It binds the system click to its own path (`map_button_pair_automatic(click, click, ..)`), which the emulated controller does not register. Only `/input/system/click`, which the driver shares between the menu and system buttons (`Paths.cpp`), reached SteamVR; `application_menu`/`create` did not. Other emulation modes, Pico 4 included, register a left system button, so this branch does not run for them.

The same trackpad gap affects Vive wand and Pico G3 sources with Vive or Index emulation: their trackpads were dropped.

## Root cause

`automatic_bindings` has a hand-written section per physical button that assumes the source has Quest-like inputs. There is no branch for left A/B, no trackpad-to-trackpad passthrough, and `map_button_pair_automatic` has no `force` handling. In the system → menu fallback added in #2871, the system click is bound to its own path, which the emulated controller does not register, so only the driver's shared SteamVR path delivers part of it (it is unclear whether `click, click` was intended). Both the system and the menu → system branches also pick the destination system touch from the *source* set (`ct(s_set, ..)`) without checking that the emulated controller has it.

## Fix

Five commits, each one small:

1. `test(server_core)`: lock bindings for Quest controllers. A test helper renders the bindings as `source -> destination (mapping)` strings. The Quest Touch source with Index, Quest and Vive emulation is asserted against the exact set that **unmodified master** produces. These tests were written and recorded on master first.
2. `fix(server_core)`: forward squeeze force. Source `force` is passed through to destination `force`. The existing fallback that derives force from the end of the value range (for sources without force) is unchanged, and a test covers it.
3. `fix(server_core)`: map Index system buttons to menu and forward their touch.
   - The source system touch is bound, but only when both controllers have it. Sources without system touch keep a click-only binding, as before.
   - The system → menu fallback (Quest emulation, left hand) now targets the menu button, but only when the source has no menu button of its own (Index).
   - The menu → system fallback still drives the system touch as on master, but only when the emulated controller registers it.
   - Source controllers that have both buttons (Pico 4, Pico 4S, Pico Neo 3, Vive, PSVR2) keep the master behaviour for the system button with Quest emulation, so their bindings do not change.
4. `fix(server_core)`: left A/B. They map to A/B if the destination has them, else to X/Y. A also falls back to the trackpad click/touch, mirroring X and right A. B has no trackpad fallback, like right B on master.
5. `fix(server_core)`: trackpad passthrough. Trackpad x/y/click/touch/force pass through when both controllers have them, unless another input already drives that destination. This keeps the master behaviour where the Index thumbsticks drive the Vive wand trackpads: no two sources fight over one destination.

### Before/after: Index controller source

"—" means not bound. Right-hand inputs behave the same as the left ones unless listed.

| Index input | Index emulation (before → after) | Quest emulation (before → after) | Vive wand emulation (before → after) |
|---|---|---|---|
| left system/click | system/click + system/touch → system/click | left system/click (not registered, only reaches SteamVR's shared system path) → **menu/click** | system/click + system/touch (not registered) → system/click |
| right system/click | system/click + system/touch → system/click | system/click + system/touch (not registered) → system/click | system/click + system/touch (not registered) → system/click |
| system/touch (L+R) | — → **system/touch** | — → — | — → — |
| left a/click, a/touch | — → **a/click, a/touch** | — → **x/click, x/touch** | — → **trackpad/click, trackpad/touch** |
| left b/click, b/touch | — → **b/click, b/touch** | — → **y/click, y/touch** | — → — |
| right a/b | a/b (unchanged) | a/b (unchanged) | a → trackpad click/touch, b — (unchanged) |
| squeeze/value | value (unchanged) | value (unchanged) | squeeze/click via threshold (unchanged) |
| squeeze/force | — → **squeeze/force** | — → — (bound with no targets, so no "not mapped" log) | — → — (same) |
| trackpad/x, y, touch, force | — → **passthrough** | — → — | — → — (Vive trackpads stay on the thumbsticks) |
| thumbstick, trigger | unchanged | unchanged | unchanged |

### Other source profiles (full dump comparison against master, 10 profiles × 5 emulation modes)

- **Unchanged:** Quest Touch, Pico 4, Pico 4S, Pico Neo 3, Vive Focus 3, YVR, for all of Index / Quest / Vive / Pico 4 / PSVR2 emulation.
- **Vive wand source:** trackpads now pass through with Vive emulation (x/y/click/touch) and Index emulation (x/y/touch; Index has no trackpad click). Everything else is the same.
- **Pico G3 source:** the same trackpad additions with Vive and Index emulation. Quest emulation is unchanged.
- **PSVR2 Sense source** (no upstream client reports it): system touch is now bound to system touch where the destination has it. The system click no longer also drives an unregistered system touch, and with Quest or Pico 4 emulation the right menu click no longer drives an unregistered right system touch.

## Testing

All commands were run in the worktree on top of `upstream/master` adaa16ba, with `CARGO_TARGET_DIR` shared:

- `cargo test -p alvr_server_core`: pass (13 tests in `input_mapping`, all green)
- Every commit on its own passes `cargo fmt --all -- --check`, `cargo test -p alvr_server_core` and `cargo clippy -p alvr_server_core --tests` (checked commit by commit)
- `cargo test -p alvr_session`: pass (4 tests)
- `cargo fmt --all -- --check`: pass. `cargo xtask format` + `git diff --exit-code`: pass (no diff)
- `cargo xtask clippy --ci`: pass, zero Rust diagnostics (only pre-existing C++ build-script warnings from `server_openvr`). I also ran `cargo clippy -p alvr_server_core --tests`, which lints the new tests: no warnings
- `cargo build -p alvr_server_openvr`: pass
- `cargo check -p alvr_server_core --target x86_64-pc-windows-gnu`: pass (mingw only approximates CI's MSVC)
- I recorded the regression lock first: I wrote the Quest tests on unmodified master, recorded what master produces, then committed them before any change. I also compared a one-off dump of all profiles × emulation modes between master and this branch (results above). The dump helper is not committed.

**Not tested:** no runtime test with SteamVR or a real headset. In particular I did not check in SteamVR's input UI that the Index left system button opens the application menu with Quest emulation. Android was not built (the NDK is not available here).

New unit tests in `input_mapping.rs`:
- `quest_to_{index,quest,vive}_bindings_are_unchanged`: exact sets recorded on master.
- `vive_to_{index,quest,vive}_bindings`: exact sets (master plus the trackpad passthrough).
- `index_to_index_binds_every_input`: every Index input is bound as a source and set as a destination.
- `index_to_quest_binds_menu_and_left_buttons`: system → menu, left A/B → X/Y, right A/B → A/B (left/right asymmetry).
- `index_to_vive_keeps_thumbsticks_on_trackpads`: no trackpad binding duplicates the thumbstick fallback.
- `index_bindings_target_registered_inputs_only`: across Quest, Pico 4, PSVR2, Index and Vive emulation.
- `squeeze_force_is_forwarded_or_derived_from_value`
- `index_system_buttons_forward_touch_and_act_as_menu`
- `system_touch_targets_registered_inputs_only`: for every source profile across Quest, Pico 4, PSVR2, Index and Vive emulation, every binding to a system touch targets an input the emulated controller registers. It fails without commit 3 (checked by running it against the code before commit 3).

The tests are about 450 lines and overlap somewhat (the exact-set Vive tests and the targeted Index tests). I can trim them on request; the Quest regression lock would stay.

## Related issues/PRs

- #1817: added the automatic button mapping system (merged). This PR extends it.
- #2871: PSVR2 Sense emulation (merged). It added the system/menu button mapping that commit 3 corrects.
- #3253: PSVR2 proximity sensors (merged). Same function; no conflict, the sensor bindings are untouched.
- #2679: Pico G3 controller profile (merged). G3 trackpads now also pass through with Vive/Index emulation.
- #1076: thumbrest → trackpad touch (merged). The behaviour is kept: it still takes the destination trackpad, and the new passthrough yields to it.
- #847: "Valve Index emulation still needs work" (closed, old). Loosely related.
- I searched for "Index controller mapping", "trackpad", "automatic button mapping", "Index emulation" buttons, "grip force", "left A B button" and `input_mapping`. No open PR touches `input_mapping.rs`, so no conflict (the open #3360 "headset emulator" only touches the connection/sockets parts of `server_core`). No open issue reports these exact symptoms. Upstream Android clients never report the Index or Vive profile, so today this mostly matters for desktop clients that do.

## Risks

- **Vive wand / Pico G3 users with Vive or Index emulation:** their trackpads now work where they were silently dropped. A user who relied on them doing nothing would notice.
- **System button on sources with menu + system (Pico 4, Pico 4S, Pico Neo 3, Vive) with Quest emulation:** kept as on master (system → system path shared with menu in the driver). This is intentional, to avoid changing existing users, but it is odd code; a comment explains it.
- **Duplicates kept from master:** with Vive emulation, X/A and the thumbstick click both drive the trackpad click (Quest source, and now Index left A too, mirroring right A). With Vive → Index/Quest, both the menu and system buttons drive the system path. I left these as they were.
- **Vive trackpad click with Index emulation** stays unbound (Index has no trackpad click). Mapping it to trackpad force could be a follow-up.
- **Pre-existing, out of scope:** with a PSVR2 source and Index or PSVR2 emulation, both the menu click and the system touch drive system/touch, and the PSVR2 menu touch is never bound as a source. No upstream client reports the PSVR2 profile.
- **Trackpad passthrough skips destinations already bound:** this is decided per component, so the order of sections in `automatic_bindings` matters and trackpads must stay last. The code comment says so.

## Checklist

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...
