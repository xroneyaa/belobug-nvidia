# VIctory fork for NVIDIA (RTX 3060 Ti / Wave32) & AMD (work in progress)

This fork of [VIctory](https://github.com/Marvisak/VIctory) builds on
upstream `main` (through `8e79c80`, 2026-11-24) and
[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD),
adding Wave32 / NVIDIA D3D12 support, pipeline scheduling fixes, texture budget
management, and host/title memory synchronization fixes.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

## State of this fork (2026-11-26)

Development and offline shader/D3D12/workspace checks in this fork target
**Windows 11**, **NVIDIA GeForce RTX 3060 Ti (8 GB VRAM, SM 6.6, Wave32
`32-32`)**, while keeping the prior `Belobug` AMD RX 9070 XT (16 GB) base
intact.

| Area | Current state |
|------|---------------|
| Target adapters | NVIDIA RTX 3060 Ti 8 GB (Wave32 `32-32`, capped descriptor heap fallback); prior base also ran on AMD RX 9070 XT 16 GB (Wave64) |
| Shader translation (Wave32 `32-32`) | **25,491 / 25,889** programs emit DXIL across 26,503 containers in the shader archive (614 duplicates skipped; 398 unsupported, see below) |
| Live in-game status (RTX 3060 Ti, pre-batch baseline) | Before this batch of fixes, menus dropped from `70+ fps` to `~2 fps` and in-world cutscenes showed sky/UI over black unlit geometry (`~1 fps`, `1467–2511 draws/frame`) |
| Live in-game status (after 2026-11-26 fixes) | **Not yet re-run in-game here** — verified so far by workspace builds, unit tests, D3D12 pipeline/copy tests, and offline full-archive shader translation |
| Ray tracing (`VICTORY_RAYS`) | Disabled by default; BVH builder/wind compute passes stay skipped unless opted in |

### What this fork changes

#### NVIDIA & Wave32 hardware compatibility (`victory-shader`, `victory-gpu`)
- **Wave64-on-Wave32 lowering (`crates/victory-shader/src/emit/lower.rs`)**:
  scalars derived from VALU/VGPR sources remain CTA-uniform on single-wave
  Wave32 dispatches (`group <= 64`), letting LDS barriers and stores lower
  without spurious Wave64 retry failures.
- **Scalarized light-loop `OwnBits` analysis (`crates/victory-shader/src/emit/own_bits.rs`)**:
  proves when a `WaveActiveBitOr` only drives a loop over each lane's own
  set bits and allows a 32-lane wave OR to stand in for 64 lanes
  (`VICTORY_WAVE_OWN_BITS=0` disables this).
- **Pre-SSA user-data unspilling & structured lane masks (`ir.rs`, `unspill.rs`, `sgpr_spill.rs`, `structure.rs`)**:
  user-data spill-table loads are unspilled in production builds on both
  Wave32 and Wave64, `would_unspill` gates un-unspilled BVH stages when
  `VICTORY_RAYS` is off, and `structure.rs` tracks incoming edge masks for phi
  joins (`phi_masks`).
- **Capped descriptor heap handling (`victory-gpu`, commits `9941b5e`, `0390c9e`)**:
  probes padded view heaps at startup and clamps view indices to the null slot
  on capped heaps (`2^18` dynamic slots), instead of failing shader translation
  or faulting out-of-bounds descriptors.

#### Pipeline queue, descriptor sync & VRAM/commit fixes (`victory-gpu`)
- **Graphics-first pipeline scheduling (`crates/victory-gpu/src/pipeline/make.rs`)**:
  splits urgent pipeline compilation into `urgent_graphics` (`graphics.is_some()`,
  preempting compute) and `urgent_compute`; marks pipelines used
  (`library::note_used`) before `enqueue`/`wait` so unbuilt urgent PSOs are not
  evicted prematurely; keeps background 20 s cache saves (`library::save(false)`)
  running even under urgent queue load; bounds frame waits via
  `VICTORY_PIPELINE_WAIT_MS` (default `500` ms) and `VICTORY_PIPELINE_WAIT_SHARED`
  (`0` disables shared per-kind budget).
- **`R9G9B9E5_SHAREDEXP` copy-bridge & descriptor sync (`descriptor/host.rs`, `draw.rs`, `host.rs`)**:
  uses a D3D12 buffer-bridge copy fallback for `R9G9B9E5_SHAREDEXP` (DXGI
  format `42`) via shared `HostHeap::placed_raw_texture` across both bindless
  (`resolve_placed`) and root-table (`use_slot`) descriptor paths, and flushes
  dirty shared-exponent copies at the end of `HostContext::bind` so both
  bindless and `Root::Table` draws see updated textures.
- **8 GB placed texture budget & lazy host textures (`resource.rs`, `texture.rs`, `059b5fd`)**:
  tracks active cached placed textures in `PLACED_CACHE_BYTES` and evicts idle
  placed textures by `HostTexture::used` (`evict_idle_placed_to`) before
  rejecting `texture_at` over budget, while keeping deferred retirement
  (`PLACED_BYTES`) intact.
- **Coalesced copy-back & sparse alignment guard (`host.rs`, `shadow.rs`, `61e1e4b`)**:
  groups `finish_submission` copy-backs by `Shadow` and merges overlapping or
  adjacent ranges (`merge_back_ranges`, `copy_back_many`, `sparse_back_many`),
  and adds an early `gcd_u64` impossible-alignment check in `sparse_on_element`
  without reintroducing blocking `memory_pressure()`/`sweep_now()` calls.
- **Multi-view write-watch coherence (`watch.rs`)**:
  unifies `guard` fast and slow paths (preserving active `pins`,
  `bump_region_epochs`, clean-range skips, and `holes` checks) and clears
  overlapping `pins` and stale `JOURNAL.written` timestamps in
  `title_memory_remapped`.

#### Upstream `Belobug` & inherited features retained
- Fallback ambient lighting (`VICTORY_AMBIENT`, `VICTORY_AMBIENT_TINT`,
  `VICTORY_AMBIENT_SHADE`, `VICTORY_AMBIENT_FILE`), integer render-target blend
  disable (PR #45), quad reads for helper pixels and `v_readfirstlane` fixes,
  multi-threaded sparse copy-backs (PR #18), `VICTORY_HOIST_CLAMPS`,
  optional `VICTORY_ASYNC_SUBMIT=1`, `VICTORY_SHADER_BUILD_OF`, and diagnostics
  switches (`VICTORY_SHADER_PATCH`, `VICTORY_SHADER_SWAP`, `VICTORY_ALWAYS_COPY`,
  `VICTORY_GPU_TIMING`).
- Targeted `victory translate` diagnostics: `VICTORY_TRANSLATE_FOR` and
  `VICTORY_TRANSLATE_ONLY=<hash,...>` in `apps/victory/src/translate.rs`.

### Verified builds & tests (2026-11-26)

All checks below ran with `--locked` on Windows 11 + RTX 3060 Ti (`host waves 32-32`):

- `cargo check --workspace --all-targets --locked`: **passed**
- `cargo build --release --locked` and `cargo build --profile diag -p victory --features diagnostics --locked`: **passed**
- Passing suites (`0` failed): `victory-gpu` (`130 passed; 4 ignored`),
  `victory` (`1 passed`), `victory-core` (`11 passed`), `victory-formats`
  (`24 passed; 4 ignored`), `victory-game` (`2 passed`), `victory-pe`
  (`7 passed`), `victory-pipeline` (`4 passed`), `victory-loader`
  (`22 passed; 1 ignored` lib + `4 passed` across `crash_log`, `load_probe`,
  and `title_dlls`).
- `cargo-clippy.exe` is not installed on this toolchain.

### Known problems

- **Unverified in-game visually after 2026-11-26 edits**: live playback of
  the title has not yet been re-tested after the shader, pipeline-queue,
  `use_slot` `R9G9B9E5` sync, and placed-texture budget fixes.
- **Wave32 unsupported console shaders**: across the title's shader archive,
  `398` of `25,889` programs still fail to translate (`195` non-VS/PS/CS/MS stages, `71` mesh
  shaders exceeding 32 KiB, `14` console wave64 lane ops, `8` LDS without
  barriers, and `opaque lane mask` / `conditional export` cases).
- **Pre-existing failing tests (4 + 5 + 1)**:
  - `victory-shader` (`279 passed; 4 failed; 6 ignored`): 4 pre-existing Wave32
    `emit::valu_tests` failures (`loop_exit_mask_counted_after_the_loop`,
    `mask_grown_inside_a_branch_counted_after_it`,
    `scalar_counted_in_a_divergent_loop_read_after_it`,
    `scalar_counted_in_nested_divergent_loops`).
  - `victory-hle` (`97 passed; 5 failed; 1 ignored`): 5 pre-existing
    `modules::xmem::tests` failures (`a_run_keeps_the_placeholders_it_spans`,
    `a_run_maps_across_two_placeholders`,
    `a_run_maps_again_after_a_partial_unmap`, `mapping_no_pages_changes_nothing`,
    `physical_pages_map_alias_and_unmap`).
  - `xtask` (`3 passed; 1 failed`): `policy::tests::repository_passes_policy`
    reports `crates/victory-shader/src/emit/lower.rs` (`677,419` bytes >
    `524,288`-byte asset-policy limit); it is Rust source, not a game asset.
- **Ray-traced indirect light (`VICTORY_RAYS=1`)**: un-unspilled BVH/ray
  compute stages remain skipped by default (`VICTORY_AMBIENT` supplies fallback
  indirect diffuse lighting).
- **Windows 10 / non-DX12 SM 6.6 hosts**: untested here (see upstream #9 / #44).

### Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors (GPL-3.0-or-later).
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**
  and merged upstream PRs **#45** (pgm1207 / Arvi23) and **#18** (PikPakPik).
- [fxceless/victory-fork](https://github.com/fxceless/victory-fork),
  [myokex/victory-amd](https://github.com/myokex/victory-amd), and
  [ivall/xcl](https://github.com/ivall/xcl) were consulted for design comparison
  by upstream/fork authors.

---

## From upstream's README

What follows is taken from upstream VIctory's own README: what the project
is, how to build it, its commands and its debugging switches. They apply to
this fork too. Upstream's status section is left out, because it describes
upstream's state and upstream's test machine (an RX 6700 XT), not this fork
or the machine above: read it
[in upstream's repository](https://github.com/Marvisak/VIctory#readme).

# VIctory

Runs Grand Theft Auto VI (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. A DualSense or DualSense Edge (USB or Bluetooth) is read
over HID as well and plays as the first pad when no XInput pad answers. With
no pad connected the keyboard is a pad while the title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, O | A, B |
| Backspace, X, Y | B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_BREAKPOINTS=1` | appends first-chance breakpoints to `crashes.log`, including ones a handler may recover from |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends unhandled exceptions, panics, and host GPU failures to
`crashes.log` in the title's data folder.

For a startup breakpoint (`0x80000003`), run the diagnostics build with
`VICTORY_CRASH_BREAKPOINTS=1`. Keep the console output along with the new
`crashes.log` entries. A host DLL refusal includes the caller stack in the
console output. The refusal is expected for DLLs outside the host policy,
including `rpcrt4.dll`.

On Series 1.0.2.1, a missing RPF9 data archive reproduced the breakpoint at
`+0x2237178` before the first window. Restore the archive from the same
package beside the title executable. `run --execute` checks that it can
read the RPF9 header before starting this build.

To compare hardware and software rendering, repeat with
`VICTORY_D3D12_WARP=1` and check the console for
`host device on the software adapter (WARP)` to confirm the WARP device was
created. Compare the exception's module and offset, stack frames, and
preceding messages across the two runs.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
