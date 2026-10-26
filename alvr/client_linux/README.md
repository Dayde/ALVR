# alvr_client_linux

Desktop Linux client. It runs `alvr_client_openxr` as a regular OpenXR application on a local
runtime, which drives a PC VR headset, and receives the stream from an ALVR streamer running on
another PC:

```text
headset <-- DisplayPort/USB -- Monado <-- OpenXR -- alvr_client_linux <-- network -- ALVR streamer
```

- The OpenXR session is created with `XR_MNDX_egl_enable`, so the runtime must support it
  ([Monado](https://monado.freedesktop.org/) does).
- Video is decoded with FFmpeg and VAAPI, and the decoded frames are imported into the renderer
  as dma-bufs, without copies. H.264 and HEVC have been used on real hardware.
- At startup the client asks the VAAPI driver what it can decode, and only offers AV1 and 10 bit
  to the streamer if the GPU supports them (AV1 profile 0, HEVC Main10, and 10 bit AV1 if AV1 is
  offered). Otherwise the streamer falls back to HEVC and 8 bit. If the driver cannot be queried,
  neither is offered. The decision is logged (`VAAPI decoder capabilities: ...`, with
  `RUST_LOG=info`). AV1 and 10 bit have not been tested on real hardware yet.
- If the GPU still cannot decode the stream (for example a resolution above the driver's limits),
  the client reports a decoder error and reconnects: change the codec or resolution on the
  streamer.
- The client never opens a window: it uses EGL's surfaceless platform and ignores `DISPLAY` and
  `WAYLAND_DISPLAY`. It does so by changing these variables for the whole process, so they are
  also hidden from the OpenXR runtime if it runs in-process and needs X11 or Wayland. Monado as
  packaged by distributions runs its compositor in `monado-service`, which is not affected.

## Requirements

Runtime:

- An OpenXR runtime with `XR_MNDX_egl_enable` and `XR_KHR_opengl_es_enable` (Monado), set as the
  active runtime or selected with `XR_RUNTIME_JSON`. OpenXR 1.0 runtimes and loaders work: the
  client falls back to OpenXR 1.0 when 1.1 is rejected. This is what makes the packages of
  Ubuntu 24.04 (Monado 21, loader 1.0.20) work.
- The OpenXR loader (`libopenxr_loader.so.1`, package `libopenxr-loader1` on Debian/Ubuntu)
- A VAAPI driver that can decode the codec selected on the streamer, and an EGL implementation
  with `EGL_EXT_image_dma_buf_import` (and `EGL_EXT_image_dma_buf_import_modifiers` for tiled
  surfaces). Used on AMD with Mesa's radeonsi. Intel and NVIDIA (nvidia-vaapi-driver) are
  untested.
- FFmpeg's libavcodec and libavutil (FFmpeg 6.x to 8.1)

Build, on top of the usual ALVR build dependencies:

- `pkg-config`, `clang` (for bindgen)
- `libavcodec-dev`, `libavutil-dev` and `libva-dev`

## Build and run

```sh
cargo build -p alvr_client_linux --release
RUST_LOG=info target/release/alvr_client_linux
```

Logs are printed to stderr. By default only errors are shown, set `RUST_LOG=info` to see more.
Once connected, the log filter and debug groups set on the streamer are also applied.

The client ends the OpenXR session and exits on `SIGTERM`, `SIGINT` and `SIGHUP`. This takes one
to three seconds, so a service manager should wait a bit longer than that before killing it (the
systemd default `TimeoutStopSec` of 90 s is fine). A second signal makes it exit immediately.

## Environment variables

| Variable                  | Effect                                                               |
| ------------------------- | -------------------------------------------------------------------- |
| `ALVR_VAAPI_DEVICE`       | DRM render node used for decoding, for example `/dev/dri/renderD129` |
