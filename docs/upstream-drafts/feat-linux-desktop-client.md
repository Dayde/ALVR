# feat: desktop Linux client (OpenXR on Monado, VAAPI decoding, dma-buf import)

Branch: `Dayde:upstream/feat-linux-desktop-client` → `alvr-org:master` (base `adaa16ba`)

> Post the proposal first (`docs/upstream-drafts/linux-client-proposal.md` on the Dayde/ALVR branch `upstream/drafts`, as an issue or on Discord). Open PRs only if maintainers want a desktop client. The branch is split into 3 PRs (see "PR split"); this draft describes the whole branch, and each PR can reuse the matching parts.

## Summary

<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (rewrite before posting):

This PR adds `alvr_client_linux`, a client that runs on a desktop Linux PC. It drives a PC VR headset through a local OpenXR runtime (Monado) and receives the stream from an ALVR streamer on another PC:

```text
Bigscreen Beyond + Index controllers <-- DP/USB -- Monado <-- OpenXR -- alvr_client_linux <-- network -- ALVR streamer (Windows PC)
```

The client reuses `alvr_client_openxr`; its structure is unchanged. The new parts are:

- a desktop way to create the OpenXR session (`XR_MNDX_egl_enable`),
- a VAAPI decoder that outputs dma-bufs, and
- a dma-buf import path in `alvr_graphics`.

Everything new is behind `cfg(target_os = "linux")` (or `Platform::LinuxPc` at runtime). The only shared-code changes are the error plumbing of commit 1 (see Risks), so Android clients build and behave as before.

Commits. Each one passes `cargo fmt --check` and `cargo clippy --all-targets` on `alvr_graphics`, `alvr_client_core` and `alvr_client_openxr` (plus `alvr_client_linux` once it exists). From commit 3 on, it also passes with `--features alvr_client_core/vaapi`.

1. **`feat(client_openxr): make entry_point fallible`**: shared-crate hardening, useful on its own.
   - `entry_point()` returns `Result<()>`. Missing loader or runtime, a missing `XR_KHR_opengl_es_enable`, no headset, and session or instance creation failures become errors with context. The Android entry points `.unwrap()` them, so they still panic, now with a message.
   - `request_exit()` (an atomic flag, safe in a signal handler) makes the frame loop call `xrRequestExitSession` and return from `entry_point()`.
   - `alvr_graphics`: `eglGetNativeClientBufferANDROID` is now an `Option`. Before, a null pointer was transmuted into a function pointer outside Android.
2. **`feat(graphics): import NV12/P010 dma-bufs on Linux`**: new `alvr_graphics::dmabuf` module (Linux only).
   - Luma and chroma are imported as two `EGL_LINUX_DMA_BUF_EXT` images (R8/GR88 or R16/GR1616) and bound as `GL_TEXTURE_2D`. wgpu creates a *desktop* GL context on Linux, and that context cannot sample multi-planar external images.
   - Each image takes up to 4 dma-buf planes, so the auxiliary planes of compressed modifiers (Intel CCS) stay with their image.
   - A Linux staging shader converts YUV to RGB with the frame's colour space (BT.601/709/2020), range and bit depth. It also offsets the chroma coordinates to the stream's chroma sample location (left-sited by default for H.264/HEVC).
   - The EGL/GL extensions are queried once (`check_dmabuf_import_support()`). An import failure returns an error, which is logged at most every 5 s, instead of `check_error()` → `abort()`. GL errors left by other users of the context are cleared before the import, at most 16 per frame.
3. **`feat(client_core): add a VAAPI decoder for Linux`**: `video_decoder/linux.rs`, using FFmpeg through `ffmpeg-sys-next` 8.1, behind a new non-default `vaapi` feature.
   - The decoder is the first FFmpeg decoder for the codec that has a VAAPI `HW_DEVICE_CTX` hw_config. `avcodec_find_decoder(AV1)` returns `libdav1d` on distro FFmpeg, which can't use VAAPI.
   - Errors are handled like with MediaCodec:
     - a bounded packet queue: a full queue → `false` → the connection requests an IDR;
     - a packet rejected by the decoder → the next push returns `false` → IDR. That next packet is still queued for decoding, so the IDR that ends the recovery is never dropped;
     - a stream VAAPI can't decode (no VAAPI format offered, or hwaccel init failure) or 90 rejections in a row → fatal decoder error → reconnect;
     - a dropped source → `false`, so a reconnection gets a fresh decoder config.
   - Timestamps come from `best_effort_timestamp`; frames without one are skipped.
   - `probe_decoder_capabilities()` asks the VAAPI driver whether AV1 and 10 bit can be decoded (see "Capability probe" below). It links `libva` directly, which is already in the Linux CI packages.
   - Chroma locations reported as unspecified are rendered as left sited (see "Rendering").
   - `WTFPL` is accepted for `ffmpeg-sys-next` only, in `about.toml`.
