#!/usr/bin/env python3
"""Did the picture get better or worse? Compares the frames two title runs saved.

usage: shots.py [--frames] <label>=<capture dir> <label>=<capture dir>
       shots.py <capture dir>             (one run's statistics only)

Producing the captures (any build; the folder is made if missing, and must be a fresh one per
run, as a Windows path):

    VICTORY_CAPTURE='Z:\\tmp\\victory-shots\\<run>' VICTORY_CAPTURE_EVERY=1 \\
      tools/vx.sh -s -t 180 <victory.exe> <log> run 'Z:\\home\\beqren\\victory-data\\game\\game_scarlett_master.exe' --execute

VICTORY_CAPTURE_EVERY=k saves every k-th presented frame from the world's first frame on (the
first frame of 500 draws or more); a save costs 70-80 ms of a 2.5-3 s world frame, about 3%
(frames.tsv, column ms). Keep k=1: a 3-minute run has only 25-31 world frames. The folder gets
frame-<n>.png (1920x1080 of the 4K picture) and frames.tsv (frame, frame after the world's
first, seconds since the first present, seconds since the world's first frame, draws, ms).

What is compared, and why. The opening cutscene runs on the title's clock, not on frame count,
and a frame takes 2.5-3 s, so frame N after the world's first is not the same picture from run
to run: in two runs of one build (2026-11-25) frames 1-12 and 25-31 showed different shots, and
the world began 82 s and 90 s after the first present. Pooled over all frames the two runs also
differed a lot (median mean luma 14.0 against 19.3, colours 852 against 2437), from which shots
each happened to sample. What does line up is the shot: frames of the same shot, found by
thumbnail within 10 s on the world's clock, gave the same statistics in both runs (mean luma
19.8/19.9, black 25%/27%). So shots.py pairs frames of the same shot and judges by the median
of the pairs' differences. With fewer than MIN_PAIRS pairs (a picture changed so much that its shots no
longer match, as when 77 pixel shaders were skipped) it compares the means over all frames,
with the wider margins of POOLED_NOISE. Empty frames are counted over all frames either way.
--frames prints every frame's statistics.

Statistics, of each frame's luma (Rec. 709 weights on the 8-bit values), the cutscene's black
letterbox bars cropped:
  black         share of pixels with luma < 2 (undrawn geometry on a cleared target, unlit
                areas; the night scene has 10-40% of it)
  centre black  the same in the middle ninth of the frame (where the cutscene's faces are)
  mean luma     0-255 (lost lighting)
  edges         share of pixels whose luma steps by more than 4 to a neighbour (detail)
  flat blocks   share of 8x8 blocks, not black, whose luma varies by less than 0.5 (flat
                colour: missing textures)
  colours       distinct colours at 5 bits a channel (noisy: police lights change it)
  white         share of pixels with luma > 250 (blown out: a broken exposure pass)
  empty frames  share of frames of one flat colour (luma spread under 2: black or white whole)
Checked 2026-11-25 on four runs: two diagnostics runs and one release run of one build (every
one of the six orderings: SAME), and a diagnostics run with 77 of the world's pixel shaders
skipped (VICTORY_SKIP_PIPELINES; the picture went white or dark: WORSE against each of the
three). Also on copies of a run with damage painted in: a black middle ninth, a black box of
1/36, every third frame black, the middle quarter flattened to 8x8 blocks, 60% of the light:
each WORSE against all three clean runs. The margins were set on these same runs; a SAME
between two builds means no difference larger than three runs of one build showed, not none.
"""
import os
import struct
import sys
import zlib

import numpy as np

