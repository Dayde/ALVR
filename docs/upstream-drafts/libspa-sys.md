# libspa-sys / bindgen / PipeWire 1.6: investigation notes

Status: **ALVR master already fixes this. No `upstream/bump-pipewire` branch is needed.**

## TL;DR

- PipeWire 1.6 is not the trigger. **libclang 22** is: LLVM 22 changed how clang represents tag types (llvm/llvm-project#147835). bindgen versions older than **0.72.1** then emit some structs as opaque `{ _address: u8 }`, without their fields.
- ALVR v20.14.1 pins `pipewire`/`libspa`/`libspa-sys`/`pipewire-sys` 0.8.0. That crate set asks for `bindgen = "0.69"` (0.69.5 gets resolved). With libclang 22, `libspa-sys` 0.8.0 emits `struct spa_pod_builder` as opaque. The `libspa` 0.8.0 crate then fails to compile:
  ```
  error[E0609]: no field `data` on type `spa_pod_builder`
    --> libspa-0.8.0/src/pod/builder.rs:38:25
     |
  38 |         (*this).builder.data = (*this).data.as_mut_ptr().cast::<c_void>();
     |                         ^^^^ unknown field
     = note: available field is: `_address`
  error[E0609]: no field `size` on type `spa_pod_builder`
    --> libspa-0.8.0/src/pod/builder.rs:39:25
  ```
  This happens with PipeWire 1.0.5 headers too, so it is independent of the PipeWire version. A distro that ships PipeWire 1.6 usually also ships LLVM 22, so the two changes tend to arrive together.
- The vendored workaround on `origin/linux-client-v20.14.1` (`98d7286d`) works. It is the crates.io `libspa-sys` 0.8.0 with only the bindgen requirement changed from `0.69` to `0.72` (resolves to 0.72.1). It is wired in through `[patch.crates-io]`.
- ALVR master already moved to `pipewire` 0.9 / `libspa-sys` 0.9 in **a6475658 "fix-pipewire to compile again" (#3242)** (Vixea, merged 2026-04-16). Master's `Cargo.lock` has pipewire 0.9.2, libspa 0.9.2, libspa-sys 0.9.2, pipewire-sys 0.9.2 and bindgen 0.72.1. That combination builds and generates correct bindings with libclang 18, 20 and 22, against PipeWire 1.0.5 headers and 1.6.10 headers.

## Root cause in detail

- In clang 22, a `TagType` points to the declaration that lookup found, not necessarily to the definition. Take a forward declaration followed later by the definition:
  ```c
  struct spa_pod_builder;              /* spa/pod/builder.h:43 */
  struct spa_pod_builder_callbacks { ... int (*overflow)(void *data, uint32_t size); };
  struct spa_pod_builder { void *data; uint32_t size; ... };
  ```
  Old bindgen reads the incomplete forward declaration and emits an opaque struct.
- Fixed in bindgen PR rust-lang/rust-bindgen#3278, "Fix type completeness issues with clang 22". It was merged 2025-08-27 and released in **bindgen 0.72.1** (changelog: "Fixed a bad interaction with upcoming clang 22 (#3278)").
- Related bindgen issues:
  - #3264, "Missing fields in nested class with LLVM 22"
  - #3275, "Bad bindings for typedef to incomplete struct after Clang 22 change"
  - #3377, "Clang 22 failures"
  - #3383: bindgen silently emits opaque structs when libclang is newer than it supports (examples come from 0.69.5 and 0.71)
- The same forward declarations exist in PipeWire 1.0.5 and 1.6.10 (`spa_graph*`, `spa_pod_builder`, `spa_ringbuffer`, `spa_source`, `spa_thread`). Nothing new in 1.6 triggers the bug.
- Opaque structs with bindgen 0.69.5 + libclang 22 (struct counts are the same with 1.0.5 and 1.6.10 headers):

  | Crate | 1.0.5 headers | 1.6.10 headers |
  |---|---|---|
  | libspa-sys 0.8.0 | 9 structs | 8 structs |
  | pipewire-sys 0.8.0 | 3 structs | 3 structs |

  - libspa-sys 0.8.0 (1.6.10 headers): `spa_graph`, `spa_graph_link`, `spa_graph_node`, `spa_graph_port`, `spa_handle`, `spa_pod_builder`, `spa_ringbuffer`, `spa_source`. One more with 1.0.5 headers.
  - pipewire-sys 0.8.0: includes `pw_mempool` and `pw_memmap`.
  - Only `spa_pod_builder` breaks compilation, because the `libspa` crate accesses its fields. The others are silent. `spa_pod_dynamic_builder` embeds `spa_pod_builder` by value, so its layout is wrong too. bindgen 0.69 puts its layout checks in `#[test]` functions, so a normal build doesn't catch this.
  - The vendored workaround patches only `libspa-sys`. `pipewire-sys` 0.8.0 still uses bindgen 0.69.5 and has the opaque `pw_mempool`/`pw_memmap` under libclang 22. ALVR and pipewire 0.8.0 don't use their fields, so it is harmless for ALVR. It is still a reason to prefer master's 0.9.2 over the vendored crate.
- **bindgen 0.72.0 is also broken** with libclang 22. Tested by forcing `cargo update -p bindgen --precise 0.72.0` with libspa-sys 0.9.2. 0.72 turns the layout checks into const assertions, so the build fails loudly:
  ```
  error[E0080]: attempt to compute `1_usize - 48_usize`, which would overflow
  ... (9 errors)
  error: could not compile `libspa-sys` (lib) due to 9 previous errors
  ```
  `libspa-sys` and `pipewire-sys` 0.9.2, 0.10.0 and 0.10.1 all declare `bindgen = "^0.72.0"`. A fresh resolution picks 0.72.1. Only a lockfile that is stuck at 0.72.0 is affected.

## Not this bug: the 128-channel break (ALVR issue #3062)

alvr-org/ALVR#3062, "Unable to build from source" (Fedora Rawhide, PipeWire 1.5.81) failed with:

```
expected an array with a size of 128, found one with a size of 64
```

That error is in `libspa` 0.8.0's `src/param/audio/raw.rs`. PipeWire 1.5.81 had raised `SPA_AUDIO_MAX_CHANNELS` to 128 in the public header. The change was reverted in pipewire commit c94aff8 ("This change causes a subtle ABI change and also breaks the Rust bindings"). After 6d74eee87, the public default is 64 again: 1.5.85, 1.6.0 and 1.6.10 all `#define SPA_AUDIO_MAX_CHANNELS 64u`. Only PipeWire's internal build uses `-DSPA_AUDIO_MAX_CHANNELS=128u`; the `.pc` Cflags don't. So PipeWire 1.6.x headers work with libspa 0.8.0 on this point, and the test matrix confirms it.

## Test matrix

Setup:
- Scratch crates under `/home/user/scratch-libspa`, each depending on `pipewire = "=X"` (feature `v0_3_49`, as ALVR uses it) plus `libspa-sys = "=X"`. Each runs a full `cargo build`.
- PipeWire 1.6.10 headers come from the GitHub mirror (`github.com/PipeWire/pipewire`, tag 1.6.10), installed in the meson layout (`spa-0.2/spa`, `pipewire-0.3/pipewire{,/extensions{,/session-manager}}`). `version.h` is generated from `version.h.in`. Hand-written `.pc` files are selected through `PKG_CONFIG_PATH`.
- System headers: PipeWire 1.0.5 (Ubuntu 24.04).
- libclang:
  - 18.1.3 (system)
  - 20.1.2 (noble `libclang-20-dev`)
  - 22.1.2 (Ubuntu resolute `libclang1-22`/`libllvm22`, signature-checked with apt, extracted with `dpkg -x` and loaded through `LIBCLANG_PATH`; strace confirmed `libclang-22.so.22` was the one loaded)

| crates (bindgen) | headers | clang 18 | clang 20 | clang 22 |
|---|---|---|---|---|
| pipewire/libspa-sys 0.8.0 (bindgen 0.69.5) | PW 1.0.5 | OK | OK | **FAIL** (E0609 `spa_pod_builder`) |
| pipewire/libspa-sys 0.8.0 (bindgen 0.69.5) | PW 1.6.10 | OK | OK | **FAIL** (E0609 `spa_pod_builder`) |
| 0.8.0 + vendored libspa-sys from 98d7286d (bindgen 0.72.1) | PW 1.0.5 | – | – | OK |
| 0.8.0 + vendored libspa-sys from 98d7286d (bindgen 0.72.1) | PW 1.6.10 | OK | – | OK |
| pipewire/libspa-sys 0.9.2 (bindgen 0.72.1) | PW 1.0.5 | OK | OK | OK |
| pipewire/libspa-sys 0.9.2 (bindgen 0.72.1) | PW 1.6.10 | OK | OK | OK (0 opaque structs) |
| pipewire/libspa-sys 0.9.2 with bindgen forced to 0.72.0 | PW 1.6.10 | – | – | **FAIL** (E0080 const layout asserts) |

ALVR master (`adaa16ba`), `cargo build -p alvr_audio` in a detached worktree:

| Headers | libclang | Result |
|---|---|---|
| system PipeWire 1.0.5 | 18 | OK |
| PipeWire 1.6.10 | 22 | OK, `spa_pod_builder` has its full fields |

Not tested:
- libclang 21 and 23. No distro packages were available here, and a conda-forge download was refused by the sandbox policy.
- PipeWire 1.6 *libraries* at runtime. Only headers were used; linking used the system 1.0.5 `libpipewire-0.3.so`.

## Links

- ALVR master fix: a6475658 "fix-pipewire to compile again" (#3242), https://github.com/alvr-org/ALVR/pull/3242. The PR has no description; it moves to pipewire/libspa-sys 0.9 and the `*Rc` API.
- Vendored workaround: `98d7286d` on `Dayde/ALVR` branch `linux-client-v20.14.1`
- Unrelated earlier break: https://github.com/alvr-org/ALVR/issues/3062 (PipeWire 1.5.81, 128 channels), fixed in PipeWire: https://github.com/PipeWire/pipewire/commit/c94aff8cae158def64e20a320dcc1b1020d1d795 and 6d74eee874ac92d587050218bf7db95fefa4ef6a
- bindgen fix: https://github.com/rust-lang/rust-bindgen/pull/3278 (released in 0.72.1); issues https://github.com/rust-lang/rust-bindgen/issues/3264, https://github.com/rust-lang/rust-bindgen/issues/3275, https://github.com/rust-lang/rust-bindgen/issues/3377, https://github.com/rust-lang/rust-bindgen/issues/3383
- LLVM change: https://github.com/llvm/llvm-project/pull/147835
- crates.io releases:
  - libspa-sys: 0.8.0 (2024-02-06), 0.9.2 (2025-09-15), 0.10.0 (2026-05-17), 0.10.1 (2026-08-19)
  - pipewire: the same version numbers and dates as libspa-sys
- pipewire-rs: https://gitlab.freedesktop.org/pipewire/pipewire-rs. **Not checked:** gitlab.freedesktop.org is unreachable from this sandbox (DNS/proxy refusal), so I could not search its issues and MRs. The bindgen 0.69 → 0.72 move is visible in the published crates: 0.8.0 asks for `^0.69` and 0.9.2 asks for `^0.72.0`.

## Is a pipewire-rs issue warranted?

Mostly no. The fix (bindgen ≥ 0.72.1) is already in every pipewire-rs release since 0.9.x, and a fresh lockfile resolves to it. The only remaining gap is small: the published manifests still say `bindgen = "0.72.0"`, which allows the broken 0.72.0. Someone whose lockfile is pinned at 0.72.0 gets confusing layout-assert errors with libclang 22.

Before filing anything, check pipewire-rs on GitLab, which I could not reach from here:
- Has main already bumped to bindgen 0.72.1 or 0.73?
- Does an issue or MR about clang 22 already exist?

If neither, this text is ready to paste. **Do not file it automatically.**

> **Title:** Require bindgen >= 0.72.1 (0.72.0 generates broken bindings with libclang 22)
>
> libspa-sys and pipewire-sys (0.9.2, 0.10.0, 0.10.1) declare `bindgen = "0.72.0"` as a build-dependency. bindgen 0.72.0 does not include rust-lang/rust-bindgen#3278. With libclang 22 (llvm/llvm-project#147835), that version emits forward-declared structs such as `spa_pod_builder` as opaque `{ _address: u8 }`. libspa-sys then fails with layout const-asserts:
>
> ```
> error[E0080]: attempt to compute `1_usize - 48_usize`, which would overflow
> error: could not compile `libspa-sys` (lib) due to 9 previous errors
> ```
>
> Reproduce with libspa-sys 0.9.2 + `cargo update -p bindgen --precise 0.72.0` + `LIBCLANG_PATH` pointing to libclang 22.1.2. Headers: PipeWire 1.0.5 or 1.6.10. bindgen 0.72.1 works.
>
> Raising the requirement to `bindgen = "0.72.1"` in `libspa-sys/Cargo.toml` and `pipewire-sys/Cargo.toml` would stop a lockfile stuck at 0.72.0 from producing this hard-to-diagnose failure. Older releases (0.8.0, `bindgen = "0.69"`) fail with libclang 22 as well (`no field 'data' on type 'spa_pod_builder'` in libspa); the fix there is to upgrade to 0.9 or later.

## Advice for the user's branch

`linux-client-v20.14.1` is based on v20.14.1 and still uses pipewire 0.8. The vendored libspa-sys works. Two cleaner alternatives:
- Port a6475658 (pipewire 0.9 + the `*Rc` API changes in `alvr/audio/src/linux.rs`). That also gets `pipewire-sys` onto bindgen 0.72.1.
- Build with an older libclang (`LIBCLANG_PATH=/usr/lib/llvm-20/lib` or similar), as the reporter of bindgen issue #3383 did.

For anything rebased onto upstream master, drop the vendored crate: master doesn't need it.
