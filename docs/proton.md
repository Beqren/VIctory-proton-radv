# Building and running under Proton on Linux

How this layer is built and run on Linux, with Wine and vkd3d-proton on a Vulkan
driver (RADV, AMD RX 9070), what broke there, and how it was fixed. The state is
that of 2026-11-25. Everything measured here was measured on one machine.

Paths here are generic. `<data>` is a data directory outside the repository (it
holds the game files, the cache, the Wine prefixes and the scripts). Nothing from
the game belongs in the repository (see `asset-policy.md`); only shader hashes and
counts appear here.

## What works and what does not

The title boots, reaches the world and renders part of it. On the stock Proton a
world frame took 2 to 3 s (0.3 to 0.5 frames a second); on the Proton with the
patched wineserver (below) it took about 1 s in the first runs and, with the later
fixes, 350 to 400 ms (2.5 to 3 frames a second: medians of 349, 373 and 401 ms in
three runs of six to ten minutes, measured on one machine). **It is not playable.**
No run has been tested past ten minutes.

- Ray tracing is off on purpose (see "Ray tracing"). Most of the draws a
  diagnostics build reports as dropped are ray-tracing passes. Ignore those.
- Every vertex, pixel and mesh program of the pack translates (14,247 of 14,247
  and 8,765 of 8,765); 2,677 of 2,682 compute programs do (scan at a22290d).
- Some textures still arrive late while the card is full.
- Two failures were seen once each and are open (see Known open problems).

## Requirements

- **Proton Experimental**, not GE-Proton11-12. GE-Proton11-12's vkd3d-proton
  (g8293130eb) turns some of the title's translated shaders into invalid SPIR-V,
  and RADV 26.2.6 then dies on a null pointer: the process exits with code 3 at
  the first world pipeline. Proton Experimental's (g2576c0b4a, 2026-11-12) converts
  them correctly. Checked 2026-11-24 with pixel shader c0a7fe77b1862e10 and compute
  shader 4e296b1885c37b29.
- **For speed, a copy of Proton Experimental with a patched wineserver** (see "The
  patched Proton"). Stock Proton works, but its frames are about three times slower.
- **Microsoft's `d3dcompiler_47.dll` in the prefix's `system32`**, with the DLL
  override `d3dcompiler_47=n`. The layer compiles helper shaders at run time with
  `D3DCompile`. Wine's own `d3dcompiler_47` refuses three of them (the indirect
  argument clamp, the depth-plane twin, the compare on the card). The world's
  indirect draws are then dropped and the screen is mostly black. The DLL was
  copied from a Steam game; Wine's own stays beside it as
  `d3dcompiler_47.dll.wine-builtin`. A new prefix needs the copy again (the rebuild
  script does it).
- **cargo-xwin** for the Windows build, with a Rust toolchain that has the
  `x86_64-pc-windows-msvc` target. `<data>/toolchain/env.sh` sets `CARGO_HOME`,
  `RUSTUP_HOME` and `PATH` for the toolchain kept in the data directory.
- A launcher: umu-run (Lutris bundles one).

## Building

Cross-build for Windows from Linux:

    . <data>/toolchain/env.sh
    cargo xwin build --release -p victory --target x86_64-pc-windows-msvc

The binary is `target/x86_64-pc-windows-msvc/release/victory.exe`. Seed a fresh
worktree's `target/` from an existing one to skip a long first build.

A diagnostics build (`--profile diag --features diagnostics`, output under
`.../diag/`) logs much more, including per-frame timers (`copystat`) and why each
draw was dropped. Frame capture (below) and the keys switch work in every build.

The shader crate's unit tests run natively and take seconds:

    cargo test -p victory-shader --lib

`victory-gpu` and `victory-hle` are Windows-only and cannot be unit-tested on the
host. `proton-test.sh` in the data directory builds every test binary of the
workspace with `cargo xwin test --workspace --no-run`, writes a batch file that
runs them, and runs it under Proton (with a GE-Proton prefix of its own, not the
one the game uses). Results go to a log directory in the data directory.

## Running

Every launch goes through `tools/vx.sh` (below); do not call umu-run or Proton
directly when several jobs share a machine. Two launchers for a person are in the
data directory:

- `play.sh`: the merged build on the real screen, with sound, no time limit, on
  stock Proton Experimental and its prefix.
- `play-viewtree.sh`: the same on the patched Proton and its own prefix. `test`
  as an argument starts the newest unmerged test build. It takes its turn behind
  any automated job.

Both press A on the start screen for the first 90 seconds and write the title's
output to a log in the data directory. The older `proton-run-experimental.sh`
starts one job without the queue:

    ./proton-run-experimental.sh run 'Z:\path\to\game_scarlett_master.exe' --execute

Paths in the arguments are Windows paths (`Z:` is the Linux root). The settings
that every launch makes, each with its reason:

- `WINEPREFIX`: a prefix of its own for this layer, with the `d3dcompiler_47.dll`
  described above.
