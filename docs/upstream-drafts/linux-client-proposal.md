# Proposal: a desktop Linux ALVR client (PC VR headset on Monado)

<!-- TODO(Dayde): rewrite in your own words before posting (ALVR AI policy). Post as a GitHub issue or in the Discord dev channel before opening the PRs. -->

<!-- Discord version (this proposal is too long for one Discord message, 2000 characters). Post the issue first, then something like:
I'd like to upstream a desktop Linux ALVR client: it drives a PC VR headset (Bigscreen Beyond + Index controllers) through Monado and receives the stream from a normal ALVR streamer on another PC, reusing client_openxr with XR_MNDX_egl_enable and a zero-copy VAAPI decoder. I've used a v20.14.1-based version daily and ported it to master.
Is upstream interested? Details and open questions: <TODO(Dayde): link to the GitHub issue>
-->

## Motivation

ALVR clients run on standalone headsets. Some PC VR headsets, like a Bigscreen Beyond (DisplayPort + USB, Lighthouse), are only usable from a PC. A Linux client that drives the headset through Monado and receives the ALVR stream would let them play Windows SteamVR content from a separate gaming PC. It would reuse ALVR's whole streaming stack.

In #2306, zmerp wrote: "We plan to add PC client when we implemented the decoder." He also pointed to ALXR, a fork with PC clients. I have been using an in-tree version daily, on a prototype based on v20.14.1, and have ported it to master. Is upstream interested?

## Architecture

```text
Beyond + Index (Lighthouse) <-- DP/USB -- Monado <-- OpenXR -- alvr_client_linux <-- network -- ALVR streamer (Windows)
                                                  XR_MNDX_egl_enable   VAAPI decode -> dma-buf -> EGLImage
```

- **`alvr/client_linux`** (new binary): a thin `main` around `alvr_client_openxr::entry_point()`, with a clean exit on SIGTERM/SIGINT/SIGHUP and surfaceless EGL.
- **`alvr_client_openxr`**: on Linux, the session is created through `XR_MNDX_egl_enable` with the EGL context wgpu already creates. Outside Android, it falls back to OpenXR 1.0 for older loaders and runtimes such as Ubuntu 24.04's. The lobby, stream and interaction code is shared with Android.
- **`alvr_client_core`**: a VAAPI decoder (system FFmpeg through `ffmpeg-sys-next`, behind a `vaapi` feature) that exports DRM PRIME dma-bufs. At startup the client asks the VAAPI driver whether it can decode AV1 and 10 bit, and only advertises what it can, so the streamer falls back to HEVC/8 bit on older GPUs.
- **`alvr_graphics`**: dma-buf import as `GL_TEXTURE_2D`, and a Linux staging shader using the stream's colour space, range, bit depth and chroma siting. Frames are never copied to the CPU.

Android is unaffected: everything new is `cfg(target_os = "linux")` or `Platform::LinuxPc`. One small hardening commit makes `entry_point()` return errors instead of panicking.

**How this differs from ALXR**: it is in-tree, uses the same protocol and stock streamer, and uses the existing GLES renderer through MNDX with zero-copy VAAPI. It targets Linux and Monado only.

## Tested

- **Real hardware, on the v20.14.1-based fork**: Beyond + Index on Monado, Radeon 780M (Mesa VAAPI), Windows streamer, H.264/HEVC. Daily use, reconnects, systemd service. AV1 is untested.
- **Master port**: not yet on the headset. <!-- TODO(Dayde): update once re-tested --> In a GPU-less sandbox, I ran:
  - workspace clippy, clean;
  - unit tests;
  - llvmpipe GL tests, including a real dma-buf import;
  - a Monado 21 smoke test: the lobby renders and exits cleanly (`docs/upstream-drafts/linux-client-smoke-test.md` on Dayde/ALVR `upstream/drafts`).

## Open questions

1. **Does upstream want a desktop client?** It adds a second target for `client_openxr` that needs PC VR hardware to test. I'm happy to maintain it.
2. **FFmpeg**: I link the system FFmpeg dynamically, not `deps/linux/ffmpeg`, which is static, GPL and streamer-only. This adds `libavcodec-dev libavutil-dev` to the Linux CI job, because workspace clippy unifies features, and accepts WTFPL for `ffmpeg-sys-next` only.
3. **Environment variables**: `ALVR_CONTROLLER_PROFILE`, `ALVR_HAND_TRACKING` and `ALVR_VAAPI_DEVICE`. The controller profile and hand trackers are fixed when the session is created, before the client knows the streamer settings. ALVR has no such knobs today. Would you rather have one of these alternatives?
   - Hand trackers always created, and skeleton sending decided from the stream config.
   - The profile persisted from the last streamer settings.
   - Bindings suggested for several profiles.
4. **`XR_MNDX_egl_enable`**: is it acceptable for now, versus Vulkan later (the existing "todo: switch to vulkan")?
5. **Window system**: wgpu has no API to pick the EGL platform, so the client hides `DISPLAY`/`WAYLAND_DISPLAY` to get surfaceless EGL. Is that OK?
6. **Reconnects**: the core keeps the old decoder callback after a disconnect. The VAAPI sink copies MediaCodec's reject-when-stopped behaviour. Should the core clear it instead?
7. **Packaging**: source-only for now, or release artifacts?

## Suggested PRs

1. Hardening: `entry_point() -> Result`, `request_exit()`, and an `Option` for the Android-only EGL function. This is useful on its own.
2. The desktop client: graphics import, decoder, MNDX session, binary. This is the first part that can be tested.
3. Linux input defaults (Index profile, hand tracking off), after question 3.
