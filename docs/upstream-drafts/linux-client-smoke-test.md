# alvr_client_linux smoke test against Monado (no GPU)

## Round 3: VAAPI capability probe

Branch: `origin/upstream/feat-linux-desktop-client` at `d2955f0f`, with the capability probe (base `upstream/master` `adaa16ba`, 7 commits). Same packages, harness and environment as round 1; the packages were reinstalled for this run and purged afterwards, and the package list matches the state before the run. The final head `893304d2` differs from it only by an atomic `swap` on the decoder's IDR flag.

There is no `/dev/dri` in this sandbox, so the probe is expected to fail and the client to advertise neither AV1 nor 10 bit:

| Scenario | Result |
|---|---|
| Probe (both runs) | `Cannot query the VAAPI decoder, AV1 and 10 bit are not advertised: Cannot open VAAPI device (default): Generic error in an external library`, logged once after EGL setup and before `xrCreateSession` |
| SIGTERM, XCB on Xvfb, after 30 s | OpenXR 1.0 fallback, MNDX session, lobby: 1771 `xrEndFrame`, `xrEndSession`, exit 0 in 2.46 s |
| SIGINT, null compositor, after 15 s | 298 frames, exit 0 in 1.33 s |

No panic. The probe doesn't delay or block the lobby when it fails. The success path (a real VAAPI driver) could not be exercised here.

---

## Round 2: branch binary with the OpenXR 1.0 fallback

Branch: `origin/upstream/feat-linux-desktop-client` at `b7daecdf` (base `upstream/master` `adaa16ba`, 7 commits).
Binary: `cargo build -p alvr_client_linux` (debug), run as built. There is **no loader shim** this time.
Packages, harness and environment are the same as in round 1 (below). The packages were reinstalled for this run and purged afterwards; the package list matches the state before the run.

| Scenario | Result |
|---|---|
| Instance creation (loader 1.0.20, Monado 21) | `xrCreateInstance called with invalid API version 1.1` from the loader, then the client logs `The OpenXR runtime does not support OpenXR 1.1.36, using 1.0.34` and continues. This is the fix for round-1 Result 1. |
| EGL + MNDX session + lobby | surfaceless EGL (llvmpipe). The session starts and the lobby renders. |
| SIGTERM, XCB on Xvfb, after 30 s | 1785 `xrEndFrame` entries in the log, `Exit requested, ending the OpenXR session`, `xrEndSession`, exit 0 in 2.46 s |
| SIGTERM, null compositor, after 30 s | 598 frames, exit 0 in 2.47 s |
| SIGINT, null, after 15 s | 299 frames, exit 0 in 1.23 s |
| SIGHUP, null, after 15 s | 300 frames, exit 0 in 1.33 s |
| Two SIGINTs 20 ms apart | exit 1 in 0.13 s (`_exit(1)`), no `xrEndSession`, as designed |
| No `monado-service` | fallback, then `Cannot create the OpenXR instance: the runtime failed …`, exit 1, no panic |

No panic in any run. The frame counts are `grep -c xrEndFrame` on the client log (`OXR_DEBUG_ENTRYPOINTS=1`), the same metric as in round 1. Logs are available on request.

Still not exercised, because there is no streamer and no GPU here: decoding, the VAAPI → dma-buf import on real hardware, controllers and hand tracking.

---

## Round 1 (original report)

Round 1 tested `62b4d5dd`, which still had the stall-watchdog commit; that commit has since been dropped from the upstream branch.


Branch tested: `origin/upstream/feat-linux-desktop-client` at `62b4d5dd` (base `upstream/master` `adaa16ba`),
checked out read-only. `cargo build -p alvr_client_linux` (debug, shared target directory,
`CARGO_INCREMENTAL=0`): OK, no warnings from ALVR crates.

## Environment