- `PROTONPATH`: Proton Experimental (see Requirements). `GAMEID=0` because umu-run
  needs one and this is not a Steam game.
- `WINEDLLOVERRIDES=d3dcompiler_47=n`: use Microsoft's DLL.
- `VICTORY_CACHE_DIR`, `VICTORY_DATA_DIR`: Windows paths of the converted-data
  cache and the game data.
- `VKD3D_SHADER_CACHE_PATH`: vkd3d-proton writes `vkd3d-proton.cache` into the
  working directory unless told where.
- `VICTORY_KEYS`: presses Enter (the pad's A) every 2 s for the first 90 s, which
  gets past the start screen without a pad. It works in every build (commit
  689166c). Set `VICTORY_KEYS=` (empty) to turn it off.
- `VICTORY_EXE`: a Linux path of another build.
- Output: Proton does not relay a console program's output, so the launcher runs
  the program under `cmd /c` with output redirected to `last-run.log` and prints
  it afterwards.

### Several jobs on one machine: `tools/vx.sh`

When several people or agents share one machine, one prefix and one GPU, two Proton
jobs at once corrupt each other. `tools/vx.sh` (kept in the data directory) takes a
lock (`<data>/.proton.lock`), runs one job and writes a log of its own:

    vx.sh [-s] [-t seconds] [-i idle-seconds] <victory.exe> <log> <victory arguments...>

- Jobs queue in front of the lock. A job of 120 seconds or less goes first, in
  order of arrival; then the long ones, in order of arrival; a job that has waited
  ten minutes goes ahead of everything that came after it. (Plain lock waiting sat
  14 minutes behind a string of four-minute title runs on 2026-11-25.) A job also
  waits while any `victory.exe` of anybody's is still running.
- Jobs run in an off-screen KWin (`offscreen.sh`) with sound sent to a null sink.
  `-s` (on the real screen) counts only with `VX_ALLOW_SCREEN=1`; `VX_AUDIO=1`
  keeps the sound; `VX_FIRST=1` puts the job first in the queue (a person starting
  it himself).
- `VX_PROTON` and `VX_PREFIX` pick another Proton and another prefix (default:
  Valve's installed Proton Experimental and `<data>/prefix-experimental`). Use
  both together for the patched Proton:

      VX_PROTON=<compatibilitytools.d>/Proton-Experimental-viewtree VX_PREFIX=<data>/prefix-viewtree \
        tools/vx.sh -t 60 <victory.exe> <log> config

- `-t` kills the job after that many seconds (default 600; exit status 124).
- `-i` calls the job hung when `victory.exe` has used no CPU time for that many
  seconds, and kills it (exit status 97). A worker thread that Wine ended leaves
  the process waiting for ever, and this is how that shows.
- The log has the colour codes removed. `<log>.proton` has Proton's output.
- `run` gets the scripted A presses unless `VICTORY_KEYS` is set.
- `VICTORY_*` variables pass through.

## The patched Proton

### Why

A world frame was slow because wineserver keeps a process's memory views in a
linked list. The title holds about 125,000 views (123,000 to 135,000 in diagnostics
runs), and every page remap the runtime makes goes through two walks of that list
in the single-threaded server: `map_view` calls `is_valid_view_addr`, which walks
every view, and `unmap_view` calls `find_mapped_view`, which walks from the head.
The runtime remaps 1,000 to 4,000 single 64 KiB pages a frame. Each remap is
Wine view calls that cost more the more views are alive, and ntdll holds its
process-wide `virtual_mutex` across the server request, so every VirtualProtect,
VirtualQuery and GetWriteWatch on every thread waits behind it. A bench
(`viewbench2`) of one page remapped as a view cost 23.6 us with 100 views alive and
552 us with 125,000 (measured on one machine).

The fix is a published wineserver patch (Wine bug 60868; CachyOS/wine-cachyos pull
request 50, commit fd66dbca8eb0754839ca974e45b5ea446ca62d2f). It touches only
`server/file.h`, `server/mapping.c`, `server/process.c` and `server/process.h`: an
rb-tree keyed by view base, kept beside the list, so each view call is O(log n).
Only `files/bin/wineserver` of Proton differs.

Measured on one machine (`<data>/wine-viewtree/NOTES.md`):

| views alive | stock, one page remapped | patched |
|---:|---:|---:|
| 100 | 63.8 us (noisy; 14-24 us in earlier benches) | 9.3 us |
| 10,000 | 113.6 us | 11.5 us |
| 50,000 | 249.8 us | 12.0 us |
| 125,000 | 520.9 us | 12.2 us |

Title runs of 240 s, median world frame: 2,862 and 3,206 ms on stock; 982 ms on a
patched run, 708 ms in the owner's patched play, and 324 ms in the first 30 s of
the world of a second patched run (not representative). With the patch texture
refreshes fell from about 1.2 s a frame to 7 to 21 ms (they no longer wait for the
mapping lock), the world was reached 33 to 37 s after the start instead of about
86 s, and the wineserver used about 7 ticks a second instead of about 79. The
patched runs dropped fewer draws, not more. Upstream reports a world frame of 3.6 s
going to 1.1 s. One run of each; the runs were single and not repeated.

### What it is

- A copy of Valve's Proton Experimental (reflink copy) in
  `<compatibilitytools.d>/Proton-Experimental-viewtree`, with the patched server as
  `files/bin/wineserver` and Valve's as `files/bin/wineserver.stock`. It has no
  `compatibilitytool.vdf`, so Steam does not list it: it is for umu, `vx.sh` and
  `play-viewtree.sh`.
- Its own prefix, `<data>/prefix-viewtree`, created by Proton on the first run,
  with Microsoft's `d3dcompiler_47.dll` copied in.
- Built from Wine's source at the commit Valve's tag names, shallowly fetched, with
  the patch applied; only `server/wineserver` is built. The binary's dynamic imports
  are the stock server's (libc only), so it runs in umu's container as it is.

### Keeping it current: `tools/proton-viewtree-rebuild.sh`

The copy stays at the Proton Experimental it was made from until it is rebuilt. Its
wineserver must match its own `ntdll.so` exactly: **never put this wineserver into
a newer Proton.** The script `tools/proton-viewtree-rebuild.sh` (a copy of
`<data>/wine-viewtree/rebuild.sh`, with the paths in variables at its top) does the
whole rebuild from Valve's installed Proton Experimental:

1. Reads the `version` file for the tag and finds that tag's `wine` submodule commit
   with the GitHub API.
2. Fetches only that commit into a fresh source directory, checks the patch's sha256
   and applies it. If the patch does not apply cleanly it stops and says so: someone
   has to port the patch.
3. Runs Wine's own generation steps (`make_vulkan`, `make_specfiles`,
   `make_requests`, `autoreconf`), configures, and builds only `server/wineserver`
   (about 2 minutes).
4. Checks the generated protocol version against the installed `ntdll.so` (Valve's
   committed header says 930 and lacks the fsync requests; `make_requests`
   regenerates it as 931, and the stock `ntdll.so` compares against `$0x3a3`). It
   stops if the version is not found in the `ntdll.so`, because then the tree is not
   the one Proton was built from.
5. Stages a new copy of Proton, then, holding the Proton lock (`flock -n` on
   `<data>/.proton.lock`; it fails with a message if a job holds it) replaces the old
   copy. A failed build or a held lock leaves the old copy in place.
6. Makes sure the prefix has Microsoft's `d3dcompiler_47.dll` (copied from the
   other prefix), and prints what it built and from which commits.

