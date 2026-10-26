# fix(graphics): use highp texture coordinates in the staging shader

Branch: `Dayde:upstream/fix-staging-shader-precision` → `alvr-org:master` (base `adaa16ba`)

## Summary

<!-- TODO(Dayde): write this summary in your own words — required by ALVR's AI policy -->

Technical summary (rewrite before posting):

The staging fragment shader (`alvr/graphics/resources/staging_fragment.glsl`) now declares its texture coordinate as `in highp vec2 uv;`. The `precision mediump float;` default added in #3104 stays, so the color math is unchanged and only the coordinate moves to highp. The diff is two lines: the qualifier and a one-line comment saying why, so nobody turns it back to mediump.

With mediump, the coordinate can be quantized on GPUs that run mediump varyings at fp16 (e.g. Mali). There it cannot hit every texel center of the decoded side-by-side frame: the right eye ends up with only about 1024 distinct sample columns, whatever the view width is. Because the external texture uses linear filtering, the expected result is horizontal softening that gets worse as resolution goes up. This follows from the spec and from emulation; I have not confirmed it on any device (see Testing).

## Problem

`StagingRenderer` (`alvr/graphics/src/staging.rs`) copies each eye out of the decoder's `AHardwareBuffer`, bound as a `samplerExternalOES` with the default `LINEAR` filter. Each eye goes into its own staging texture, with the viewport sized to the staging resolution (`W x H` per eye). The decoded frame is `2W x H` (side by side), so the copy is meant to be texel-exact: 1 output pixel = 1 source texel.

With `precision mediump float;`, on a GPU that interpolates and stores mediump varyings as fp16, the sampled coordinate is quantized coarser than one texel. Some output pixels then become blends of two neighboring source texels, so the image would be slightly blurred (mostly horizontally, and mostly in the right eye), before foveation decoding, upscaling and the final composition even start.

Which devices this hits is uncertain. Mali phones (PhoneVR / the Android client) are the most likely to be affected, and Arm's GPU guidance recommends highp for texture coordinates for this reason. Adreno may sample an unmodified varying passed straight to `texture()` at full precision (texture-prefetch path), so Quest users may see no change.

## Root cause

**How the coordinate is produced.** `staging_vertex.glsl` draws a full-screen strip of 4 vertices and outputs `uv = ((screen_uv.x + view_idx) / 2, screen_uv.y)`. That is a highp vertex output (the vertex language defaults to highp). It is interpolated and read in the fragment shader as `in vec2 uv`, which takes the fragment default `mediump`. It does not use `gl_FragCoord`. GLSL ES 3.00.6 §4.3.10 (Linking of Vertex Outputs and Fragment Inputs): *"The minimum precision at which vertex outputs are interpolated is the minimum of the vertex output precision and the fragment input precision, with the exception that for highp, implementations do not have to support full IEEE 754 precision. In this case, the precision of the interpolated value is defined by a range and resolution as below:"* (the 3.00.6 document does not actually include a table after that sentence). So with the mediump default, `uv` may be interpolated and stored at mediump. With `highp`, interpolation is not strictly guaranteed to be full fp32 either, but it is no longer capped at mediump.

For output pixel `i` of eye `v`, the exact coordinate is `uv.x = (i + 0.5 + v*W) / (2W)`, which is the center of source texel `i + v*W`. Left-eye coordinates fall in `[0, 0.5)` and right-eye coordinates in `[0.5, 1)`.

**What mediump guarantees.** GLSL ES 3.00.6 §4.5.1, table "required ranges and precisions": mediump has a floating-point range of (−2^14, 2^14), a magnitude range of (2^−14, 2^14) and a *relative precision of 2^−10*. Highp is "As IEEE 754" (relative 2^−24). The ES 3.0.6 API spec (§6.1.12, `GetShaderPrecisionFormat`) defines `precision` as `−log2(ε)`, where `1 + ε` is the smallest value above 1. So mediump guarantees ε = 2^−10 at 1.0, which is exactly IEEE binary16 (10 explicit mantissa bits). The spec says it explicitly allows that (§12, Issues: *"The specification should allow efficient implementation of mediump float on 16-bit floating point hardware"*). Mobile GPUs such as Mali can use fp16 for mediump; whether a given driver does so for this varying is not something I could verify. Mesa llvmpipe, which I tested on, reports `MEDIUM_FLOAT precision 10` but runs it as fp32.

**The numbers.** For fp16, the spacing between representable values (ulp) is 2^−11 ≈ 4.88e−4 in `[0.5, 1)` and 2^−12 ≈ 2.44e−4 in `[0.25, 0.5)`. Round-to-nearest gives a maximum error of half an ulp. The source texel spacing is `1 / (2W)` in uv.x and `1 / H` in uv.y.

