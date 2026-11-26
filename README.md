# VIctory under Proton on AMD (RADV)

A fork of [VIctory](https://github.com/Marvisak/VIctory), the compatibility layer
that runs an Xbox Series title on a PC from files the user supplies. This fork is
about one thing: **running it on Linux through Proton on an AMD card with the Mesa
RADV driver.** It collects the fixes that needed, each with the evidence for it.

No game data is in this repository and none will be (see
[docs/asset-policy.md](docs/asset-policy.md)).

## State (2026-11-25)

Tested on **one machine only**: CachyOS (kernel 7.3), Radeon RX 9070 16 GB, Mesa
RADV 26.3.1, Ryzen 7 9800X3D, 64 GB RAM, Proton Experimental with a patched
wineserver (below), 3840x2160, the Series X profile.

| | |
|---|---|
| Boots, loads a save, reaches the world | yes |
| Longest session without a crash or a lost device | 74 minutes |
| Frame time in the world | 170 to 330 ms (3 to 6 frames a second), rising with the scene: about 123 ms + 33 ms per 1,000 draws with the speed switches below |
| Characters' heads, facial animation, eyes | right |
| Water (things float, swimming) | right |
| Prologue | played through; a save was written after it |
| Ray-traced light | off (see [docs/proton.md](docs/proton.md), "Ray tracing"): rooms without direct light are dark |

**It is not playable for pleasure at this frame rate.** It is a base for people who
want to work on the Linux side.

Known wrong, not yet explained:

- Coloured glows (green, pink) on some lights and on rain: one particle program's
  result is wrong although every input to it was checked and is right. The same is
  seen with other forks under Proton.
- Skin looks glossy when the card's memory is full (normal and specular maps not yet
  on the card). An eviction of unread textures is in the tree, off by default
  (`VICTORY_TEXTURE_EVICT`): it has run clean in tests and was on in the one session
  that lost the device, cause not found.
- Frame time roughly doubles over an hour as scenes get heavier and the card fills.
- One device loss at world entry (a read near address 0) on the real screen that
  no off-screen run reproduces.

## What this fork adds for Proton

Each has a section in [docs/proton.md](docs/proton.md) with how it was found. The
switch named turns it off (`=0`) unless noted.

| Problem under Proton | Fix | Switch |
|---|---|---|
| Frames of seconds: every commit of memory walks the wineserver's list of views | A wineserver patch (a tree instead of the list); see "The patched Proton" | (a Proton build, not this code) |
| Pipelines stuck "being made": vkd3d-proton's shader converter recursed to a stack overflow on loops with two back edges | Loops are emitted with a single latch | always on |
| Characters without heads: a compute dispatch issued once was dropped when its pipeline was not made yet | Compute dispatches wait for their pipeline | `VICTORY_COMPUTE_WAIT` |
| GPU page fault inside a texture made a moment ago, device lost after minutes | A view of a fresh texture is written into its heap slot only when no submission is in flight | `VICTORY_PUBLISH_SAFE` |
| GPU page faults at addresses near 0 | Ray-tracing passes that are handed address 0 read zeros there; null buffer views read zeros | `VICTORY_MEMORY_RUN_FIRST`, `VICTORY_NULL_BUFFER_ZEROS` |
| Faces still, eyes glowing | Table-bound dynamic slots carry their integer-kind word | `VICTORY_TABLE_SLOT_WORDS` |
| Faces torn: a typed load of 16-bit integers after a group barrier returned a value from before another wave's store | Those loads read the slot's raw view | `VICTORY_RAW_TYPED_LOADS` |
| Everything sinks in water | Textures that shaders write are copied back into the title's memory | `VICTORY_TEXTURE_BACK` |
| A read failing with `0x800703e6` mid-run | DirectStorage's writes take a lease on guarded pages (from upstream) | |
| A controller turned on after the start is "disconnected"; several pads | One pad that is always there | `VICTORY_ONE_PAD` |
| Seconds-long stalls when a new scene's pipelines are made | Pipelines earlier runs used are made ahead | `VICTORY_PIPELINE_WARM` |