It never touches Valve's install and does not use sudo. It is safe to run twice.
If Wine merges bug 60868 or Valve takes the patch, drop all this and use stock.
After a rebuild, the view bench in `<data>/wine-viewtree/` shows that a remap stays
flat as views grow.

## Problems found under Proton, and their fixes

Each entry gives what was seen, then the cause.

### Exit code 3 at the first world pipeline

GE-Proton11-12's vkd3d-proton makes invalid SPIR-V of some translated shaders and
RADV dies on a null pointer. Not fixed in this layer: use Proton Experimental.

### A mostly black screen, `Method 'InterlockedMax' is not defined`

Wine's `d3dcompiler_47` refuses three of the layer's own helper shaders, so the
world's indirect draws are dropped. Fixed by Microsoft's DLL and the override
(see Requirements). The `InterlockedMax` method is used by the indirect argument
clamp (`crates/victory-gpu/src/indirect.rs`).

### Pipelines stuck "being made", and the silent stack overflow

Symptom: the per-frame line says "N being made" and N never goes down; some compute
pipelines are never made, and a run can go on with no CPU use.

Cause: vkd3d-proton's DXIL converter recursed without end on three compute
programs (559d9f0184e15fe3, a1b20bc47e3c9597, e8c962358d862e3f), seen on Proton
Experimental of 2026-11-12 and GE-Proton11-14. A 64 MiB stack overflowed as the
2 MiB one did. On Windows a fault in pipeline creation ends the process. Under
Wine a stack overflow inside the host call ends only the thread, without a word,
and the pipeline stayed "being made" with a worker lost for each.

Two fixes:

- Commit a575f88: pipeline workers are watched. One that ends while making a
  pipeline is seen, its pipeline is given up (its draws or dispatches are dropped,
  and it is not tried again in the run) and a new worker takes its place. The log
  says "a pipeline worker ended while making a host pipeline". `runstat.py` counts
  these as "worker deaths". A later change (d1d1a9e) makes a wait for a pipeline
  never join a worker that is making it, does not make a queued pipeline known to
  end workers, and remembers what ended a worker by its whole set of shaders.
- Commit 6b1d840: the cause. The converter's recursion was in
  `CFGNode::find_common_dominator` and `recompute_immediate_dominator`, on a loop
  with two back edges (a `continue`); in a synthetic shader it needs a wave
  shuffle (`WaveReadLaneAt`) in the same program as well, and the same shader
  without the shuffle converts. DXC writes loops with one latch only, so
  every loop now gets one (`Module::one_latch`): the blocks that branched back
  branch to a new block that joins what each brought and branches back alone. The
  program does the same either way. With one latch the three convert and their
  pipelines are made. `VICTORY_ONE_LATCH=0` turns it off; the shader cache keys on
  the switch.