| Component | Version |
|---|---|
| OS | Ubuntu 24.04.5 LTS, 4 cores, no `/dev/dri` |
| Rust | 1.97.1 |
| Monado (`monado-service`, `libopenxr1-monado`, `monado-cli`) | 21.0.0+git2905.e26a272c1~dfsg1-2build2 |
| OpenXR loader (`libopenxr-loader1`) | 1.0.20~dfsg1-1build2 |
| Mesa (EGL llvmpipe, lavapipe Vulkan) | 25.2.8-0ubuntu0.24.04.4 (llvmpipe, LLVM 20.1.2) |
| Xvfb | 21.1.12-1ubuntu1.9 |

```sh
sudo apt-get install -y --no-install-recommends monado-service libopenxr1-monado monado-cli mesa-vulkan-drivers vulkan-tools
```

`grep -a MNDX_egl_enable /usr/lib/x86_64-linux-gnu/libopenxr_monado.so` matches; the runtime advertises
`XR_MNDX_egl_enable`, `XR_KHR_opengl_es_enable`, `XR_MND_headless`, `XR_FB_display_refresh_rate`, `XR_EXT_hand_tracking`, among others.
Monado picks the "legacy" builder and creates a "Simulated HMD" automatically. There are no controllers.

Two compositor targets work headlessly with lavapipe:

- `XRT_COMPOSITOR_NULL=1`: the null compositor.
- `DISPLAY=:99 XRT_COMPOSITOR_FORCE_XCB=1` with `Xvfb :99 -screen 0 1920x1080x24`: "Deferred target backend X11(XCB) Windowed selected", 3-image VkSwapchain, IMMEDIATE present mode.

The default target (no env vars) tries Wayland direct mode and fails.

Sandbox gotchas, which are not client problems:

- `monado-service` needs `XDG_RUNTIME_DIR` set (it creates `monado_comp_ipc` there).
- `monado-service` needs a pollable stdin. With `</dev/null` it fails with `ERROR [init_epoll] epoll_ctl(stdin) failed`, so it was run as `sleep N | monado-service`.
- The DBus/BLE errors at startup are harmless.

Run harness (a small shell script, available on request). For each scenario:

1. Start `monado-service` with `XDG_RUNTIME_DIR=<fresh dir> XRT_LOG=debug [XRT_COMPOSITOR_NULL=1 | DISPLAY=:99 XRT_COMPOSITOR_FORCE_XCB=1]`.
2. Start the client with `XDG_RUNTIME_DIR=<same> DISPLAY=:99 XR_RUNTIME_JSON=/usr/share/openxr/1/openxr_monado.json RUST_LOG=info OXR_DEBUG_ENTRYPOINTS=1`.
3. Wait N seconds, send the signal, then wait up to 15 s for the client to exit.

## Result 1: instance creation fails on stock Ubuntu 24.04 (client bug, fixed in round 2)

```
OpenXR available extensions: [ ... "XR_MNDX_egl_enable", ... ]
Error [GENERAL | xrCreateInstance | OpenXR-Loader] : xrCreateInstance called with invalid API version 1.1.  Max supported version is 1.0
Cannot create the OpenXR instance: the runtime does not support the requested API version
```

This happens with and without `monado-service` running. Exit status is 1, and there is no panic.

Root cause: on desktop Linux the platform falls into `_ => ("", CURRENT_OPENXR_VERSION)`, so `alvr/client_openxr/src/lib.rs` always requests OpenXR 1.1.36. OpenXR 1.0-only loaders and runtimes reject that. Ubuntu 24.04 ships loader 1.0.20 and Monado 21, and Debian 12 is similar. The client only works with a 1.1 loader and a 1.1 runtime, which means Monado 24.0 or newer.

The client does not use any OpenXR 1.1 core functions. The only raw `fp()` calls are `create_session`, `get_system_properties` and `get_instance_proc_addr`.

Proposed fix: retry with 1.0 on `XR_ERROR_API_VERSION_UNSUPPORTED`, which is the pattern the spec recommends. I verified it in a throwaway `git archive` copy, not on the branch:

- `rustfmt --check` passes.
- `cargo clippy -p alvr_client_openxr -p alvr_client_linux` reports 0 warnings.
- Without any shim it logs `The OpenXR runtime does not support OpenXR 1.1.36, using 1.0.34`, then runs the lobby: 1125 frames in 20 s on XCB, SIGTERM gives exit 0 with `xrEndSession`.

```diff
--- a/alvr/client_openxr/src/lib.rs
+++ b/alvr/client_openxr/src/lib.rs
@@ -15,6 +15,7 @@
     glam::{Quat, UVec2, Vec3},
     info,
     parking_lot::RwLock,
+    warn,
 };
 use alvr_graphics::GraphicsContext;
 use alvr_session::{BodyTrackingBDConfig, BodyTrackingSourcesConfig, PerformanceLevel};
@@ -304,19 +305,34 @@
         })
         .collect::<Vec<_>>();
 
-    let xr_instance = xr_entry
-        .create_instance(
+    let create_instance = |api_version| {
+        xr_entry.create_instance(
             &xr::ApplicationInfo {
                 application_name: "ALVR Client",
                 application_version: 0,
                 engine_name: "ALVR",
                 engine_version: 0,
-                api_version: openxr_version,
+                api_version,
             },
             &selected_exts,
             &[],
         )
-        .context("Cannot create the OpenXR instance")?;
+    };
+    let xr_instance = match create_instance(openxr_version) {
+        // OpenXR 1.0 runtimes and loaders (for example Monado 21 and loader 1.0.20 on Ubuntu
+        // 24.04) reject an instance for OpenXR 1.1. The client doesn't depend on 1.1 features.
+        Err(xr::sys::Result::ERROR_API_VERSION_UNSUPPORTED)
+            if openxr_version != LEGACY_OPENXR_VERSION =>
+        {
+            warn!(
+                "The OpenXR runtime does not support OpenXR {openxr_version}, \
+                using {LEGACY_OPENXR_VERSION}"
+            );
+            create_instance(LEGACY_OPENXR_VERSION)
+        }
+        result => result,
+    }
+    .context("Cannot create the OpenXR instance")?;
```

The fallback is in shared `client_openxr` code, so it also affects Android. There it only triggers on runtimes that reject 1.1, which currently fail outright. Alternatively, it could be gated with `#[cfg(target_os = "linux")]`. If the fix is not taken, the README should state "OpenXR 1.1 loader and runtime (Monado ≥ 24)" as a requirement.

As shipped on the branch (round 2), the fallback is gated with `#[cfg(not(target_os = "android"))]`, so Android keeps a single `create_instance` call with its per-device version.

To test the rest of the client without editing the branch, I used a test-only loader shim. It is a 30-line C `libopenxr_loader.so.1` put on `LD_LIBRARY_PATH` that forwards to the system loader and rewrites `apiVersion` 1.1 to 1.0.34 in `xrCreateInstance`. This is equivalent to the fix above. All results below use the original branch binary with this shim, apart from the patched run already described.

## Result 2: lobby works (with the 1.0 fallback)