| Per-eye W x H (frame 2W) | Texel spacing 1/(2W) | Right-eye ulp in texels (= W/1024) | Right eye: max / mean error (texels) | Right eye: distinct columns | Left eye: max / mean error | Vertical max error |
|---|---|---|---|---|---|---|
| 1024 x 1024 (2048) | 4.88e−4 | 1.00 | 0.50 / 0.50 | 513 / 1024 | 0 / 0 | 0 |
| 1280 x 1280 (2560) | 3.91e−4 | 1.25 | 0.50 / 0.30 | 1025 / 1280 | 0.25 / 0.10 | 0.25 |
| 1824 x 1920 (≈ Quest 2 native, 3648) | 2.74e−4 | 1.78 | 0.88 / 0.45 | 1025 / 1824 | 0.44 / 0.15 | 0.44 |
| 2048 x 2208 (≈ Quest 3 native, 4096) | 2.44e−4 | 2.00 | 0.50 / 0.50 | 1025 / 2048 | 0.50 / 0.25 | 0.53 |
| 2560 x 2560 (5120) | 1.95e−4 | 2.50 | 1.00 / 0.60 | 1025 / 2560 | 0.50 / 0.20 | 0.50 |
| 3840 x 3840 ("4K" per eye, 7680) | 1.30e−4 | 3.75 | 1.75 / 0.93 | 1025 / 3840 | 0.88 / 0.31 | 0.88 |

(This was computed by rounding the exact fp32 coordinate of every output pixel to IEEE half with numpy. The per-eye sizes are ones the server can actually produce: `get_view_res` aligns each per-eye dimension down to a multiple of 32 (`alvr/server_core/src/connection.rs`, `align32`). At 2048 the right-eye error is exactly 0.5 texel at every pixel, so the whole right eye is a 50/50 blend. The left eye has 0.5 max / 0.25 mean, with 50 % of its pixels blended.)

The right eye can only use about 1024 distinct x coordinates (the fp16 values in [0.5, 1)), whatever W is. Above a per-eye width of 1024, columns get merged or blended. Even at exactly W = 1024, every right-eye texel center falls exactly halfway between two fp16 values, so the right eye is a 50/50 blend everywhere. For fp16 to hit texel centers exactly in the right eye, W must be 512 or less. Note that the sampled texture is the full side-by-side frame, so it is twice as wide as one eye.

**highp is safe on every context this shader is compiled in.** The shader is `#version 300 es` + `GL_OES_EGL_image_external_essl3`. It is only built through `GraphicsContext::new_gl()` (EGL + GLES 3, Android/Linux; `unimplemented!()` on Windows/macOS). It is used by `alvr_client_openxr` and by the `alvr_client_core` C API (PhoneVR / the Android app). In GLSL ES 3.00, highp float is required in both shader stages: §4.5.1 *"highp floating point values are stored in IEEE 754 single precision floating point format"*, with no optional fragment-highp clause (that option only existed in GLSL ES 1.00). So no `#ifdef GL_FRAGMENT_PRECISION_HIGH` fallback is needed.

## Fix

```diff
-in vec2 uv;
+// mediump (fp16 on mobile GPUs) is too coarse to address every texel of the decoded frame
+in highp vec2 uv;
```

Why only `uv` and not `precision highp float;`: the color path (`texture()` returns the `lowp` default of `samplerExternalOES`, then the optional limited-range expansion) is fine at mediump, and #3104 deliberately chose mediump for the default (see Related). Raising only the coordinate fixes the addressing and leaves every other computation as it is today. One highp `vec2` varying feeding one texture fetch per fragment has negligible cost in a full-screen copy pass.

**Alternative for the maintainer:** a global `precision highp float;` would most likely also be safe (see Risks: every wgpu pass in the client and the old FFR shader already run highp on the same contexts). I kept the narrower change to stay close to #3104, but I'm happy to switch if that is preferred.

## Testing

All commands were run in the worktree with `CARGO_TARGET_DIR=/home/user/target-shared`, on Linux x86_64.