### A mesh pipeline refused for blending on an integer target

The mesh pipeline [mesh f5e50eb2108c425b, pixel ce3f0432a08f1d35] blends an
`R8_UINT` render target with MAX, and vkd3d-proton refused it with `E_INVALIDARG` in
every run. Fixed in commit 4f51ca6: host pipelines do not blend on integer targets.
A refused mesh pipeline's warning now names its state.

### "dstorage: request failed ... hr=0x800703e6" in the middle of a run

Symptom: a run ended at the title's own breakpoint after a DirectStorage request
failed with `ERROR_NOACCESS`. It happened one start in three within two seconds in
one batch of runs, and three minutes into the owner's play on the patched Proton.
The patched wineserver reaches it sooner because the world is reached sooner; it
also happened on stock Proton.

Cause: a race. Every such failure was a raw request (options 0x0) of 1 to 45 KB: a
file read by the kernel straight into title memory. The kernel's write does not
fault into the layer's page guard handler, so the layer opened the guarded blocks
first (`kernel_will_write`), but a copy on another thread could put the guard back
before the read landed, and the read failed.

Fix, commits bda311b, 37e360d and 7669a65: requests into a title address are read
into a buffer of the layer's and written in from user mode (`write_title_memory`:
a block guarded meanwhile faults, is opened and noted as written). The write holds
the reading lock so that no remapping leaves a gap, checks that each page is
committed and writable or guarded by the layer, and is tried again 12 times
(`VICTORY_DSTORAGE_RETRIES`) before the request fails. `fread` into title memory
reads beside in 1 MiB chunks the same way. After review, every view of the written
pages is noted as written after the write. `VICTORY_READ_BESIDE=0` reads in place,
as before. A test under Proton shows `0x800703e6` read in place and `S_OK` read
beside.

### The character's black face

Symptom: the main character's head, face and neck were drawn black or as the null
placeholder in the opening's close-ups, for tens of frames.

Cause, from the slot listing (`VICTORY_LIST_SLOTS`): the head's 1024x1024 BC5 and
BC7 maps and a 512x512 BC7 map were in the queue of textures picked by index, due
since frame 835 among about 9,000 due slots, and were still null at frame 866. While
the card was at nine tenths of its budget the pass admitted nothing new and gave up
after 256 slots; the card then stood at 97% for minutes. A second cause was the
stale rule: under the 4 KiB rule 42,974 slots were made stale in one run and the
face was black again, with no view made stale it was drawn.

Fixes, helped by the faster frames of the patched wineserver:

- Commit 20efa1a (`VICTORY_TEXTURE_STARVED=0` turns this and the next off): on a
  full card the pass serves the slots written longest ago first, and slots due 8
  frames or more take a reserve band up to 96% of the budget
  (`VICTORY_TEXTURE_RESERVE_PERCENT`), at most 64 MB a pass.
- Commit 083b06c: the draws of the character shaders note the slots their material
  record names, and on a full card those come first and may be made up to 99% of
  the budget, 32 MB a pass (`VICTORY_TEXTURE_ASKED=0` turns it off).
- The stale rule is the head rule (a newer view makes an older one stale where
  their spans overlap and the older texture's first 4 KiB, its smallest mips,
  changed since its view was noted). Under the overlap rule, the default before,
  the title's small textures packed where a newer streamed texture's large mips
  would be were made stale and null while draws still read them: of the null slots
  the character programs asked for, 51,280 slot-frames of 53,500 were stale (run
  room-3); under the head rule none, slots made stale fell from 68,820 to 708, and
  the diner shots' faces, clothes and posters are textured (run room-4).
  `VICTORY_STALE_TEXTURES=overlap` or `=start` (the 4 KiB rule) select the others,
  `=0` makes none stale. `VICTORY_STALE_LIST=1` lists stale slots read later.
- Eviction of host textures by their last read (`VICTORY_TEXTURE_EVICT=1`) is off:
  the host sees no reads of 6.2 of the 6.4 GB of textures slots show (programs
  pick them by index), and what it can see is read again within seconds.
- `tools/face_time.py` measures the result: from the per-frame log line of character
  textures asked for and missing on the card, it prints the frames and seconds after
  the world's first frame until none is missing.

### "a descriptor read only as data"

Symptom: shader does not translate, with that reason. Forty programs of the pack
stopped there, among them three pixel programs that scale their coordinates by the
size of a texture picked by index before they sample two others (for example
6b6db9f1a526daeb at 0x1c5c).

Cause: the translator refused a whole image descriptor (8 dwords) that no
instruction samples or loads through, because the code reads only its size out of
it (width, height and base level are in dwords 1 to 3).