| Criterion | Result |
|---|---|
| OpenXR instance created | yes: `LOG in xrCreateInstance: Instance created`, devices `Head: 'Simulated HMD'` |
| EGL surfaceless + MNDX session | yes. Client: `No windowing system present. Using surfaceless platform`, `libEGL debug: Falling back to surfaceless swrast without DRM.` (llvmpipe). `xrCreateSession` OK, `xrGetOpenGLESGraphicsRequirementsKHR`, 2 swapchains 1280x1440 `VK_FORMAT_R8G8B8A8_UNORM` created on the Monado side. Hiding `DISPLAY` in main.rs works: the client went surfaceless while Monado used `DISPLAY=:99`. |
| Lobby render loop | yes. Null compositor: 598 `xrWaitFrame`/`xrBeginFrame`/`xrEndFrame` in 30 s (about 20 fps). XCB on Xvfb: 1655 frames in 30 s (about 55 fps), 1125 in 20 s with the patched binary. The stall watchdog never fired. Xvfb screenshot shows both eyes with "ALVR v21.0.0-dev14 / hostname … / Searching for streamer… / Open ALVR on your PC then click "Trust"" ([linux-client-smoke-test-lobby.png](linux-client-smoke-test-lobby.png)). |
| SIGTERM (null, after 30 s) | exit 0 in 2.7 s. `Exit requested, ending the OpenXR session` → `xrRequestExitSession` → `xrEndSession` → all spaces/swapchains/actions/session/instance destroyed. Monado: `Stopping native session, 0 active app session(s)`. No panic. |
| SIGTERM (XCB, after 30 s) | exit 0 in 2.5 s, same sequence |
| SIGINT (null, after 15 s) | exit 0 in 1.4 s, same sequence |
| SIGHUP (null, after 15 s) | exit 0 in 1.4 s, same sequence |
| Two SIGINTs 20 ms apart | exit 1 in 0.15 s, as designed (`_exit(1)`). There is no `xrEndSession`, and Monado survives and cleans up the IPC client. |
| Client started without monado-service (shim) | Monado prints its "Please make sure that the service process is running" banner, then `Cannot create the OpenXR instance: the runtime failed …`, exit 1, no panic. |

Benign noise in the client log:

- `EGL 'eglInitialize' code 0x3001: DRI2: failed to load driver` ×2: Mesa tries the hardware driver first because there is no `/dev/dri`.
- `No config found!` ×2: wgpu tries EGL config tiers.
- ` WARN [vk_format_to_gl] Cannot convert VK format 100 to GL format!`: Monado's format probing.
- `Error reading ALVR config. Using default`

Minor observations:

- Shutdown after the first signal takes 1.4–2.7 s. The OpenXR teardown itself is quick, so the time is probably spent joining client-core threads. It is acceptable, but worth knowing for a systemd `TimeoutStopSec`.
- Not exercised: `ALVR_EXIT_ON_STALL`, because no stall happened.
- Not exercised: controllers and hand tracking, because the simulated HMD has no controllers. The 44 Index-profile actions were created and bound without errors.

## Result 3: streaming (not tested), mock-server feasibility

`upstream/mock-server` (`da7cf11e feat: Add server_mock`, 93 commits behind master) adds `alvr/server_mock`. It is about 170 lines on `alvr_server_core`: it reads `static_resources/test_video.mp4` with the `mp4` crate, sends the H.264 SPS/PPS through `set_video_config_nals`, and sends samples through `send_video_nal` at the track frame rate, using the head pose from tracking. It also touches the dashboard, the launcher and xtask.

It could act as a streamer for the handshake and the video path:

1. Port the crate onto master. This is mostly the `main.rs` and Cargo entries. The server_core API may have drifted, and the protocol ID must match, which it does when both sides are built from the same tree.
2. Make an H.264 MP4 with `ffmpeg -c:v libx264`.
3. Pre-trust the client in the mock's session.

I estimate 1–2 h. In this sandbox it would only get as far as connection, stream config and decoder creation. `alvr_client_core/src/video_decoder/linux.rs` is VAAPI-only with no software fallback, and there is no `/dev/dri`, so decoding and the dma-buf/EGLImage import path cannot run here. That needs a machine with a VAAPI GPU. I did not attempt it.

## Cleanup

The Monado/Vulkan packages installed for this test were purged afterwards (`apt-get purge` of the newly installed packages), Xvfb was stopped, and the scratch source copy was deleted. The shared target was not cleaned. The patched-copy build added about 40 s worth of ALVR crate artifacts to it.

The logs, the shim, the harness and the fix diff are available on request. To reproduce, reinstall the packages with the apt line above.