Speed switches, all **off by default**, measured together at 19 to 30% less frame
time on this machine (same save, same script, back to back): `VICTORY_GUARD_BITS=1
VICTORY_COPY_PLAIN=1 VICTORY_LAND_WINDOW=1 VICTORY_CARD_STEPS=2
VICTORY_LOOKUP_CACHE=1`. Several are ports from
[fxceless/victory-fork](https://github.com/fxceless/victory-fork) and
[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX), named
where they are in the code.

## What has NOT been tested

**Nothing here has been run on Windows, on an NVIDIA or Intel card, or on any other
AMD card.** Today most of the fixes above apply on every system, not only under
Wine. Until that is changed (next), treat this tree as Linux-on-RADV only: on
another system it may be slower or wrong where upstream is right.

## Where this is going

1. Each Proton fix made to apply only where it is needed (under Wine, or behind a
   switch that is off by default), so that the same code cannot change anything for
   Windows users, and each hardened with tests.
2. Each offered upstream on its own, as a small change with its evidence.
3. Kept current with upstream; this fork should shrink as fixes land there.

Reports from other Linux machines are welcome in the issues: card, driver and Mesa
version, kernel, Proton and vkd3d-proton versions, the commit, how far it got, and
the kernel's lines if the device was lost (`journalctl -k | grep amdgpu`).

## Building and running

[docs/proton.md](docs/proton.md): requirements, the build (`cargo xwin`), the
patched Proton and how to build it, running, the diagnostics and the checking tools.

## Lineage and credit

[Marvisak/VIctory](https://github.com/Marvisak/VIctory) →
[fxceless/victory-fork](https://github.com/fxceless/victory-fork) →
myokex/victory-amd (the section below is its README) → this. Commits keep their
authors. Much of the investigation and code in this fork was done with an AI coding
assistant (Claude Code) directed and tested by the owner on the machine above; the
measurements are from that machine's logs.

GPL-3.0, as upstream.

---

# The fork this one started from

## victory-amd

This copy is a fork of
[fxceless/victory-fork](https://github.com/fxceless/victory-fork), itself a fork
of [VIctory](https://github.com/Marvisak/VIctory). It adds fixes found while
running on an AMD card, tested on one machine only: Windows 11, Radeon RX 7800 XT
16 GB (driver 32.0.32029.2010), Ryzen 5 7500F, 32 GB RAM.

State on that machine (2026-11-25, Series X profile): the prologue's cutscene and
the first room are drawn, with the player moving, at 2 to 3 frames per second.
Runs of 8 minutes mostly end without a device loss; one ended with
`DXGI_ERROR_DEVICE_HUNG` after about 5 and a half minutes, cause unknown. Lamps
were dark in one run. The first room's walls were missing in several runs; with the
guard over all the views of a page (below) they were there in two runs out of two,
too few to call it fixed. Some shots of the opening cutscene come out white.
Ray-traced light does not run.

What it changes over `victory-fork` (each is explained in a comment where it is):

- Texture views are told apart by the memory their mips really take, so textures
  streamed in part are not taken for one another and are uploaded from their first
  mip that is there.
- Views of placed textures the host lost sight of are put back
  (`VICTORY_TEXTURE_SWEEP`).
- Dispatches of more than 65,535 groups are made in two parts (the final gather at
  2560x1440 asks for 115,200).
- DirectStorage requests read their page array when they run, and wait for it.
- The list of pipelines to make early keeps what earlier runs listed, and compute
  pipelines get their own waiting time while a world loads.
- A write of the title's through one view of a physical page is seen by the copies
  taken through its other views (`VICTORY_VIEW_GUARD=0` turns it off).
- Textures picked by index are looked at again after their upload
  (`VICTORY_HEAP_RECHECK=0` turns it off).
- View indices past the host heap are clamped on AMD too.
- Programs that stop the AMD driver's compiler, or take it minutes and tens of GB,
  are left out (ray-tracing resolves).
- Diagnostics: `VICTORY_NULL_MAGENTA`, `victory warm`, more captures.

The guard over all the views of a page is code of
[victory-fork #115](https://github.com/fxceless/victory-fork/pull/115) by Spexivus
with [its correction](https://github.com/Spexivus/victory-fork/pull/1) by joroyt and
a part of [VIctory #1122](https://github.com/Marvisak/VIctory/pull/1122) by tightfall,
adapted here. The second look at textures is fxceless's idea (victory-fork `a803c87`),
written anew. Other parts were prompted by pull requests and issues of VIctory and
victory-fork, and their code was written anew. Developed with [Claude Code](https://claude.com/claude-code).

A work-in-progress fork of [VIctory](https://github.com/Marvisak/VIctory), a
compatibility layer that runs an Xbox Series title natively on Windows from
files the user supplies. This fork follows upstream
`main` and adds fixes aimed at one goal: getting the game's world to render
correctly and playably on one particular PC.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork. Upstream's history is kept intact below
  the single commit that carries this fork's changes.
- **[XCL](https://github.com/ivall/xcl)** by ivall and its contributors
  (GPL-2.0-only) was used as a reference for how the same title behaves under
  another layer. Fixes inspired by it were written anew in VIctory's code.
- This fork's changes were developed with [Claude Code](https://claude.com/claude-code).

## Status of this fork (2026-11-25)

**Not playable yet: 2.5 to 4 frames per second, and shadowed areas are black.**
Tested on one machine: Windows 11, Intel i7-14700KF, NVIDIA RTX 4070 12 GB,
32 GB RAM. The Series X profile is used throughout.

| Stage | State in this fork |
|-------|--------------------|
| Boot, menus, start screen | works, 30 fps (as upstream) |
| Opening cutscene | plays from start to finish, in order, lit: title cards, credits cards, night and dusk shots |
| Saves | the game autosaves a checkpoint and loads it again |
| First gameplay scene (the prologue) | renders: characters, textures, sky, sunlit exterior; HUD, pause menu and pad input work |
| Light in shadow | missing: the game's ray-traced bounce light and ray-traced lamp and sun shadows do not run yet, so anything the sun does not hit directly is black |
| Frame rate in the world | 2-4 fps: 420-450 ms per frame by default, 240-350 ms with the newer switches (`VICTORY_RELAY=2`, or `VICTORY_LATE_WAIT=3 VICTORY_WIDE_BACK=1 VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_CARD_BUFFERS_MB=1024`); it started at 3-6 seconds per frame |
| Stability | no driver hang or device loss in normal runs (a hang introduced by the wave64 update was found and fixed: its barrier form for 64-lane programs is now off unless `VICTORY_WAVE_WIDE=1`) |

### What this fork changed (summary)

Rendering:
- Meshlet culling read its scene buffers as the wrong kind of view, so the world drew almost nothing; fixed.
- Per-draw descriptor tables could run out, after which draws used slots never made for them and the card hung; fixed.
- Light packed by the game's compute shaders was read back through empty copies (sky, probes, fog); fixed for every mip level and layer.
- The fog reconstruction, shadow-mask, hair, blend-shape skinning, fire and several material programs did not translate; fixed.
- Gathers at a named mip level (with and without an offset) read level 0; fixed.
- Implicit-level samples outside pixel shaders produced invalid DXIL in 730 compute programs; fixed.
- Rectangle lists, depth and stencil twins for depth textures written by compute, texture read-backs.
- Ray tracing, in progress and off by default: the game's own structure-builder programs translate, title memory can be read by raw address from shaders, the hardware ray/box and ray/triangle test is emulated in DXIL (checked against a reference on the GPU), and 59 of the 64 trace programs translate. Nothing is lit by it yet. See [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md).

Speed and memory:
- Waits for memory values, write-watch queries and page queries no longer walk page tables per call.
- Large title buffers get a copy in card memory, with shader writes found by hashing on the card.
- Submissions can be sent ahead of the wait for the card (`VICTORY_LATE_WAIT`), with a relay thread that lands results as the card finishes (`VICTORY_RELAY`): in a first five-minute run the card's idle time behind the submitting thread fell from about 136 to 30 ms per frame.
- Title memory can be opened as a graphics heap so that buffers are used in place instead of copied (`VICTORY_INPLACE`, first stage only; a probe confirmed the approach on the test GPU).
- The bindless texture queue no longer uploads stale views; the view table no longer grows without bound; the pipeline library is mapped instead of read into memory.
- An intermittent device loss was traced to the pipeline library file being replaced while mapped; fixed.

- Translated shaders get clean-up passes over the finished module (dead code, constant folding, reuse of values, branch facts); the pack's host instruction count falls by about 43 %, worth about 20 ms of GPU time per frame on the test machine (`VICTORY_SHADER_OPT=0` turns them off).

Most of the newer speed changes sit behind environment switches that are off
by default (`VICTORY_LATE_WAIT`, `VICTORY_RELAY`, `VICTORY_WIDE_BACK`,
`VICTORY_CARD_BUFFERS_MIN_MB`, `VICTORY_INPLACE`, `VICTORY_BVH_BUILDER`, ...).
Each is described in a comment where it is read.

### Known problems

- Everything in shadow is black (see above).
- The interior of the first gameplay room is sometimes not drawn after the cutscene hands over to gameplay.
- Around 24 GB of memory is in use during play.
- A few style lints are switched off for this fork in the workspace `Cargo.toml` until the newer code is tidied.
- Many changes here are specific to this title and this GPU, and some are marked `shortcut:` in the code where they approximate what the console does.

Everything below this line is upstream VIctory's own README, unchanged; its
status section describes upstream, not this fork.

---

# VIctory

Runs the title (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | – |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | – |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

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

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
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
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`), in every build |
| `VICTORY_CAPTURE_EVERY=<k>` | with the above, every k-th frame from the world's first on, listed in `frames.tsv`; compare two runs with `tools/shots.py` |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_PIPELINE_WAIT_MS=24` | reduce the per-frame wait for background pipeline creation from the default of 500 ms. A draw can be omitted until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