# A median of paired differences counts only above these. Three runs of one build
# (2026-11-25), matched shots, every ordering: black up to 4.2 points (a release run against a
# diagnostics run), mean luma 1.1, edges 0.1 point, flat 0.1 point, colours up to 476.
NOISE = {
    "black": 0.06,
    "centre_black": 0.05,
    "mean_luma": 2.0,
    "edges": 0.005,
    "flat": 0.02,
    "colours": 800,
    "white": 0.02,
    "blank": 0.07,
}
# Thresholds on 8-bit luma. The opening cutscene is at night: half its frames have a median luma
# under 8 and a 90th percentile under 40, so the usual "black < 16, edge > 20" saw nothing.
BLACK = 2
EDGE = 4
FLAT = 0.5
# Matching shots between runs: frames within WINDOW seconds of each other on the world's clock
# whose thumbnails differ by less than SAME (mean squared difference of normalized luma).
WINDOW = 10.0
SAME = 0.3
MIN_PAIRS = 6
# With too few matched shots, the means over all frames, and these margins: the means of the two
# runs of one build differed by black 38.1%/22.8%, centre black 17.3%/7.5%, mean luma 13.2/19.4,
# edges 3.4%/3.9%, colours 1193/1920, empty frames 0%/4.2% (and white and flat by nothing).
# shortcut: one pair of runs; more same-build pairs would make these margins better founded.
POOLED_NOISE = {
    "black": 0.20,
    "centre_black": 0.15,
    "mean_luma": 10.0,
    "edges": 0.010,
    "flat": 0.02,
    "colours": 1200,
    "white": 0.03,
    "blank": 0.10,
}
# Which way is better for each statistic (+1: higher is better).
# Mean luma has no better side: lost lighting lowers it, a broken exposure pass raises it (with 77
# pixel shaders skipped the cutscene went white and its median luma rose from 12 to 110). Its
# change is reported, and decides nothing.
BETTER = {"black": -1, "centre_black": -1, "white": -1, "mean_luma": 0, "edges": +1, "flat": -1, "colours": +1, "blank": -1}
LABELS = {
    "black": "black (luma<2)",
    "centre_black": "centre black",
    "mean_luma": "mean luma",
    "edges": "edges",
    "flat": "flat blocks",
    "colours": "colours (5-bit)",
    "white": "white (luma>250)",
    "blank": "empty frames",
}


def read_png(path):
    """8-bit RGB or RGBA PNG to an array (Pillow if there, else zlib: the layer writes filter 0)."""
    try:
        from PIL import Image

        return np.asarray(Image.open(path).convert("RGB"))
    except ImportError:
        pass
    data = open(path, "rb").read()
    pos, idat, w = 8, b"", None
    while pos < len(data):
        n, kind = struct.unpack(">I4s", data[pos : pos + 8])
        body = data[pos + 8 : pos + 8 + n]
        if kind == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", body[:10])
            ch = {2: 3, 6: 4}[ctype]
            assert depth == 8
        elif kind == b"IDAT":
            idat += body
        pos += 12 + n
    raw = np.frombuffer(zlib.decompress(idat), np.uint8).reshape(h, w * ch + 1)
    out = np.zeros((h, w * ch), np.int16)
    for y in range(h):
        f, row = raw[y, 0], raw[y, 1:].astype(np.int16)
        prev = out[y - 1] if y else np.zeros(w * ch, np.int16)
        if f == 0:
            out[y] = row
        elif f == 2:
            out[y] = (row + prev) & 255
        else:  # filters 1, 3, 4 need a per-pixel loop; the layer never writes them
            raise ValueError(f"{path}: PNG filter {f} needs Pillow")
    return out.astype(np.uint8).reshape(h, w, ch)[:, :, :3]


def crop_letterbox(rgb):
    """Without the cutscene's black bars: all-zero rows at the top and bottom, up to 12% each."""
    lit_rows = rgb.max(axis=(1, 2)) > 0
    h = len(lit_rows)
    top = 0
    while top < h * 0.12 and not lit_rows[top]:
        top += 1
    bottom = h
    while bottom > h * 0.88 and not lit_rows[bottom - 1]:
        bottom -= 1
    return rgb[top:bottom]


