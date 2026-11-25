# Building and running under Proton on Linux

How this layer is built and run on Linux, with Wine and vkd3d-proton on a Vulkan
driver (RADV, AMD RX 9070), what broke there, and how it was fixed. The state is
that of 2026-11-24. It was found on one machine.

Paths here are generic. `<data>` is a data directory outside the repository (it
holds the game files, the cache, the Wine prefix and the scripts). Nothing from
the game belongs in the repository (see `asset-policy.md`).

## What works and what does not

The title boots, reaches the world and renders part of it. It runs at about 0.3 to
0.5 frames a second. **It is not playable.**

- Ray tracing is off on purpose. Most of the draws a diagnostics build reports as
  dropped are ray-tracing passes. Ignore those.
- Frames are slow, mostly because of the cost of tracking the title's memory
  under Wine. A late world frame took 2 to 3 s while the GPU needed about 0.2 s,
  and the process used under 2 s of CPU in a 3.3 s frame: it mostly waits.
  This comes from a read-only study of the code, not from a measurement (see
  Known open problems).
- Some textures arrive late.
- Some pipelines are still not made (see Known open problems).

## Requirements

- **Proton Experimental**, not GE-Proton11-12. GE-Proton11-12's vkd3d-proton
  (g8293130eb) turns some of the title's translated shaders into invalid SPIR-V,
  and RADV 26.2.6 then dies on a null pointer: the process exits with code 3 at
  the first world pipeline. Proton Experimental's (g2576c0b4a, 2026-11-12) converts
  them correctly. Checked 2026-11-24 with pixel shader c0a7fe77b1862e10 and compute
  shader 4e296b1885c37b29.
- **Microsoft's `d3dcompiler_47.dll` in the prefix's `system32`**, with the DLL
  override `d3dcompiler_47=n`. The layer compiles helper shaders at run time with
  `D3DCompile`. Wine's own `d3dcompiler_47` refuses three of them (the indirect
  argument clamp, the depth-plane twin, the compare on the card). The world's
  indirect draws are then dropped and the screen is mostly black. The DLL was
  copied from a Steam game; Wine's own stays beside it as
  `d3dcompiler_47.dll.wine-builtin`. A new prefix needs the copy again.
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
draw was dropped.

The shader crate's unit tests run natively and take seconds:

    cargo test -p victory-shader --lib

`victory-gpu` and `victory-hle` are Windows-only and cannot be unit-tested on the
host. `proton-test.sh` in the data directory builds every test binary of the
workspace with `cargo xwin test --workspace --no-run`, writes a batch file that
runs them, and runs it under Proton (with a GE-Proton prefix of its own, not the
one the game uses). Results go to a log directory in the data directory.

## Running

The launcher `proton-run-experimental.sh` is a sh script in the data directory:

    ./proton-run-experimental.sh run 'Z:\path\to\game_scarlett_master.exe' --execute

Paths in the arguments are Windows paths (`Z:` is the Linux root). Its settings,
each with its reason:

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
  gets past the start screen without a pad. It needs a build with commit "Input:
  VICTORY_KEYS works in every build" (before it, only diagnostics builds read it).
  Set `VICTORY_KEYS=` (empty) to turn it off.
- `VICTORY_EXE`: a Linux path of another build.
- Output: Proton does not relay a console program's output, so the launcher runs
  the program under `cmd /c` with output redirected to `last-run.log` and prints
  it afterwards.

### Several jobs on one machine

When several people or agents share one machine, one prefix and one GPU, two Proton
jobs at once corrupt each other. `tools/vx.sh` (kept in the data directory) takes a
lock, runs one job and writes a log of its own:

    vx.sh [-s] [-t seconds] [-i idle-seconds] <victory.exe> <log> <victory arguments...>

- Jobs run in an off-screen KWin (`offscreen.sh`) with sound sent to a null sink.
  `-s` (on the real screen) counts only with `VX_ALLOW_SCREEN=1`; `VX_AUDIO=1`
  keeps the sound.
- `-t` kills the job after that many seconds (default 600; exit status 124).
- `-i` calls the job hung when `victory.exe` has used no CPU time for that many
  seconds, and kills it (exit status 97). A worker thread that Wine ended leaves
  the process waiting for ever, and this is how that shows.
- The log has the colour codes removed. `<log>.proton` has Proton's output.
- `run` gets the scripted A presses unless `VICTORY_KEYS` is set.
- `VICTORY_*` variables pass through.

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
  these as "worker deaths".
- Commit 6b1d840: the cause. The converter's recursion was in
  `CFGNode::find_common_dominator` and `recompute_immediate_dominator`, on a loop
  with two back edges (a `continue`). DXC writes loops with one latch only, so
  every loop now gets one (`Module::one_latch`): the blocks that branched back
  branch to a new block that joins what each brought and branches back alone. The
  program does the same either way. With one latch the three convert and their
  pipelines are made. `VICTORY_ONE_LATCH=0` turns it off; the shader cache keys on
  the switch.

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
texture's, as nothing says what it is.

This one is not Proton-specific. It was found while looking at dropped draws in
Proton runs.

### Translator additions (commit c93f5b7)

`v_cvt_f32_f64` and `v_add_f64` sums, the first half of an interpolation read as
data, and `v_permlane16`/`v_permlanex16`. Ported from victory-fork's 3bff182 and
31714de. 12 more programs of the pack translate. Also not Proton-specific.

## Checking tools

These live in `tools/` of the data directory, not in the repository. All launches go
through `vx.sh`.

- `scan.sh <victory.exe>`: translates every program of the shader pack, off-screen,
  in about 3 minutes. The numbers are exact (the same build gives the same). It
  prints how many programs translate per stage and the failures by kind. Baseline
  at c93f5b7: 14199 of 14247 vertex and pixel, 2674 of 2682 compute, 8762 of 8765
  mesh. (The comment in `scan.sh` still gives the figures of fa37224: 14191, 2670,
  8762.)
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

  Judge it by its log only. Scenes differ between runs by wall-clock time, so
  differences in frame time under about 30% between two single runs are noise.
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

I have not run these tools for this guide, so there are no run figures here beyond
the scan baseline above.

## Known open problems

- Frame time. The working account, from a read-only code study at c93f5b7 and not
  yet measured: the title's memory is tracked with Wine's write watches
  (`GetWriteWatch`) and page guards. A late diagnostics frame showed 66.5
  submissions, 3038 watch calls costing 1762 ms (guard time included), and about
  2 s between a present and the first command list call. The same range is asked
  about again and again, and nothing is kept between submissions. The statistics
  are misleading: "checked" counts answers from the journal too, so 15.9 GB checked
  is not bytes scanned by the kernel. The notes (`tools/notes-write-watch.md`) list
  ranked suggestions, none tried, and unexplained time (what the title does in
  those 2 s). Tried without gain: `VICTORY_RELAY=2`, `VICTORY_WIDE_BACK=1`,
  `VICTORY_CARD_BUFFERS_MIN_MB=1`, `VICTORY_CARD_BUFFERS_MB=1024`,
  `VICTORY_LATE_WAIT_LISTS=256`.
- Shaders that do not translate: at c93f5b7, 48 vertex and pixel, 8 compute and 3
  mesh programs of the pack.
- Ray tracing is off.
- Some textures arrive late.
- vkd3d-proton's converter may fail on other programs the same way. The worker
  watch makes that cost one pipeline, not a lost worker, but the pipeline is then
  missing for the run.
