#!/usr/bin/env python3
"""How soon do the characters' textures reach the card? From a title run's log.

usage: face_time.py <log> [<log> ...]

The run needs VICTORY_CAPTURE_EVERY=1 (with VICTORY_CAPTURE=<a fresh folder>) or
VICTORY_CAPTURE_WORLD=1, so that the log says which frame is the world's first:

    the first frame of 500 draws or more: the world's frames are counted from it frame=N

Every frame in which the title's character shaders (the ones that take their textures' heap
slots from a material record, `State::note_asked` in crates/victory-gpu/src/draw.rs) drew
logs one line, in any build:

    character textures the frame's draws asked for, and of them missing on the card frame=F asked=A missing=M missing_long=L every_slot=true

M counts the distinct slots those draws named whose host slot was null (nothing on the card);
L those of them missing at every ask for 30 frames or more (never arriving). Builds that write
every_slot=true count every slot the pixel program may take a texture from (the material
record's both parts, the global record, the small buffers and textures of the draw's tables);
older builds counted only the first 16 words of the material record, and a run they called
whole (fast.log, 2026-11-25: median 0 missing) had no heads in the diner shots.
In the opening cutscene the main character's head is drawn with three such maps (1024x1024
BC5 and BC7, 512x512 BC7) that were null for tens of frames while the card was full
(2026-11-25, runs D and E); a frame with M = 0 drew every character texture it asked for.

Printed per log: the world's first frame and time; the first frame after it with M = 0 and the
first from which M stays 0 for 3 frames in a row, each as frames and seconds after the world's
first; and the median and largest M over the world's frames. Compare a run with
VICTORY_TEXTURE_STARVED=0 against one without it.
"""

import re
import statistics
import sys
from datetime import datetime

LINE = re.compile(r'^(\S+)\s.*character textures the frame\'s draws asked for.*frame=(\d+) asked=(\d+) missing=(\d+)'
                  r'(?: missing_long=(\d+))?(?: every_slot=(\w+))?')
WORLD = re.compile(r'^(\S+)\s.*the first frame of 500 draws or more.*frame=(\d+)')
ANSI = re.compile(r'\x1b\[[0-9;]*m')


def stamp(text):
    # 2026-11-25T08:21:26.420853Z
    return datetime.strptime(text[:26], '%Y-%m-%dT%H:%M:%S.%f')


def one(path):
    world = None
    frames = []
    with open(path, errors='replace') as f:
        for line in f:
            line = ANSI.sub('', line)
            if world is None:
                m = WORLD.search(line)
                if m:
                    world = (int(m.group(2)), stamp(m.group(1)))
                    continue
            m = LINE.search(line)
            if m:
                frames.append((int(m.group(2)), stamp(m.group(1)), int(m.group(3)), int(m.group(4)),
                               int(m.group(5)) if m.group(5) else None, m.group(6)))
    print(path)
    if world is None:
        print('  no world line: set VICTORY_CAPTURE_EVERY=1 or VICTORY_CAPTURE_WORLD=1')
        return
    w_frame, w_at = world
    after = [f for f in frames if f[0] >= w_frame]
    print(f'  world from frame {w_frame}; {len(after)} world frames with character draws')
    if not after:
        return
    rel = lambda f: f'frame {f[0]} (+{f[0] - w_frame} frames, +{(f[1] - w_at).total_seconds():.0f} s)'
    first = next((f for f in after if f[3] == 0), None)
    print('  first with none missing:', rel(first) if first else 'never')
    steady = None
    for i in range(len(after) - 2):
        if all(f[3] == 0 for f in after[i:i + 3]):
            steady = after[i]
            break
    print('  none missing from then on for 3 frames:', rel(steady) if steady else 'never')
    missing = [f[3] for f in after]
    print(f'  missing per frame: median {statistics.median(missing)}, largest {max(missing)}; '
          f'asked per frame: median {statistics.median(f[2] for f in after)}')
    if any(f[5] != 'true' for f in after):
        print('  WARNING: an older build: only the first 16 words of the material record were counted, '
              'so 0 missing does not mean the characters are whole')
    long = [f[4] for f in after if f[4] is not None]
    if long:
        bad = sum(1 for n in long if n > 0)
        print(f'  missing 30 frames or more: in {bad} of {len(long)} frames, largest {max(long)}')


for p in sys.argv[1:]:
    one(p)