Fix, commit fa37224: the size comes from the host, as it does for a descriptor that
is sampled. 35 pixel programs of the pack translated afterwards, and three of them
had dropped about 10 draws a frame. Shortcut: such a descriptor is taken for a 2D
texture's, as nothing says what it is. Commit 94676fc does the same for a buffer
descriptor loaded only for its address or record count.

This one is not Proton-specific. It was found while looking at dropped draws in
Proton runs.

### Other translator additions

Not Proton-specific either: `v_cvt_f32_f64` and `v_add_f64` sums, the first half of
an interpolation read as data, and `v_permlane16`/`v_permlanex16` (c93f5b7, 12
programs); a sampler picked between immediate samplers by a join (cd0ca14, 40
programs); primitive-ordered pixel programs, with their heap UAVs as
rasterizer-ordered views (e3fc61c, ten programs); a lane mask copied a dword at a
time (f8016e0); first halves of interpolations evaluated as pairs only in a program
refused for one, with the second translation made in `emit` so that the offline
tools get it too (8dfd60d, a22290d). With these the scan reaches the counts above.

Ports from upstream (branch d16-pack, 2026-11-25): 16-bit typed buffer data through
integer views passes as bits (after 4243a96: the 63 compute_graph_ops programs, the
face network; the host's table by heap slot carries the view's format kind), the same
packing for tbuffer d16 loads (c430b1d; no program of the pack has one), and
normalized buffer codes converted from integers (da82e44; on this card the host's
division gave 16-bit code 257 back as 256, a test under Proton showed it). In the
title neither changed the pictures visibly. The waitress's red face (and red streaks
on the heroine's neck) is intermittent in every build tried: absent in one run with
the d16 fixes, present in one with d16 and the normalized fix, absent in one with both
plus the quad change; none of these is shown to affect it. The eyes still glow: their
lighting program reads no d16 data. Unverified: the integer path relies on vkd3d-proton
passing UINT bits through the float-typed handle (upstream checked Windows AMD and WARP).
The face network (branch face-anim, 2026-11-25). The 65 `compute_graph_ops`
programs (a tensor library: MatMul, VecMatMul, Silu, Gather, Concat, Formula ...,
all wave64, all bound through descriptor tables, their tensors in R16_UINT and
R16G16B16A16_UINT views) run every frame (368 to 2,095 dispatches recorded a
frame). Their output, the R16_UINT buffer the head extract 970c4fbb3b017c0e reads
(`buffer_load_format_x`, low half taken as an f16 by `v_fma_mix_f32`), was all
zero in every frame: a table's copy in dynamic slots never got its word in the
table by slot, so the 16-bit integer path of 1adb4e0 never ran for them and the
integers became halves, then 0. With the words written (`VICTORY_TABLE_SLOT_WORDS=1`,
off by default) the output is alive: 180,000-270,000 of 532,480 halves not zero,
none NaN, none over 4, changing every frame; the halves the extract reads are
plausible-looking triples (x, y, z about -0.005, 0.023, -0.036, smooth from
vertex to vertex). Faces then move but are torn: mouths agape, jaw and lips
pulled out of shape, as in upstream's own build (captures upstream-5,
frame-853). The eyes stop glowing (the rig drives the eye meshes too): the
lavender-white eyes are this zero output, not lighting.

Ruled out with tests and runs: integer bits through float-typed handles (GPU
tests under Proton: they arrive); f16 rounding (RADV rounds `v_add_f16` to
nearest, wave64 test); DirectStorage page lists (the hold after upstream eddde73,
`VICTORY_DSTORAGE_HOLD`, on: its tests pass, the formula program's buffer hashes
the same with and without, faces the same); dropped or resubmitted lists (none);
pipeline syncs (the title never calls them); ordering between dispatches
(`VICTORY_DISPATCH_BARRIER=1`, a UAV barrier after every dispatch: faces the
same; the title records about 710-760 barrier groups a frame in the world).
Not yet checked: each op of the network against a CPU reference on real data
(the probe, `VICTORY_GRAPH_FRAMES`, hashes and decodes every view of every
dispatch in a window of frames), the mapping pass's delta indices (an index off
by one gives each vertex a neighbour's delta of the right size), and what the
network's inputs (face controls) hold.

Checked offline on real data (branch face-ref, 2026-11-25; dump run with
`VICTORY_GRAPH_DUMP=1020,1,<folder>`, which copies the network's 24 MB arena on
the card before each of its dispatches, every other view at first sight, and the
extract's, mapping's and skinning's views before and after; scripts and dumps in
/tmp/victory-faceref):