4. **`feat(graphics): expose eglGetProcAddress on Linux`**: opens `libEGL.so.1` (the library wgpu uses) to get the `eglGetProcAddress` that `XR_MNDX_egl_enable` needs. It is an `Option`, with no unwrap.
5. **`feat(client_openxr): add an MNDX session on Linux`**:
   - Raw `xrCreateSession` with `XrGraphicsBindingEGLMNDX`; the openxr crate only binds the Android GLES binding.
   - Loads `libopenxr_loader.so.1`.
   - Fails early if dma-buf import is unsupported.
   - Advertises AV1 and 10 bit only when the startup probe confirms them. The capabilities of the other platforms are unchanged: their expressions are the same as on master, only moved under `cfg(not(target_os = "linux"))`.
   - `crate-type = ["cdylib", "rlib"]`.
   - **OpenXR 1.0 fallback, only `cfg(not(target_os = "android"))`**: if `xrCreateInstance` fails with `XR_ERROR_API_VERSION_UNSUPPORTED`, it retries with 1.0.34. This is needed for the Ubuntu 24.04 packages (loader 1.0.20, Monado 21). The client uses no OpenXR 1.1 functions. On Android, the instance is still created with a single `create_instance` call and the per-device version, so its behaviour doesn't change.
6. **`feat(client_linux): add a desktop Linux client`**: the new crate.
   - `SIGTERM`/`SIGINT`/`SIGHUP` handlers (installed with `sigaction`, `SA_RESTART`) call `request_exit()`. A second signal exits immediately with `_exit(1)`. A handler that cannot be installed is reported on stderr.
   - `DISPLAY`/`WAYLAND_DISPLAY` are hidden from wgpu, so EGL is surfaceless. This is process-wide (see Risks).
   - On other OSes the crate builds as a stub `main`.
   - Adds a README, `libavcodec-dev libavutil-dev` in the Linux CI job, and a "Desktop Linux Client Building" section in `wiki/Building-From-Source.md`. On Fedora (untested), decoding needs RPM Fusion's `ffmpeg-libs` and `mesa-va-drivers-freeworld`, because `ffmpeg-free` lacks the H.264/HEVC decoders that the VAAPI hwaccel goes through and Fedora's Mesa has H.264/HEVC VAAPI disabled. The wiki says so.
7. **`feat(client_openxr): default to Index controllers`**:
   - Index profile by default (`ALVR_CONTROLLER_PROFILE=vive|quest` overrides).
   - Hand trackers off unless `ALVR_HAND_TRACKING=1`.
   - When hand tracking is on, a hand's skeleton is dropped while its controller is tracked.

   See the open question below.

## Problem

ALVR clients only run on standalone headsets (Android, visionOS). There is no in-tree way to use a PC VR headset connected to a Linux machine as an ALVR client, for example a Bigscreen Beyond on a small Linux PC streaming from a Windows gaming PC. #2306 asked for this use case. zmerp answered: "You can look into ALXR which is a fork of ALVR supporting your usecase." and "We plan to add PC client when we implemented the decoder."

## Root cause

Not a bug. `alvr_client_openxr` assumed Android in three places:

- session creation (`XR_KHR_opengl_es_enable`'s Android binding),
- decoding (MediaCodec + AHardwareBuffer), and
- loading the OpenXR loader.

## Fix

### Session (commits 4, 5)

Monado accepts the EGL context created by wgpu through `XR_MNDX_egl_enable`. The binding needs that EGL library's `eglGetProcAddress`, and khronos-egl doesn't expose it. So `alvr_graphics` opens `libEGL.so.1` (the same library as wgpu) and keeps it loaded while the context exists.

`create_egl_session`'s `# Safety` documents what the caller must guarantee:

- MNDX is enabled on the instance,
- `xrGetOpenGLESGraphicsRequirementsKHR` was called first,
- the `Session<OpenGlEs>` wrapper matches MNDX's GLES swapchain images, and
- the context outlives the session.

### Decoding (commit 3)

FFmpeg/VAAPI runs with `AV_CODEC_FLAG_LOW_DELAY`, one thread, and the parameter sets passed as extradata. Frames are exported with `av_hwframe_map(..., AV_PIX_FMT_DRM_PRIME)`.

FFmpeg's VAAPI export uses one layer per plane. Each layer becomes one EGLImage with all of that layer's planes, auxiliary planes included. A single NV12/P010 layer with exactly 2 planes is split into two images. Other layouts are an error. Plane object indices are checked against `nb_objects`.

`ALVR_VAAPI_DEVICE` selects a render node; by default FFmpeg picks one.

**Codecs: only H.264 and HEVC have been exercised on real hardware, on the v20.14.1-based fork. AV1 and 10 bit are advertised when the driver reports them, but untested.** If the GPU still can't decode the stream, the decoder reports a fatal error and the client reconnects, instead of showing a black screen.

### Capability probe (commits 3, 5)

The streamer only falls back from AV1 to HEVC and from 10 to 8 bit when the client doesn't advertise them (`server_core/src/connection.rs`, around the `encoder_10_bits`/`encoder_av1` checks). Desktop GPUs differ: no VAAPI AV1 before Intel Gen12 or AMD RDNA2, and some drivers have no HEVC Main10. Advertising them unconditionally would make `get_format` fail, the client reconnect, and the same capabilities be sent again, forever.

So, once at startup, `probe_decoder_capabilities()` opens the same VAAPI device as the decoder (FFmpeg's `av_hwdevice_ctx_create`, honouring `ALVR_VAAPI_DEVICE`), takes the `VADisplay` from `AVVAAPIDeviceContext`, and calls `vaQueryConfigProfiles`/`vaQueryConfigEntrypoints`/`vaGetConfigAttributes(VAConfigAttribRTFormat)` through a minimal `extern "C"` binding:

- **AV1** is advertised if `VAProfileAV1Profile0` has a VLD entrypoint and FFmpeg has an AV1 decoder with a VAAPI hw_config.
- **10 bit** is advertised if `VAProfileHEVCMain10` has a VLD entrypoint, and, when AV1 is advertised, AV1 profile 0 supports `VA_RT_FORMAT_YUV420_10`. ALVR has one 10-bit flag, which the streamer applies to whichever codec it selects (the Linux VAAPI encoder only uses it for HEVC and AV1). AV1 8-bit and 10-bit are both profile 0, so the render target format tells them apart.
- If AV1 can only be decoded in 8 bit, AV1 is kept and 10 bit is dropped. This is a judgement call, and such hardware should be rare.
- If the probe fails (no device, driver error), neither is advertised, and a warning says why: `Cannot query the VAAPI decoder, AV1 and 10 bit are not advertised: ...`. On success it logs `VAAPI decoder capabilities: AV1 <bool>, 10 bit <bool>` (visible with `RUST_LOG=info`).

The mapping from driver profiles to capabilities is a pure function with a unit test. H.264 10 bit (High 10, only produced by the software encoder when 10 bit is forced) is not decodable by VAAPI drivers; that is the same on Android and is not addressed here.

### Why system FFmpeg, and why a feature plus a CI change

- `deps/linux/ffmpeg` (built by `prepare-deps` for the streamer) is static, GPL (`--enable-gpl`, x264), patched, and configured with `--disable-everything` and no decoders. Reusing it would couple the client build to the streamer deps and to GPL.
- Distros ship FFmpeg with VAAPI decoders. `ffmpeg-sys-next` 8.1 supports FFmpeg up to 8.1 (Ubuntu 24.04 has 6.1, tested) and uses the workspace's bindgen 0.72 / cc 1.2. The newer `ffmpeg-sys-next` 9 would bump `cc` for every C build script and pull in bindgen 0.73.
- The `vaapi` feature keeps FFmpeg out of the other Linux users of `alvr_client_core` (C API, `client_mock`). `cargo tree -i ffmpeg-sys-next` lists it only under `alvr_client_linux`. It is not in the streamer, the dashboard, the launcher or the Android build.
- A feature alone cannot keep libavcodec out of the CI job: `cargo xtask clippy --ci` builds the whole workspace, and cargo unifies the features that `alvr_client_linux` enables. So commit 6 makes the Linux job install `libavcodec-dev libavutil-dev`.

### Rendering (commit 2)

`StagingRenderer` has a Linux-only `render()` that imports both images and then runs the Linux shader. The Android `render()` is byte-for-byte unchanged, only `#[cfg]`-gated. The `fix_limited_range` hack isn't used by the Linux shader, which gets the real range of each frame.

Chroma siting: FFmpeg reports `AVCHROMA_LOC_LEFT` for H.264/HEVC streams that don't signal a location, which is the spec default; this was checked with ffprobe on x264/x265 streams. The shader then moves the chroma coordinates by half a luma texel, which is a quarter of a chroma texel. **Choice for "unspecified"** (usual in AV1 streams): it is rendered as left sited too, like mpv/libplacebo do for unknown 4:2:0 locations, and consistent with the H.264/HEVC default. Only an explicit `AVCHROMA_LOC_CENTER` is sampled as centered. The difference is a quarter chroma texel either way. An ignored GL test reads back alternating chroma columns at even and odd luma columns, which fails if the offset is missing or has the wrong sign (checked by mutating it).

### Reconnect / decoder config (in commit 3)

`ClientCoreContext` never clears its decoder callback, so after a reconnect the previous stream's sink keeps receiving NALs. The client only gets a new `DecoderConfig` after `RequestIdr`, which is sent when the callback returns false.

The MediaCodec sink returns false once its source is dropped, and the VAAPI sink copies that behaviour. A generic fix (clearing the callback on `StreamingStopped`) is left as an open question.

### Window system (commit 6)

wgpu's EGL backend picks Wayland if `wl_display_connect(NULL)` succeeds, then X11, and only then `EGL_MESA_platform_surfaceless`. Mesa's Wayland platform has no pbuffer configs, which `GraphicsContext` needs, and a service may run outside any desktop session. wgpu 25 has no API to choose the platform.

So `main` sets `WAYLAND_DISPLAY` to a socket that doesn't exist and removes `DISPLAY`. This is done before any thread exists, which is what makes the edition-2024 `unsafe` `set_var` sound.

### Open question: environment variables (commit 7)

ALVR has no user-facing `ALVR_*` environment knobs today. This PR adds `ALVR_CONTROLLER_PROFILE`, `ALVR_HAND_TRACKING` (commit 7) and `ALVR_VAAPI_DEVICE` (commit 3).

**Why env vars.** The interaction profile is bound and the hand trackers are created with the session, before the client connects and learns the streamer settings. `xrAttachSessionActionSets` can only be called once per session. And Monado exposes Index controllers as "conforming" hand tracking, while the streamer switches to hand tracking whenever a skeleton is sent.

**Alternatives** (I couldn't validate a redesign without the hardware at hand, so I kept the env vars for now):

- Create the hand trackers unconditionally, and decide whether to send skeletons from the stream config. That could use `controllers.hand_skeleton` / `multimodal_tracking` in `session/src/settings.rs`, and `InteractionSourcesConfig` / `select_sources` in `client_openxr/src/interaction.rs`, which already select sources per stream.
- Persist the controller profile in the client-side config from the last streamer settings, so it applies at the next session.
- Suggest bindings for several profiles (Index, Vive, Touch) and let the runtime pick, as many OpenXR apps do.

`ALVR_VAAPI_DEVICE` could become a client config entry too. Which would you prefer?

## Testing

Real hardware, by me (Dayde), on the **v20.14.1-based fork** this was ported from (`linux-client-v20.14.1`):

- Bigscreen Beyond + Valve Index controllers on Monado (Lighthouse), AMD Radeon 780M, Mesa radeonsi VAAPI.
- Windows streamer.
- Streaming with H.264/HEVC, controllers, reconnects, clean stop as a systemd service.
- AV1 was not used.

<!-- TODO(Dayde): add codecs/bit depth/resolution/refresh rate you used, and re-test this master port (which also changed the error handling, chroma siting, dma-buf layout code and added the VAAPI capability probe since the fork; check the `VAAPI decoder capabilities: ...` line with `RUST_LOG=info`) on the headset before posting -->

**This master port has not been run on a headset or a GPU yet.** It was built and tested in a sandbox without a headset or GPU (Ubuntu 24.04, FFmpeg 6.1 dev packages, Mesa 25.2 llvmpipe), with `CARGO_TARGET_DIR` shared and `CARGO_INCREMENTAL=0`:

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check`; `cargo xtask format` + `git diff --exit-code` | pass |
| `cargo xtask clippy --ci` (whole workspace, Linux) | pass, 0 diagnostics |
| per commit (all 7): `cargo fmt --check` + `cargo clippy --all-targets` on graphics/client_core/client_openxr(/client_linux); also with `--features alvr_client_core/vaapi` from commit 3 on | pass, 0 diagnostics |
| `cargo test -p alvr_graphics -p alvr_client_core -p alvr_client_openxr -p alvr_client_linux -p alvr_session --features alvr_client_core/vaapi` | pass (graphics 7 + 3 ignored, client_core 6 including the capability mapping, session 4). The matrix test checks all 5 published coefficients of BT.601, BT.709 and BT.2020. |
| `EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 cargo test -p alvr_graphics -- --ignored` | pass, 3 tests: (1) Linux shader on limited BT.709 red planes → RGB (255,0,0); (2) a linear NV12 frame in a `/dev/shm` file imported through the real `EGL_LINUX_DMA_BUF_EXT` path → red, and fd −1 → `Err` without calling the render callback (no abort); (3) the same path with chroma columns alternating Cr 96/160, read back per luma column: even columns get the chroma sample, odd ones the average. Mutating the dma-buf data makes (2) fail; a centered or negative chroma offset makes (3) fail. |
| `cargo xtask check-licenses` | pass; fails if the `[ffmpeg-sys-next]` section is removed |
| `cargo build -p alvr_client_linux` | pass; links `libavcodec.so.60`/`libavutil.so.58`/`libva.so.2` |
| `XR_RUNTIME_JSON=/nonexistent alvr_client_linux` | loader messages, then `Cannot query the OpenXR runtime, is it running?: the loader was unable to find or load a runtime`, exit 1, no panic |
| decoder selection on Ubuntu's FFmpeg 6.1 (ad hoc) | default decoders `h264`/`hevc`/`libdav1d`; VAAPI-capable picks `h264`/`hevc`/`av1` |
| `cargo clippy --workspace --target x86_64-pc-windows-gnu --exclude alvr_server_openvr` | pass, 0 diagnostics. `alvr_server_openvr` fails identically on master (mingw rejects MSVC flags). CI uses MSVC; mingw is only an approximation. |
| `cargo clippy -p alvr_client_openxr --target aarch64-linux-android` | the same 16 warnings as master, all in untouched code; none new |
| capability probe in the sandbox (no `/dev/dri`) | probe fails, conservative capabilities: `Cannot query the VAAPI decoder, AV1 and 10 bit are not advertised: Cannot open VAAPI device (default): Generic error in an external library`, logged before `xrCreateSession`; the client continues to the lobby |
| Monado smoke test (no GPU, branch binary) | see below |

Re-verified on 2026-10-28 at `893304d2` (same trees as the final head `bddb14ca`, which only rewords the commit messages; still based on the current `upstream/master` `adaa16ba`), on Debian 12 in WSL2 (Rust 1.97.1, FFmpeg 5.1 dev packages: libavcodec 59, libavutil 57):

| Check | Result |
| --- | --- |
| per commit (all 7): `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` on graphics/client_core/client_openxr(/client_linux); also with `--features alvr_client_core/vaapi` from commit 3 on | pass |
| `cargo clippy --workspace --all-targets --exclude alvr_server_openvr -- -D warnings` (`server_openvr` needs `prepare-deps`) | pass |
| `cargo test` on graphics/client_core/client_openxr/client_linux/session with `vaapi` | pass (client_core 6, graphics 7 + 3 ignored, session 4) |
| `cargo build -p alvr_client_linux` | pass, links `libavcodec.so.59`/`libavutil.so.57`/`libva.so.2`: FFmpeg 5.1 builds too |
| `XR_RUNTIME_JSON=/nonexistent alvr_client_linux` without a loader installed | `Cannot load the OpenXR loader libopenxr_loader.so.1: ...`, exit 1, no panic |
| `cargo xtask build-client` (CI's `check-android` setup: NDK r26b, platform 32, cargo-apk) on this branch and on master | both build `alvr_client_android.apk` |
| `cargo clippy -p alvr_client_openxr --target aarch64-linux-android` on this branch and on master | 16 warnings on both, none new |

**Monado smoke test**, full report in `docs/upstream-drafts/linux-client-smoke-test.md` (Dayde/ALVR branch `upstream/drafts`). Setup: Ubuntu 24.04 `monado-service` 21.0.0 + loader 1.0.20, lavapipe, simulated HMD, no controllers, no streamer. With the branch binary and no shim:

- The 1.0 fallback triggers (`The OpenXR runtime does not support OpenXR 1.1.36, using 1.0.34`).
- EGL goes surfaceless, the MNDX session starts, and the lobby renders: 1785 frames in 30 s on XCB/Xvfb, 598 with the null compositor.
- SIGTERM/SIGINT/SIGHUP → `xrEndSession` and exit 0 in 1.2–2.5 s. Two quick SIGINTs → exit 1 in 0.13 s.
- Without `monado-service`: a clean error and exit 1.
- Re-run with the capability probe: the probe logs the conservative decision above, and the lobby still renders (1771 frames in 30 s on XCB, SIGTERM → exit 0 in 2.5 s; 298 frames in 15 s null, SIGINT → exit 0 in 1.3 s).

Not tested:

- The master port on real hardware: VAAPI decode, real dma-bufs from a GPU, Intel CCS modifiers, P010/10-bit, AV1, chroma siting on real content, and the capability probe on a real driver (only its failure path ran here).
- Streaming end to end: there was no streamer in the sandbox.
- The Android APK on a headset. It builds (see the re-verification above), and Android code paths are unchanged apart from commit 1's `Result`/`Option` plumbing.
- MSVC Windows and macOS (CI will run them).
- NVIDIA (nvidia-vaapi-driver), Intel.

## Related issues/PRs

- **ALXR** ([korejan/ALVR](https://github.com/korejan/ALVR), [ALXR client wiki](https://github.com/korejan/ALVR/wiki/ALXR-Client)): direct prior art. It is "a cross-platform/device OpenXR based client for ALVR", with Windows/Linux PC builds (D3D11/D3D12/Vulkan; NVDEC/CUVID/D3D11VA/VAAPI/CPU decoders) and Android. As far as I can tell, it lives in a fork and is used with the ALVR server builds published with ALXR-nightly; I couldn't verify how far those diverge from upstream's protocol. This PR differs:
  - it is in-tree, using the same `client_openxr`/`client_core` code and protocol as the official clients, so it works with the stock streamer;
  - it targets Monado through `XR_MNDX_egl_enable` with the existing GLES renderer, rather than a separate Vulkan/D3D renderer;
  - it uses zero-copy VAAPI → dma-buf → EGLImage;
  - it is Linux only.
- #2306 "Does ALVR also support wired VR headsets with an HDMI or DisplayPort cable" (closed 2024). zmerp: "You can look into ALXR which is a fork of ALVR supporting your usecase." and "We plan to add PC client when we implemented the decoder." This PR is that PC client, with a VAAPI decoder.
- `upstream/monado` branch (Leonhard Saam, authored 2024, rebased Nov 2025): streamer side. It makes `server_core` work as a Monado driver instead of SteamVR. On the client side it only renames the Android package (plus protocol changes carried from the tracking rewrite commits). No overlap with this PR.
- #3347 "Monado driver for ARM64 Linux — enables streaming to Vision Pro without SteamVR" (closed as stale): also streamer side. Not a conflict.
- #2993 "feat: ✨ Add server_mock" (zmerp, draft): a mock streamer, "laying the ground for Monado support", on the streamer side. Only `Cargo.lock` might conflict. Once merged, it could drive an end-to-end test of this client without a Windows PC.
- #1453 "openxr client does not launch at Monado based runtime" (closed 2023): Android Monado, not desktop.
- #3420, #3412: streamer-side VAAPI issues, unrelated.
- WiVRn is the reverse setup: the Linux PC runs the runtime and streams to a standalone headset.

Works best with my other branches. Each is a separate PR, and this feature doesn't depend on them to build or run:

- `upstream/fix-index-automatic-mapping` (server_core): maps every Index input in the automatic button mapping.
- `upstream/fix-display-time-ahead` (client_openxr): Monado holds a frame until its display time, and a stale timestamp after a reconnect can freeze the stream.
- `upstream/fix-pose-history-reconnect` (server_openvr): the streamer-side cause of those stale timestamps.
- `upstream/fix-stream-config-timeout` (sockets, client_core).
- `upstream/fix-staging-shader-precision`: only touches the Android staging shader.

## PR split

Upstream squash-merges PRs, so the branch is meant to be posted as 3 PRs. Each one leaves master working:

1. **PR 1: hardening** (commit 1). `entry_point() -> Result` with context, `request_exit()`, the `get_native_client_buffer` `Option`. It is useful on its own: clearer errors on any runtime, and no undefined behaviour outside Android.
2. **PR 2: desktop client** (commits 2–6: graphics import, decoder, `eglGetProcAddress`, MNDX session, binary). This is the first unit that can be tested. A graphics-only PR would be untestable dead code.
3. **PR 3: Linux input defaults** (commit 7), after the environment-variable discussion above.

## Risks

- **Android/Quest/Pico**: shared code changes (commit 1 only):
  - `entry_point()` returns `Result`, unwrapped at the Android call sites, so a panic is still a panic, now with context;
  - `assert!` → `bail!` for `XR_KHR_opengl_es_enable`;
  - `create_session` returns `Result`;
  - `get_native_client_buffer` is an `Option`, always `Some` on Android;
  - a relaxed atomic load per frame for `request_exit()`, which is never set on Android.

  The OpenXR 1.0 fallback is `cfg(not(target_os = "android"))`. Commit 7's changes are `Platform::LinuxPc` only. The capabilities of non-Linux targets use master's expressions unchanged; the probe is `cfg(target_os = "linux")`.
- **Workspace builds and the C API**: a workspace-wide build (clippy, `cargo build` without `-p`) enables `vaapi` in `alvr_client_core` through feature unification. So the `alvr_client_core` cdylib/staticlib and `alvr_client_mock` built that way contain the VAAPI decoder. On Linux, the C API's `alvr_create_decoder`/`alvr_get_frame` would then hand out `DmabufFrame*` instead of nothing. This is harmless today, since there is no Linux consumer of the C API, but worth knowing.
- **CI and developer setup**: the Linux job gains two apt packages (`libva-dev`, which the probe now links, was already installed). Developers running workspace-wide commands on Linux need `libavcodec-dev libavutil-dev` (documented in `wiki/Building-From-Source.md`). `cargo xtask build-streamer` doesn't need them.
- **Licence**: `WTFPL` is accepted for `ffmpeg-sys-next` only. The client links the system FFmpeg dynamically.
- **ffmpeg-sys-next 8.1** won't build against FFmpeg 9 until it is bumped.
- **Decoder error policy**: 90 consecutive packets rejected by the decoder restart the connection. With the default `avoid_video_glitching: false`, every packet reaches the decoder (the one after a rejection is queued and only reports `false`), so this is 90 frames: about 1 s at 90 fps, 0.75 s at 120, 1.25 s at 72. With `avoid_video_glitching` on, the core stops forwarding non-IDR packets after a `false`, so rejections accumulate only about once per IDR round trip and the fatal threshold takes several seconds. That number is a guess. The bounded queue of 4 packets may request IDRs if the decoder thread is starved; packets dropped because the queue is full are not counted.
- **Capability probe**: it trusts the driver's profile list. A driver that lists a profile it then fails to decode still ends in the fatal-error/reconnect path. The 8-bit-only AV1 case drops 10 bit rather than AV1.
- **Environment mutation** in `main` (window system variables): it is only sound because it runs before any thread. It is process-wide, so an OpenXR runtime loaded in-process that needs X11 or Wayland would not find them either. Monado as packaged runs its compositor in `monado-service`, which is unaffected (the smoke test used `DISPLAY=:99` on the service side). Documented in the README.
- **Maintenance**: a second (desktop) target for `client_openxr` that maintainers can't test without PC VR hardware.

## Checklist

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...