- `cargo fmt --all -- --check`: pass. `cargo xtask format` + `git diff`: no changes.
- `cargo xtask clippy --ci`: pass, with 0 Rust/clippy warnings. The only output is pre-existing gcc warnings from the `alvr_server_openvr` C++ build script and the `proc-macro-error2` future-incompat note, both unrelated.
- `cargo test -p alvr_graphics`: pass (0 tests). `cargo test -p alvr_session`: pass (4 tests).
- `cargo build -p alvr_client_openxr` (host): pass.
- `cargo check -p alvr_graphics --target aarch64-linux-android` and `--target x86_64-pc-windows-gnu`: pass. The shader is `include_str!`'d, so this does not validate GLSL. mingw is only an approximation of CI's MSVC.
- **GLSL validation (glslang 15.1.0):** I reproduced the exact final sources from `StagingRenderer::new`: `str::lines()`, then (with `fix_limited_range`) inserting `"#line 0 1\n#define FIX_LIMITED_RANGE"` after `#version`, then `join("\n")`. With `glslangValidator`, both variants compile and link with `staging_vertex.glsl`. The intermediate tree shows `'uv' (smooth in highp 2-component vector of float)` and `texture (…, operation at highp)`, while `LIMITED_MIN` and the range-expansion `add` stay mediump. On master the same tree shows `'uv' (smooth in mediump …)`.
- **Real GLES driver (Mesa 25.2.8 llvmpipe, EGL surfaceless, OpenGL ES 3.2):** a throwaway C program compiles and links the exact sources (both `FIX_LIMITED_RANGE` variants) and confirms `GL_OES_EGL_image_external_essl3` is present. Fragment `MEDIUM_FLOAT` reports precision 10 and `HIGH_FLOAT` reports 23. The program wraps a `2W x H` RGBA8 `GL_TEXTURE_2D`, where every texel encodes its own x/y, in an EGLImage (not a YUV `AHardwareBuffer` as on Android), binds it as `GL_TEXTURE_EXTERNAL_OES` (default `LINEAR`), runs the staging draw for both views, and reads the result back. The pixel-exact comparison was done only with `FIX_LIMITED_RANGE` off; with it on, only compile and link were checked.
  - Patched shader: **0 mismatching pixels** in both eyes at 1832x1920, 2064x2208 and 3840x3840. This shows the vertex math plus highp is texel-exact on this driver.
  - Master shader: also 0 mismatches, because llvmpipe runs mediump as fp32. That is why this can't be reproduced on desktop Mesa.
  - Master with fp16 emulated (`texture(tex, unpackHalf2x16(packHalf2x16(uv)))`):

    | Per eye | Left eye wrong pixels | Right eye wrong pixels |
    |---|---|---|
    | 1024x1024 | 0 % | 50.3 % |
    | 1832x1920 | 0.7 % | 44.6 % |
    | 2064x2208 | 4.8 % | 52.7 % |
    | 2560x2560 | 19.4 % | 64.1 % |
    | 3840x3840 | 41.6 % | 79.7 % |

**Not tested:**
- On a headset or phone (Quest 1/2/3, Pico, Lynx, Mali phones). I have not confirmed the blur on real hardware or measured performance.
- The Android build (`cargo xtask build-client`), because the NDK can't be downloaded in this environment.
- Adreno may already sample an unmodified varying passed straight to `texture()` at full precision (texture-prefetch path), which would hide the issue on Quest. Mali is the most likely place to see it. Please check by comparing on-device captures of a fine vertical-line test pattern, left eye vs right eye, before and after.

## Related issues/PRs

- #3104 (merged), "fix: add precision default to staging fragment shader", fixes #3103 (Mali-style `S0032: no default precision defined for variable 'LIMITED_MIN'` on a Redmi Note 13 Pro). **Possible conflict:** in that PR, zmerp wrote that *"Highp will most likely break ALVR on many smartphones … Possibly also old headsets like Quest 1"*. He recalled that highp made FFR "just break and the screen is a solid color" on phones, and that his old FFR code "was using default highp" before being lowered because of the "white snow" bug. This PR is designed around that concern: the default stays mediump and only the texture coordinate is highp, as GLSL ES 3.00 requires every implementation to support. The old reports concerned the much heavier FFR shader. The v20.8.0 notes attribute the "white snow" fix to #2083, which did not touch precision. The 2023 sRGB pass (the predecessor of this shader) was switched from highp to mediump in #1434 with no stated reason. Testing on a phone and a Quest 1 would settle it.
- Prior highp use on the same contexts: the old C++ FFR shader shipped with `precision highp float;` from v20.0.0 to v20.11.0 (`git show v20.11.0:alvr/client_core/cpp/ffr.cpp`, line 15). Every wgpu pass in the client also already runs fully highp, because naga's GLSL backend writes `precision highp float; precision highp int;` into every GLSL ES shader it emits (see Risks).
- #2839 (closed) has a later comment showing the same S0032 crash as #3103, already fixed by #3104.
- I found no issues for "mediump"/"highp" beyond those, "blurry right eye", "pixelated resolution", or "Bigscreen Beyond" that describe this symptom.
- No open PR touching the staging shader was found. zmerp mentioned a planned move to wgpu external textures, which would replace this GLES pass.

## Risks

- A GLES 3 driver with broken fragment highp. This is spec-mandated, so it should not exist, but #3104's discussion suggests caution. The evidence that it is safe in practice is strong:
  - The client's wgpu instance uses the GL backend (`alvr/graphics/src/lib.rs`, `Backends::GL`). naga 25.0.1's GLSL backend writes `precision highp float;` and `precision highp int;` into every GLSL ES shader (`naga-25.0.1/src/back/glsl/mod.rs:721-722`). So every other wgpu pass in the client already runs fully highp on the same EGL contexts and devices.
  - The old C++ FFR shader used `precision highp float;` from v20.0.0 to v20.11.0.
  - If such a driver did exist, the failure would be limited to the staging copy.
- Performance: one highp varying in a single-fetch copy pass. Expected to be negligible, but not measured.
- No behavior change on desktop/llvmpipe (already fp32).

- [ ] This PR respects our AI policy: I completely understand and "own" the coding decisions made, and can discuss them without relying on AI copy-pasted responses.
- [ ] This PR's description contains a human-written summary of the content of the code.
- [ ] Testing: ...