def frame_stats(rgb):
    rgb = crop_letterbox(rgb).astype(np.float32)
    y = rgb @ np.array([0.2126, 0.7152, 0.0722], np.float32)
    h, w = y.shape
    black = y < BLACK
    centre = black[h // 3 : 2 * h // 3, w // 3 : 2 * w // 3]
    gx = np.abs(np.diff(y, axis=1))[:-1, :]
    gy = np.abs(np.diff(y, axis=0))[:, :-1]
    edges = np.maximum(gx, gy) > EDGE
    bh, bw = h // 8, w // 8
    blocks = y[: bh * 8, : bw * 8].reshape(bh, 8, bw, 8).swapaxes(1, 2).reshape(bh, bw, 64)
    lit = blocks.mean(axis=2) >= BLACK
    flat = (blocks.std(axis=2) < FLAT) & lit
    q = rgb.astype(np.uint32) >> 3
    colours = len(np.unique((q[..., 0] << 10) | (q[..., 1] << 5) | q[..., 2]))
    return {
        "black": float(black.mean()),
        "centre_black": float(centre.mean()),
        "mean_luma": float(y.mean()),
        "edges": float(edges.mean()),
        "flat": float(flat.sum() / max(lit.sum(), 1)),
        "colours": colours,
        "white": float((y > 250).mean()),
        "blank": float(y.std() < 2),
    }


def run_frames(folder, until=float("inf")):
    """The world frames a run saved up to `until` seconds into the world: (row of frames.tsv as
    a dict, path)."""
    rows = []
    with open(os.path.join(folder, "frames.tsv")) as f:
        head = f.readline().split()
        for line in f:
            r = dict(zip(head, line.split()))
            if int(r["after_world"]) >= 1 and float(r["secs_world"]) <= until:
                rows.append((r, os.path.join(folder, f"frame-{r['frame']}.png")))
    return rows


def thumb(rgb):
    """32x18 luma, normalized to zero mean and unit spread: which shot a frame shows, whatever
    its brightness. None for a frame without contrast (a black or flat frame)."""
    y = crop_letterbox(rgb).astype(np.float32) @ np.array([0.2126, 0.7152, 0.0722], np.float32)
    h, w = y.shape
    t = y[: h // 18 * 18, : w // 32 * 32].reshape(18, h // 18, 32, w // 32).mean(axis=(1, 3))
    sd = t.std()
    return None if sd < 0.5 else (t - t.mean()) / sd


def load(folder, until, show=False, label=""):
    """Each world frame up to `until`: its frames.tsv row, statistics and thumbnail."""
    out = []
    for r, path in run_frames(folder, until):
        rgb = read_png(path)
        s = frame_stats(rgb)
        out.append((r, s, thumb(rgb)))
        if show:
            print(
                f"{label:>8} +{int(r['after_world']):3d} {float(r['secs_world']):6.1f}s {int(r['draws']):5d} draws "
                + " ".join(f"{k}={v:.3f}" if isinstance(v, float) else f"{k}={v}" for k, v in s.items())
            )
    return out


def pooled(frames):
    """Mean, lower quartile, median and upper quartile of each statistic over a run's frames."""
    out = {}
    for k in BETTER:
        v = np.array([s[k] for _, s, _ in frames], float)
        out[k] = (float(v.mean()), *(float(np.percentile(v, q)) for q in (25, 50, 75)))
    return out


def matched(a, b):
    """Pairs of frames showing the same shot: for each frame of `a`, the frame of `b` within
    WINDOW seconds (of the world's clock) whose thumbnail is nearest, if nearer than SAME."""
    pairs = []
    for ra, sa, ta in a:
        best = None
        for rb, sb, tb in b:
            if ta is None or tb is None or abs(float(ra["secs_world"]) - float(rb["secs_world"])) > WINDOW:
                continue
            d = float(np.mean((ta - tb) ** 2))
            if best is None or d < best[0]:
                best = (d, sb)
        if best and best[0] < SAME:
            pairs.append((sa, best[1]))
    return pairs


def fmt(k, v):
    if k == "colours":
        return f"{v:.0f}"
    if k == "mean_luma":
        return f"{v:.1f}"
    return f"{v * 100:.1f}%"


def judge(changes, la, lb, how, noise=NOISE):
    """changes: (statistic, value of a, value of b) for each statistic; the verdict line. When
    statistics disagree, detail (edges) decides which way it leans: every failure seen here
    (undrawn geometry, flat textures, lost light, a blown-out picture) lowered it."""
    better, worse, changed = [], [], []
    edges = 0
    for k, va, vb in changes:
        if abs(vb - va) <= noise[k]:
            continue
        text = f"{LABELS[k]} {fmt(k, va)}->{fmt(k, vb)}"
        if BETTER[k] == 0:
            changed.append(text)
        elif (vb - va) * BETTER[k] > 0:
            better.append(text)
        else:
            worse.append(text)
        if k == "edges":
            edges = 1 if vb > va else -1
    also = f"; changed {', '.join(changed)}" if changed else ""
    if better and not worse:
        return f"{lb} BETTER than {la} ({how}): " + ", ".join(better) + also
    if worse and not better:
        return f"{lb} WORSE than {la} ({how}): " + ", ".join(worse) + also
    if worse:
        lean = {1: ", leaning BETTER (more detail)", -1: ", leaning WORSE (less detail)", 0: ""}[edges]
        return f"{lb} MIXED against {la}{lean} ({how}): better " + ", ".join(better) + "; worse " + ", ".join(worse) + also
    if changed:
        return f"{lb} CHANGED against {la} ({how}), neither better nor worse by these: " + ", ".join(changed)
    return f"{lb} SAME as {la} ({how}: every difference within the noise of runs of one build)"


def main(argv):
    show = "--frames" in argv
    args = [a for a in argv if not a.startswith("--")]
    if not args or len(args) > 2:
        print(__doc__)
        return 2
    runs = [(a.split("=", 1) if "=" in a else (os.path.basename(a.rstrip("/")), a)) for a in args]
    # The same stretch of the cutscene from both: up to the shorter run's last world frame.
    ends = [max([float(r["secs_world"]) for r, _ in run_frames(folder)] or [-1]) for _, folder in runs]
    # No world frame: either the switch was not set, or the run never drew a frame of 500 draws
    # (most draws dropped), the worst failure there is.
    empty = [label for (label, _), end in zip(runs, ends) if end < 0]
    if len(empty) == len(runs):
        print("verdict: cannot tell (no world frames saved: was VICTORY_CAPTURE_EVERY set?)")
        return 1
    if empty:
        (la, _), (lb, _) = runs
        why = "never reached a frame of 500 draws or more; no world frames saved"
        if empty[0] == lb:
            print(f"verdict: {lb} WORSE than {la} ({lb} {why}; check that VICTORY_CAPTURE_EVERY was set)")
        else:
            print(f"verdict: {lb} BETTER than {la} ({la} {why}; check that VICTORY_CAPTURE_EVERY was set)")
        return 0
    until = min(ends)
    frames = [(label, load(folder, until, show, label)) for label, folder in runs]
    print(f"world frames up to {until:.0f} s after the world's first")
    print(f"{'all frames: mean (quartiles)':32}" + "".join(f"{label:>26}" for label, _ in frames))
    print(f"{'world frames':32}" + "".join(f"{len(f):>26}" for _, f in frames))
    ms = [float(np.median([float(r["ms"]) for r, _, _ in f])) for _, f in frames]
    print(f"{'ms a save (median)':32}" + "".join(f"{m:>26.1f}" for m in ms))
    stats = [pooled(f) for _, f in frames]
    for k in BETTER:
        cells = [fmt(k, d[k][0]) if k == "blank" else f"{fmt(k, d[k][0])} ({fmt(k, d[k][1])}-{fmt(k, d[k][3])})" for d in stats]
        print(f"{LABELS[k]:32}" + "".join(f"{c:>26}" for c in cells))
    if len(frames) < 2:
        return 0
    (la, a), (lb, b) = frames
    pairs = matched(a, b)
    print(f"\nsame shot in both: {len(pairs)} of {len(a)} frames of {la} matched")
    # Of matched pairs: the median of each side, and the median of the pairs' differences. The
    # verdict goes by the differences: the medians of the two sides can land on different shots
    # (a release and a diagnostics run of one build: medians of black 11.9% and 21.8%, paired
    # differences a median of +1.3 points).
    paired = {}
    if len(pairs) >= MIN_PAIRS:
        print(f"{'':32}{la:>26}{lb:>26}{'paired difference':>26}")
        for k in (k for k in BETTER if k != "blank"):
            va = float(np.median([p[0][k] for p in pairs]))
            vb = float(np.median([p[1][k] for p in pairs]))
            paired[k] = float(np.median([p[1][k] - p[0][k] for p in pairs]))
            sign = "+" if paired[k] >= 0 else "-"
            print(f"{LABELS[k]:32}{fmt(k, va):>26}{fmt(k, vb):>26}{sign + fmt(k, abs(paired[k])):>26}")
    if min(len(a), len(b)) < 10:
        print(f"verdict: cannot tell (fewer than 10 world frames in a run: {len(a)}, {len(b)})")
    elif len(pairs) >= MIN_PAIRS:
        changes = []
        for k in (k for k in BETTER if k != "blank"):
            va = float(np.median([p[0][k] for p in pairs]))
            changes.append((k, va, va + paired[k]))
        # A black frame matches no shot: their share is of all frames.
        changes.append(("blank", stats[0]["blank"][0], stats[1]["blank"][0]))
        print("verdict: " + judge(changes, la, lb, f"{len(pairs)} matched shots"))
    else:
        # Too few shots in common: the picture changed too much to match, or the runs fell on
        # different shots. All frames, with wider margins.
        changes = [(k, stats[0][k][0], stats[1][k][0]) for k in BETTER]
        print("verdict: " + judge(changes, la, lb, f"means of all frames; only {len(pairs)} shots matched", POOLED_NOISE))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