- The deltas land on the right vertices. For each of the six heads the extract
  ran in frame 1020, map, base positions (24/24/16-bit packed, mode 0) and the
  network output as the extract read them: the deltas as indexed (`3 * (map >>
  16) + constant word 15`) form a smooth field (median distance to the mean of
  the 8 nearest vertices' deltas 0.06-0.21 of the median delta), every
  re-indexing is rougher (delta index -1/+1: 0.6-1.3; a half off: 0.19-0.72;
  4 or 6 halves a vertex: 1.2-5.3), and the field is anatomical: zero on the
  skull, largest on lips and jaw, brows, eyelids. So the network computes the
  wrong expression; the indexing is not it. Head seq 55: jaw and lips moved 5 cm.
- Ops matched against CPU models of their console code, dispatch by dispatch:
  VecMatMulAligned (40 dispatches), AddBroadcastAligned, Silu, MatMulAligned,
  and the copies (Gather, SliceCopy, ConcatAligned, Zero) write values of their
  inputs where expected. Every result is the exact value rounded to 16 bits, but
  rounded toward zero: all 6,400 results of an AddBroadcastAligned dispatch equal
  the toward-zero rounding of the exact sum, 41% of them a last place below the
  nearest; matrix products came out 0.6% small (fitted gain 0.9937, the error
  correlated -0.92 with the value). The host's `f32tof16` rounds toward zero on
  RADV as on NVIDIA; the programs set MODE's f16 rounding to nearest. Fixed
  (6bb83c5): every 16-bit float result is rounded to nearest even
  (`f16_nearest`; `VICTORY_F16_NEAREST=0` gives the host's conversion back),
  packed `v_pk_*` results included, which the switch never reached.
- That was not the cause: with it (run confirm1, captures
  /tmp/victory-faceref/cap1, `VICTORY_TABLE_SLOT_WORDS=1`) mouths are still
  agape and teeth float under the nose (frame 938); eyes are normal. Not
  checked: everything after the first ~225 dispatches of the network (frame
  1020's recording began mid-network and the skinning's 32 MB views used the
  copy room in frame 1021): Formula (the face controls, its output is read
  only after it is overwritten), Gather/GatherAligned with 32-bit indices made of
  two halves, TransposeVecMatMulAligned (a wave64 f16 sum the translator lowers
  in single precision, `half_sum_here`), SmallMatMul, Scatter, Place, Mult, Add.
  Next: a dump window that starts at a frame's first network dispatch, without
  the skinning's copies, to check those.

Second round (same branch; dump by network pass, `VICTORY_GRAPH_DUMP=1020,1,<dir>`,
`VICTORY_GRAPH_DUMP_LIGHT`; run dump3 for pass 0, 851 dispatches, 4 heads; run
watch for who writes the controls):

- Every op after Formula matches a CPU model of its console code, pass 0: Gather
  and GatherAligned (32-bit indices from two halves: exact), Scatter (exact),
  Place/PlaceAdd (aligned or not), Mult, Add, SmallMatMul (aligned or not),
  TransposeVecMatMulAligned (the `v_readlane 32` wave sum and its single-precision
  shortcut: within 0.07%), and the five final Gathers into the output buffer
  (exact, all 5 x 15,462 halves). Not modelled: SliceCopy, Concat(Aligned), one
  VecMatMul.
- The controls come in right. The Formula's input (three buffers in turn, 27 KB)
  holds the same bytes on the card as in title memory at each pass. Of its 43
  blocks of 160 halves, the 4 of the heads in view change every frame, smoothly,
  as speech (values 0-0.75). The other blocks stay the same per buffer, and the
  title writes nothing there (run watch: no copies, no other dispatch touches
  them, only 21-24 of 429 pieces change between presents). They are probably
  characters not animated. So the bug is not upstream of the network.
- Formula (0aa7934094794dbf, the face controls) is the one op that disagrees. It
  is an interpreter: 1,024 threads, 12 stages with a barrier between them,
  registers in the arena (an R16 view), and ops copy, add, sub, mul, div, abs,
  neg, min, max, sin, cos, tan and sqrt. A CPU model of its console code
  reproduces the register file's set-up exactly and stages 0-2 exactly. From
  stage 3 on it differs: 108 of the 440 controls the next op (Clamp) reads, and
  148 registers. The first disagreement is a stage-3 read that saw a register's
  stage-0 value where another wave's stage-2 write should have landed. That
  fits a missed barrier, or a stale vector cache across the WGP. Making UAVs
  globally coherent for such programs (`VICTORY_COHERENT_UAVS=1`, 926cc9b, off
  by default) left the faces torn (run confirm2, captures
  /tmp/victory-faceref/cap2, frame 948). Either the CPU model is wrong somewhere,
  or the cause is not cache coherence (the barrier itself, or an instruction in
  the interpreter's decode).
- Next: replay Formula alone on the GPU, from the dumped program, input and
  arena, with the stage count in its header (half 43) cut to 1, 2, 3... That
  gives the card's register file after each stage, to compare with the model.
  Then check the barrier's lowering, and dxil-spirv's semantics for typed UAVs.

Quad reads through quad reads in pixel programs (upstream 4015a18, the same lowering
as 8487caf; branch d16-pack-quad) made skin plastic and glossy again: not merged.

## What was tried for the frame time

Frames were slow because of the view list (see "The patched Proton"). Before that
was found, the measured breakdown of a 3.6 s world frame on stock Proton was:
preparing submissions 3,096 ms (texture refreshes 1,634, of which 1,481 waiting
for the title-memory mapping lock; buffer-copy refreshes 1,109, of which 1,018 in
write-watch and guard checks), waiting for the GPU 164 ms, copying back what shaders
wrote 314 ms. The submitting thread ran only about 255 ms of CPU in the frame: it
waited. wineserver ran at one core, 85 to 90 ticks a second.

Tried on stock Proton and what came of it (`<data>/notes/`):

- Title memory's mapping lock by range (`VICTORY_MAP_RANGES=1`, e49433f): reads'
  wait fell from a median of 202 ms a frame to 0, all of it false contention, but
  the median world frame stayed at 2.4 s because the frame waits on the remaps
  themselves.
- The GPU's page mappings as private copies, not Wine views
  (`VICTORY_ALIAS_COPY=1`, dd30398): 1,830 ms a frame down to 840 ms (2.2x), views
  alive 122-129k down to 83-86k. Not correct: 34 to 622 copies a frame differed from
  their page. It stays off. It showed what removing the views is worth.
- Joining page views (`VICTORY_VIEW_JOIN=1`, 3d694f4): views 123-126k down to
  98-100k, each remap memory call 0.57-0.80 down to 0.43-0.57 ms, but the GPU's
  mappings' time a page did not fall. Not a demonstrated win: off.
- Leaving page mappings undone that change nothing (`VICTORY_REMAP_SAME`): 0 of
  2,000 to 3,700 pages a frame were already mapped as asked. Gained nothing.
- Known ranges in view memory go to the guards without a failing `GetWriteWatch`
  call (`VICTORY_VIEWS_KNOWN`, on by default): failed calls fell from 4,536-7,852 a
  frame to 536-687; frame time unchanged within noise.
- Page marks (`VICTORY_PAGE_MARKS=1`, off by default; port of upstream cd94d35 and
  940d32f, branch `page-marks`): translated shaders mark the 64 KiB pages they write
  through heap views, and copies in the PC's memory are compared only there. Two check
  runs (`VICTORY_PAGE_MARKS=check`) found 0 unmarked pieces that differed, out of about
  160 to 210 differing pieces judged a second. Every program of the pack still
  translates, and its pipeline builds with marks on. The gain depends on how much is
  compared on the CPU, and that depends on which copies get a card side, which varies
  from run to run. The only run with a full card was a run without marks: 720 MB
  compared a frame, in 40 to 54 ms. In the marks runs every copy of 1 MiB or more
  was on the card, and 13 MB were compared against about 27 MB without marks.
  Snapshots are still taken. Upstream drops them, but that needs a save on the
  title's first write.
- Also without gain: `VICTORY_RELAY=2`, `VICTORY_WIDE_BACK=1`,
  `VICTORY_CARD_BUFFERS_MIN_MB=1`, `VICTORY_CARD_BUFFERS_MB=1024`,
  `VICTORY_LATE_WAIT_LISTS=256`.
- Mapping memory behind Wine's back (memfd and `mmap` through raw Linux system calls,
  for a true alias that costs no Wine view) is not possible. Wine 11 turns on
  syscall user dispatch for every thread: a raw `syscall` instruction in the layer's
  code outside glibc is trapped and served as an NT system call (getpid returned
  0xC0000005, memfd_create 0xC000001C, which are NT status codes). The ways round
  it depend on Wine internals and work around the interception; the answer was the
  server patch above.
- `winbench` (`tools/winbench`) times the Windows memory calls the write tracking
  uses, alone and with four contending threads. Contention is what multiplies
  costs: a write-watch reset that finds pages 10x, a guard fault 9x,
  `VirtualProtect` on a 256 MiB view 38x. `VirtualProtect` on views costs about 19x
  more a page than on private memory. These are inferences from one machine, not
  traced.

## Ray tracing

It stays off. The builder programs (52) all translate and get host pipelines, and
28 of the 33 trace programs dumped for the study translate; the 1,000 per-material
resolve programs translate too. What is missing is correctness: with
`VICTORY_BVH_BUILDER=1` the title crashes within seconds on pointers that have
become bounding-box bit patterns (`0x7c007c00fc00fc00`, `0x7f800000ff800000`, the
empty-box values), so the builder's stores land in the title's own objects. Cause
not proven; the likely one is memory reached through pointers read out of memory,
which is not made ready (a documented shortcut). With the quarantine switch the
process ran on silent until the time limit. Five shadowmask trace programs do not
translate (`emit: a descriptor read only as data`), and the cost of the address
path is unmeasured. The ordered work to first lit shadows is in
`<data>/notes/ray-tracing-plan.md`: estimated at 3 to 5 weeks of one person, most of
the uncertainty in finding where the builder's stores land.

TuxBox (another project) was tried on this card and renders black, and it is not a
way over: its translator refuses every program with the console's node instruction,
and the only exceptions are programs of a different title matched by hash. Its own
builder is not used for this title. In a traced run every batched build carried a
console extension that TuxBox refuses, and host structures do not fit the
console's buffers (host TLAS sizes were larger). What transfers is the point to
intercept builder calls, for a guard rail and for make-ready extents
(`<data>/notes/ray-tracing-tuxbox.md`).

## Capture

`VICTORY_CAPTURE=<folder>` saves presented frames in every build.
`VICTORY_CAPTURE_EVERY=<k>` saves every k-th frame from the world's first frame on
(the first frame of 500 draws or more); a save costs 70 to 80 ms of a 2.5 to 3 s
world frame. The folder holds `frame-<n>.png` (1920x1080) and `frames.tsv`. Use a
fresh folder a run, as a Windows path.

- `tools/shots.py`: compares the frames two runs saved. Frames of the same shot are
  paired by thumbnail, because the opening cutscene runs on the title's clock and
  frame N is not the same picture from run to run; it judges by the median of the
  pairs' differences. A run without world frames is the worse one.
- `sheet.py` (in the data directory's `tools/`): a contact sheet of one run's world frames, labelled with the
  frame and the seconds since the world's first frame (needs Pillow).
- `tools/face_time.py`: see "The character's black face".

## Checking tools

`vx.sh`, `scan.sh`, `pipes.sh`, `time1.sh`, `offscreen.sh`, `runstat.py` and
`sheet.py` live in `tools/` of the data directory; `shots.py`, `face_time.py`, the
rebuild script and `winbench` are in the repository's `tools/`. All launches go
through `vx.sh`.

- `scan.sh <victory.exe>`: translates every program of the shader pack, off-screen,
  in about 3 minutes. The numbers are exact (the same build gives the same). It
  prints how many programs translate per stage and the failures by kind. Baseline at
  a22290d (2026-11-25): 14,247 of 14,247 vertex and pixel, 2,677 of 2,682 compute,
  8,765 of 8,765 mesh.
- `pipes.sh <victory.exe> <.cdxil file or directory>...`: for each shader, does it
  translate and does the host make a pipeline of it. About 5 s a file. Prints `OK`,
  `HUNG` (idle for 10 s) or `FAIL` with what the tool said. Corpora in the data
  directory: `repro-stack-overflow` (3 compute programs, need one latch a loop),
  `repro-untranslated` (3 pixel programs that read a texture's size, 1 mesh and 1
  pixel program), `repro-faildump`.
- `time1.sh <seconds> <.cdxil>...`: times translation plus pipeline creation for each
  file alone, and reports a hang after `IDLE` idle seconds (default 10). It is run
  through `offscreen.sh` and does not take the lock itself.
- `offscreen.sh <command>`: runs a command inside a KWin on a virtual output with
  its own D-Bus session, so no window reaches the real screen.
- A full title run, 3 minutes:

      vx.sh -t 180 <victory.exe> <log> run 'Z:\path\to\game_scarlett_master.exe' --execute

  It runs off-screen and muted. Judge it by its log only. Scenes differ between runs
  by wall-clock time, so differences in frame time under about 30% between two single
  runs are noise.
- `runstat.py <label>=<log> ...`: a table to compare logs. What its rows mean:
  - frames presented: the last `frame presented` number.
  - ms/frame, draws/frame, dropped (no pipeline), dropped (indirect): medians over
    the world frames only (those with 100 or more draws a frame). Draws dropped for
    ray tracing are among them.
  - being made (last): pipelines still being made at the last sample.
  - compute shaders bound: distinct compute shaders the title bound. Compared with
    compute pipelines made, it shows what never got a pipeline.
  - compute, mesh and vertex pipelines made: counts of the log lines.
  - shaders not translating, pipelines refused, worker deaths, errors: counts of the
    log lines (`shader does not translate`, `pipeline refused`, `pipeline worker
    ended`, ` ERROR ` or `panicked`).
- `upstream-check.sh`: three `git ls-remote` calls; prints a `NEW` line for each of
  the other VIctory repositories (victory-amd, victory-fork, VIctory) that moved
  since the last look. `--ack` records the heads as seen.

## Known open problems

- A GPU ring timeout, seen once: the kernel's amdgpu log said "ring gfx timeout" and
  the device was removed, after a submission of compute pipelines that took 2 s. The
  cause is not known.
- An access violation in the title, seen once. The cause is not known.
- Nothing has been tested past ten minutes.
- The patched Proton goes stale whenever Valve updates Proton Experimental: run
  `tools/proton-viewtree-rebuild.sh`.
- Some textures still arrive late while the card is full. The black face is helped,
  not proven fixed.
- Ray tracing is off (see "Ray tracing").
- Shaders that do not translate: 5 of the pack's 2,682 compute programs.
- Copying back what shaders wrote into title memory
  took 0.3 to 2.4 s a frame on stock Proton and nothing splits that time; it was not
  measured on the patched one.
- vkd3d-proton's converter may fail on other programs the same way. The worker watch
  makes that cost one pipeline, not a lost worker, but the pipeline is then missing
  for the run.
- Whether the DirectStorage failure still happens after the fix was not measured in a
  long run.
