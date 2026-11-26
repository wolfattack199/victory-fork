# VIctory fork (work in progress)

A work-in-progress fork of [VIctory](https://github.com/Marvisak/VIctory), a
compatibility layer that runs an Xbox Series title natively on Windows from
files the user supplies. This fork follows upstream
`main` and adds fixes aimed at one goal: getting the game's world to render
correctly and playably on one particular PC.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

## This copy (wolfattack199/victory-fork)

This is a copy of [fxceless/victory-fork](https://github.com/fxceless/victory-fork)
(at `0b0074e`) with the changes below on top. Its aim is correct shaders and
graphics on an NVIDIA RTX 5060 Ti (16 GB) first, speed after that.

| | State (2026-11-26) |
|---|---|
| Shader and pipeline caches | limited to 2 GB in total; they grew without a limit before. The entries not used for the longest go first. `victory cache` shows what they take, `victory cache --clear` empties them, `VICTORY_CACHE_MAX_MB` changes the limit |
| Checks | 862 tests pass, 1 fails, 15 are ignored (2026-11-26, morning; the graphics crate has one test more since, 286 of 286 in most runs). The one that fails (`same_target_conditional_branch_keeps_both_edge_blocks`) failed before these updates too. Some tests of the graphics crate fail in some runs and not in others, a different one each time: in 14 runs of the crate this morning, 3 had a failure and one stopped without a result, as noted before (`memory_that_is_no_view_is_not_written_behind`, `warp_a_fetch_through_the_copy_queue_brings_what_the_direct_queue_brings`, and in a run without this morning's last change `waits_end_on_ticks_and_count_frames` with `written_pages_are_reported_to_every_asker`); each passes run again. They share tables of views and sections with tests running beside them, which is the likely cause; not mended yet. `warp_a_title_buffer_in_place_shows_its_bytes_at_its_offset` fails in some runs and not in others (2 of 14 runs of its crate before this update, 1 of 13 with it; it places plain private memory, which this update does not touch), and the graphics crate's test program has stopped without a result on an access violation while it and others were under way, once in each of two series of runs. `warp_budget_saturates_without_wraparound_and_preserves_draw_fields` took for granted which group of 64 records the card runs first, and accepts either since the update before |
| In progress | about 150 fixes from VIctory and its open pull requests, mostly shader translation and graphics, on the `improvements` branch once every test passes |
| Graphics in the game | **people, skin, clothes, streets, shops, cars and night lights draw** (since 2026-11-25). Right after a camera cut faces or skin can be black for a second, until their textures are there. From launch to launch a fence and wall photos, people in the distance and a neck come out right or wrong: not understood yet |
| Frame rate in the world | **not playable: 8.4 fps average** (115 ms median) over a minute in the first room once it has loaded, 4.4 fps before. Runs since, played by hand: 5.8 to 9.2 fps in the last minute of each run, by where the player was; no clear change. While it loads, some 75 s, 0.4 to 4 fps. One scene on one PC; busier places are slower |
| Frame rate in the opening cutscene | **about 4.8 fps** (2026-11-26, night: ten runs of the same length, 3.43 to 5.58, 4.81 on average; in the 3.43 a fault, now mended, turned the faster texture answer off a minute in), about 4.0 in the three clean runs before (3.41 to 4.63; the "about 4.2" said here before was two of them), about 3.5 before that, about 2.0 before that. Still far from 30 |

The results in "Status of this fork" further down are fxceless's, measured on
another PC.

### Working on right now

- **Frame rate in the opening cutscene: 30 fps is the aim.** The pass over
  the textures shaders pick by index now takes 12 to 19 ms a frame (25 to 32
  before; runs c33, c34, c36, c37). The process uses about 5 of the PC's 6 cores (profile of
  run c31), so what costs processor time is what holds the frame back. Next:
  the descriptor tables and binding before each draw (about a core in all), and
  the copies of title memory (about two thirds of a core). In run c24 a frame had
  two halves of about 200 ms each (2.5 ms steps): first one thread of
  the title's works alone while ours stand idle, then our submitting thread and
  its four helpers work (hashing, copies of title memory, descriptor tables,
  textures) while the title waits for them. What that title thread does is
  next. The card is busy about a third of the frame. A fence the title signals
  is set 120-290 ms after its call (run c22, `VICTORY_PACE=1`).
- **Frame rate: 20 fps first, then 30.** Our submitting thread works about
  100 ms a frame: some 65 preparing (the descriptor tables the title wrote,
  refreshing copies of its buffers) and 35 sending about 125 MB up to the card,
  while the copy-back brings some 55 MB down. The card would allow about 18 fps
  by itself. 20 fps needs all of it under 50 ms.
- **Memory.** The game takes about 21 GB of the PC's commit, 10 GB of it the
  card's memory (Windows charges that too), plus a 4 GB pool of the title's
  pages. With other programs open the PC's limit came within 0.5 GB in every
  run. 2.2 to 2.6 GB of the blocks textures are placed in stand empty, in
  pieces, so no block can be given back.
- **Flicker** (ground going black and back): textures the title fills after
  writing their view are caught and uploaded again 1 to 7 frames late; whether
  that is the cause is not known yet.

### Progress by area

| Area | Done | Change | Based on |
|---|---|---|---|
| Frame rate towards 20 fps, first room, loaded | 42 % | no clear change | measured: 8.43 fps average over the last minute of a 4 minute run (511 frames, median 115 ms, 4,406 draws a frame). Runs since, played by hand: 5.8 to 9.2 fps (9.15 with 2,948 draws a frame) |
| Frame rate towards 30 fps, the same | 28 % | no clear change | the same runs |
| Frame rate while the room loads | about 5 % | no change | measured: 0.4 to 4 fps for about 75 s |
| Frame rate in the opening cutscene, towards 20 fps | about 23 % | 24 % before: the usual runs since are lower, 3.98 fps on average, 4.57 over all fourteen | measured: 4.67, 4.28, 3.50 and 3.47 fps in the four usual runs since (c42, c48, c51, c58; the others had switches on for measuring, see the newest update); before them 4.87, 5.17, 5.48, 3.43, 4.10, 5.25, 3.76, 5.22, 5.26 and 5.58 fps in ten runs with textures' memory asked of the runtime (c32-c41), 4.81 on average (4.96 without c35, where a fault since mended turned it off a minute in), at 2,015 to 3,343 draws a frame; 4.63, 3.85 and 3.41 in the three clean runs before it (c23, c24, c27), 3.96 on average; 2.86 to 3.97 in five runs before those (c13-c16, c19); about 2.0 before staging buffers were cut from blocks. Each run: the game started, the last 60 s of its first 75 s in the world measured. The scenes in that minute differ from run to run, so a few runs say the direction more surely than the size |
| Frame rate in the opening cutscene, towards 30 fps | about 15 % | 16 % before | the same runs |
| Card time a frame, towards the 50 ms of 20 fps | about 97 % | 88 % before | measured: 44 to 53 ms busy in c32-c34, c36 and c37 (30 to 36 the title's lists, 13 to 17 sending and hashing, 1 to 1.5 comparing) at 2,700 to 3,300 draws a frame; 54 to 60 ms in the four runs before at 2,900 to 3,900 draws; 76 ms at 4,400. Fewer draws, not only a faster card. It counts as done at 50 |
| Draws that reach the card | 94 % | 90 % before | measured: about 256 of 4,660 a frame are not run, 254 of them the ray tracing builder's programs, which are off on purpose |
| Shaders that translate | 99 % | no change | measured earlier: 25,679 of 25,889 in the game's whole library |
| Shaders the game asks for that fail | 4 left | no change | 3 refused by the driver, 1 not translated |
| Picture looks right | about 75 % | no change | estimate: the first room and its yard look right in most launches; 20 of 20 frames of the last two clips lit and steady, without black or red faces; skin on arms looks grey; ground flicker reported; no ray traced shadows or reflections. In the opening cutscene (contact sheets of runs c16-c19): with this update faces were lit in the frames of c16 and c19; in c18, where it was switched on and off in turns, the two frames just after it came on had a black face and a head without its texture; with it turned off (c17) 4 of 9 frames had a black face or black arms. Green and yellow dots on one character's face in 3 frames of run c16, not seen in c17-c19: not understood yet |
| Runs that finish | 49 of the last 51 short runs | 34 of 34 before | measured: the 75 s cutscene runs c8 to c58 ran to the end and closed when asked, except c55, where the title stopped on a breakpoint of its own while loading (see the newest update below); and in c46 the card's device was lost two minutes in (a fault in a shader, reported by the driver; see the newest update below), so it does not count. Longest frame 0.65 to 1.28 s in c32-c34, c37 and c39-c41, 2.1 to 2.2 s in c35, c36 and c38; up to 2.3 s in the runs c26-c30, which were profiled or had switches on for measuring; 2.4 to 10.8 s in runs before c23 |
| Textures and cars loading in time | about 55 % | no change | estimate |
| Audio | about 88 % | 90 % before | measured: one gap of 0.54 s in a longer recording; the 88 s without a gap named here before were not the whole truth |
| Keyboard and mouse | 100 % | | done |
| NVIDIA driver | 100 % | | the game draws through it |
| DLSS | 0 % | | no code for it yet |
| Ray tracing | 0 % | | off. A stand-in for its indirect light (`VICTORY_AMBIENT`): a constant, less of it in creases and corners from the depth image. That is not ray tracing |

The measured rows come from the first room on the PC below, with the Series S
profile and the switches listed, unless they name the cutscene. The estimates
are my own judgment, not measurements.

### New in this update (2026-11-26, morning): where the long frames come from; switches measured, none adopted

Seventeen cutscene runs since the update below (c42-c58): five usual ones (c42,
c46, c48, c51 and c58, with Windows' memory counters read in c48) and twelve
with a switch, a measurement or a profiler on; and the full checks (862 pass; the one shader test that
failed before still fails). New in the code: the merge of what shaders wrote
compares 32 bytes at a time, and two switches for measuring, off by default
(`VICTORY_WRITE_PLAIN`, `VICTORY_GRAIN_SAMPLE`, `VICTORY_GUARD_HOT`). Tests
check the merge against the old loop, the plain write on a page that is not
there, and a block left open.

- **Writing back what shaders wrote** (`crates/victory-gpu/src/shadow.rs`,
  `crates/victory-gpu/src/plaincopy.rs`). A thread of its own puts what
  shaders wrote back into title memory: 55 to 68 MB a frame, 55 to 61 ms
  (c42-c45), about 0.92 ms a MB. Now it compares 32 bytes at a time (AVX2)
  instead of one, with the old loop for processors without AVX2 and a test
  that the two agree; the cost a MB did not clearly change, so the time is in
  moving the memory, not in comparing it. Writing title memory directly with a
  guarded store instead of through Windows (`VICTORY_WRITE_PLAIN=1`, off by
  default): 0.85 to 0.86 ms a MB, about 7 % less, and none of 1,009,106
  writes found a page missing (c43-c45); but those three runs made 4.69, 3.32
  and 3.49 fps, with frames up to 3.1 s, so it stays off until a run says why.
- **How much of a guarded block changes** (`VICTORY_GRAIN_SAMPLE=1`, off by
  default, for measuring). The guards find the title's writes 64 KiB at a
  time, and a block found written is copied whole: 160 to 200 MB a frame. In
  one copy of 32 the 4 KiB pages were compared with the copy's: in c47,
  86,423 of 324,141 had changed (27 %; 8 to 59 % from one log line to the
  next). Guards of 4 KiB would copy less, but each guard costs about 35 µs
  now (1,453 a frame took 51 ms in c47), and a block's changed pages would
  each need one: about three times as many. Not worth it as things stand.
- **The PC runs short of memory in the cutscene.** Measured with Windows'
  counters over the measured minute of c48 (a usual run, 4.28 fps): 838 MB
  of the 32 GB free on average, 12,320 pages a second read from disk to
  resolve faults, 45,194 faults a second on pages Windows had taken away,
  and the process at 19.7 GB in memory, 22.3 GB private. The long frames go
  with the title's remapping of its memory, which slows from about 0.04 ms a
  call to 0.3 to 0.6 ms in them (frame 4183 of c47: 1,541 calls, 873 ms).
- **A finer grid for windows of title memory** (`VICTORY_WINDOW_KB=256`,
  off): 1.6 GB less private memory, 2,224 MB free on average and 4,995 pages
  a second read from disk (c49, against c48 above); but 3.28 fps, and the
  title's remaps were slower still (248 µs a call against 101), so memory
  short is not all that slows them. Not adopted; one run each.
- **Windows read in place** (`VICTORY_INPLACE=1`, off; never run in the
  cutscene before): only 80 windows (380 MB) could be opened in place, and
  1,200 were turned down, nearly all of them in the title's pool of
  physical pages, which cannot be opened that way (c50: 3.75 fps, and the
  card's time in the title's lists 56 ms against 34 in c48). Not adopted.
  Showing the pool's scattered pages to the card as tiles of one buffer is
  ruled out too: tile mappings into an opened heap remove the device (an
  earlier probe, `crates/victory-gpu/src/inplace.rs`).
- **The guards find the same blocks frame after frame.** A count in the
  fault handler (`crates/victory-gpu/src/watch.rs`, always on, a line in
  the log): of the guard faults of c51, 12 to 29 % were on a block that
  faulted in the frame before too, 3 to 30 % on one that faulted earlier in
  the same frame, 25 to 53 % on one that faulted two to four frames before.
- **Blocks that faulted lately left open** (`VICTORY_GUARD_HOT=4`, off by
  default; a test checks that such a block counts as written at every
  check, so no write is missed): no gain. A block left open is copied at
  every asking, many a frame: 431 and 465 MB copied a frame against 174
  (c52, c53 against c51), and faults fell little, since guarding another
  view of the same page makes it read-only again. 3.50 and 2.61 fps against
  3.50. The title's remaps were fast in c52 (60 µs a call) and slow in c53
  (319): what they cost goes with the scene, not with this.
- **Six helper threads instead of four** (`VICTORY_WIDE_THREADS=6`; the
  runs set 4): 5.22 fps in c54, 3.79 in c56, within what runs differ by; the
  guards' wait for one another doubled (24 to 27 ms a frame against 9 to 14).
  Not adopted. In c55, with the same setting, the title stopped on a
  breakpoint of its own while loading, before the world (exception
  0x80000003 just after a call it takes for one that does not return):
  the first such stop in any run log here. Not understood; it may have
  nothing to do with the setting.
- **Where a frame could go without copies.** The ray tracing work has a
  directory with an entry for every 64 KiB of title memory, through which
  translated shaders read and write by address. Pointed at the title's pool
  of physical pages (a committed section can be opened as a heap; a reserved
  one, as made today, cannot), and used by every shader, it would leave out
  the copies, the guards and the writing back that fill most of the
  submitting thread's frame. That is a large change to the translator and
  the layer, with the card then reading the PC's memory over the bus; not
  begun.
- **Staging memory in bursts** (a new log line, `crates/victory-gpu/src/slabs.rs`).
  A profile of c57 found one of the title's threads 41 % of the time in the
  kernel, mapping an upload buffer made for one texture's staging. Counted in
  c58: 2,785 staging buffers (1,121 MB) were made one by one because the
  blocks they are cut from held all they may (1,536 MB), nearly all in two
  bursts of a few seconds (at the world's start, and a minute later), and 216
  (about 2 GB) because they were larger than a piece may be (8 MB). Between
  the bursts some 50 MB of the blocks are in use; the blocks were made 208
  times in the run (26 ms each), let go after the bursts and made again at
  the next. Not changed yet: more blocks kept, or a higher cap, is more of
  the PC's memory, which is short already.
- **A lost device in c46.** About two minutes in, the driver reported a
  fault in a shader on the card ("Graphics SM Global Exception … Multiple
  Warp Errors"), and the device was lost (0x887A0006) while the submitting
  thread prepared a submission. It is the first in all the run logs kept
  here; the driver watch turned off the draw windows (`VICTORY_INDIRECT_WINDOW`),
  the path it holds suspect for such faults, as it should. Not understood
  yet. c47 ran in the same way and closed cleanly.

### New in this update (2026-11-26, night): textures' memory asked of the runtime, about 4.7 fps

| Runs (75 s each, the opening cutscene) | How much of a texture's memory is there, asked of | fps |
|---|---|---|
| c23, c24, c27 | Windows, page by page | 4.63, 3.85, 3.41 |
| c32 | the runtime (how much is mapped) | 4.87 |
| c33 | the runtime (that, and whether all of it is there) | 5.17 |
| c34 | the same, on by default (no switch given) | 5.48 |
| c35 | the same; one check went wrong a minute in and turned it off (see below) | 3.43 |
| c36, c37 | mended | 4.10, 5.25 |

- **What it cost.** Before a texture that shaders pick by index is drawn, the
  graphics side asks how much of its memory is mapped (textures stream in, and
  the runtime maps their pages as they arrive), and before one is read,
  whether all of it is there. Each was a question to Windows for its pages and
  a one-byte read for each page not in the working set: about 25 µs a texture,
  400 to 500 textures a frame, on the thread that records the title's commands.
  That asking took 13 to 15 ms a frame (runs c27, c31), and the whole pass
  over those textures 25 to 32 ms. Now: 0.7 to 1.2 ms, and 11.7 to 19.1 ms
  (c33, c34, c36, c37; the 19.1 in c36's busier scene).
- **How.** The runtime now tells the graphics side of the placeholders it
  reserves for physical pages, as it already told of every view it maps into
  them. Inside a placeholder, what the views show is what is mapped
  (`crates/victory-gpu/src/inplace.rs`, `crates/victory-gpu/src/texture.rs`,
  `crates/victory-hle/src/modules/xmem.rs`). The runtime tells of a view after
  it is mapped and of its end before it is unmapped, so the answer is never
  more than Windows would give. `VICTORY_MAPPED_KNOWN=0` asks Windows again.
- **Checked against Windows while it runs.** One answer in 64 is also asked of
  Windows, while no remap of that memory is under way. Where the two differ,
  that placeholder is asked of Windows from then on and a warning is logged.
  None differed in c32-c34 and c36-c37 (4,820, 12,181, 10,514, 11,314 and
  11,819 checks). The title can free a
  placeholder without the runtime's knowing; memory the runtime reserves over
  one makes it forget that one too. A wrong answer would say a texture is not
  there yet, never that memory is there when it is not.
- **Mended the same night.** In c35 one check differed: a texture whose
  first and last pages were not mapped and six pages between were. Asked of
  Windows the answer is none then (the pass does not look between when
  neither end is there); the views had said six pages. For that one the
  title's whole 16 GB placeholder was forgotten, and the pass asked Windows
  again for the rest of the run (3.43 fps). Now the views answer as Windows
  does there, so nothing changes for such textures; a check that differs has
  Windows answer that once, and the placeholder is forgotten only at every
  fourth. c36 drew a busier scene (594 textures made a frame against about
  370 in c33, c34 and c37) and made 4.10 fps.
- **Blocks of upload memory kept between bursts** (`crates/victory-gpu/src/slabs.rs`).
  Making and mapping a block of 32 MB takes about 26 ms. Let go after two
  seconds empty, blocks were made again about once a second through the
  cutscene (run c39: 188 blocks in 4.9 s, and 638 upload buffers of their own
  once the blocks had reached their limit). Now up to 8 wholly free blocks
  (256 MB) stay for up to 20 s (`VICTORY_UPLOAD_PIECES_KEEP`, 1 as before).
  c40 and c41: 95 and 101 blocks in 2.3 and 2.5 s, 33 and 42 upload buffers
  of their own; 5.26 and 5.58 fps against 5.22 in c39, which is within what
  runs differ by. The PC's commit came no nearer its limit than before (195
  and 352 MB free at the closest; 146 to 498 MB in the 17 runs before).
- **Tried, no gain.** Setting the title's fences as soon as its work is sent
  (`VICTORY_FENCE_EARLY=1`, for measuring only: frames can show before they are
  done): 3.46 fps (c30), so waiting for the card's fences is not what holds the
  frame. Presenting on another thread (c29): no gain, the submitting thread's
  work grew as much as it saved.
- **New log lines** for measuring: the bytes of the host copies of title
  memory refreshed each frame, split into those found written and those copied
  whole, and the 4 KiB pages Windows' write watch found against the 64 KiB
  blocks the guards found.
- A test checks the answer from the views: inside a placeholder, reaching out
  of one, a view taken away, and a placeholder reserved over or forgotten.

### The update before (remaps hold only what they change, about 4.0 fps)

| Runs (75 s each, the opening cutscene) | Lock while the title remaps its memory | fps |
|---|---|---|
| c13-c16, c19, c21 | all of title memory, as before | 2.86, 3.97, 3.72, 3.45, 3.75, 3.42 |
| c23, c24 | only the addresses the remap changes | 4.63, 3.85 |

- **What the frames waited for.** Texture refreshes read title memory in place
  and hold a lock while they do, so that the runtime does not take pages away
  under the read; the runtime takes the same lock alone while it maps or
  unmaps physical pages, some hundreds of times a frame here. It was one lock
  for all of memory: every remap, wherever it was, held up every texture read,
  and every read every remap. In run c22 refreshes on all threads waited up to
  7.4 s a frame (added over the threads) "for title memory to hold still", and
  in one frame of 5.2 s 1,327 remaps held the lock for 3.9 s. Earlier runs
  had 0 to 880 ms a frame of it.
- **Now a remap holds only the addresses it changes** (`crates/victory-gpu/src/watch.rs`,
  `crates/victory-hle/src/modules/xmem.rs`; `VICTORY_MAP_RANGES=0` brings the
  one lock back), and a read only the texture or buffer it reads. A remap
  holds the whole of every view it meets, since the runtime takes a whole view
  away to change one page of it and maps the rest again. A read waits for a
  remap that is waiting where it reads, so that reads cannot keep a remap out
  for ever. In c23 and c24 the refreshes waited 0.0 ms in every line of the
  log, and remaps 0.0 ms for readers.
- **Tests.** A new test has a thread read the first and last page of a view of
  three pages while the middle page is mapped again, 400 times on fresh views.
  With the remap holding only the page it changes, the test process stopped
  on an access violation in 3 of 3 runs; with the whole view held, it passed 3
  of 3. Another test checks that reads and remaps wait for one another only
  where their ranges meet.
- **Fence timing** (`VICTORY_PACE=1`) says its line every ten seconds too, not
  only every 256 frames (at 3.5 fps that was longer than a run). In c22 a
  fence the title signals on its queue was set 120 to 290 ms after its call,
  about 17 such signals a frame.
- The remaps themselves still take time: 36 to 68 ms a second usually, and in
  some seconds 600 ms or more (1,178 views unmapped in 614 ms in one).

### The update before (staging buffers cut from blocks, about 3.5 fps)

| Runs (75 s each, the opening cutscene) | Staging buffers | fps |
|---|---|---|
| c3-c5, c7, c9, c11, c12 | one upload buffer each, as before | 2.22, 1.66, 1.69, 1.58, 2.23, 2.32, 2.61 |
| c17 | the same (`VICTORY_UPLOAD_PIECES=0`) | 1.71 |
| c13-c16, c19 | cut from blocks | 2.86, 3.97, 3.72, 3.45, 3.75 |

- **What the frames waited for.** In the frames of 1 to 3.6 s the card stood
  idle and our submitting thread was at work for less than half a second. A
  sampler of what the title's threads wait on (stopped every 10 ms, the handle
  read from the register Windows' wait keeps it in) showed the title's render
  thread waiting for its five recording threads, and those making upload
  buffers for new textures: each a new allocation of the driver's, mapped,
  waiting for the card's paging of it. Frames of 1.8 to 2.4 s made 1,400 to
  2,700 of them.
- **Staging buffers are now cut from blocks of upload memory made once**
  (`crates/victory-gpu/src/slabs.rs`, `VICTORY_UPLOAD_PIECES`, on; `=0` off,
  `=alt` in turns). Blocks are 32 MB, a piece up to 8 MB, in steps of 4 KB; a
  piece is given back when the last list that named it has run, and a block
  that is wholly free for 2 s goes. Upload buffers made in a run went from
  about 25,000 to about 50. In one burst the pass over textures picked by
  index had 3.2 GB of staging buffers alive at once, so the blocks may hold
  1.5 GB at most (`VICTORY_UPLOAD_PIECES_MB`); past that a staging buffer is an
  upload buffer of its own, as before.
- **How much of a texture is there is asked of the kernel once for up to 256
  of its 64 KB blocks**, not once a block (`mapped_bytes`): 27 against 33-38
  microseconds a texture, 12.7 against 16-19 ms a frame of the pass over
  textures picked by index (run c21; 3.42 fps, within the range above). Most
  of what is left is the one-byte kernel reads of pages not in the working
  set.
- **Presents that return at once** (`VICTORY_ASYNC_PRESENT=1`, run c20): 3.72
  fps, no gain, and a frame of 10.7 s in which the title's remapping of its
  memory took 10.6 s (9,235 calls of about 1.1 ms; 10-25 microseconds is
  usual). Off.
- **`VICTORY_FRAME_RATE`** (31 to 240) lets the title's clock tick faster than
  the 30 a second it asks for, for when frames are made faster than that.
  Seen at 60 on the start screens; not tested in play yet.
- Measured, no gain, off: the process's pages kept from Windows' trimming
  (`VICTORY_KEEP_RESIDENT`, run c8: as many of the title's pages were out of
  the working set with it as without); textures copied out of title memory
  under the mapping lock and detiled after it (`VICTORY_TEXTURE_SHORT_HOLD`,
  run c10); presents that return at once (`VICTORY_ASYNC_PRESENT`, run c6,
  again no clear answer, and see above); more changed runs merged before flights are settled
  (`VICTORY_MERGE_RUNS_WIDE`). `VICTORY_COPY_PROBE=1` is a measurement only: 1.5 to
  23 % of the title's pages a copy reads were out of the working set first.
- The runs set `VICTORY_AMBIENT=0.6` (at 0.2 faces went black in sunlit
  scenes) and `VICTORY_MERGE_MASK=1`, besides the switches listed further down.

### The update before (lighting stand-in, fewer waits at indirect clamps)

No clear frame rate gain that time (see the table above); what changed:

- **The stand-in for the ray traced light** (`crates/victory-gpu/src/ambient.rs`)
  is KomanderDG88's newer one (their fork, after Belobug's): less light in
  creases and corners, worked out from the depth image, and reflections held
  to a part of the light around, so that glossy floors do not glow white. The
  program it holds reflections after was not seen in these runs, so that part
  has not acted here yet; the log says why when it does not. Read line by line
  before merging; it reads and writes nothing outside the card.
- **Indirect clamps wait less** (`VICTORY_CLAMP_LEAN`, now on; `=0` turns it
  off): each of the ~1,900 indirect executions a frame waited for every shader
  write before and after its clamp. After it the wait is never needed, before
  it only if a draw that can store ran since the last such wait. 4.5 ms off the
  usual frame, measured in turns in one run.
- **Textures go into the fullest block first**, so that blocks empty as their
  textures go. It helps while a scene loads (87 blocks against 89); in a running
  scene 2.6 GB of them still stood empty in pieces.
- **Keyboard and mouse take the play bindings by themselves** once the world
  is shown: W A S D walk without F10 first (`VICTORY_KBM_AUTO_PLAY=0`: not;
  F10 and Esc switch as before).
- Measured in turns, no gain, off: only the bytes that differ sent up
  (`VICTORY_COPY_CHANGED`: 13 % of what the title marks written differs, but
  finding it costs as much as it saves, 154 against 151 ms); constants copied
  on the card (`VICTORY_CONSTANTS_ON_CARD`, +3.5 ms); the copy-back through a
  view of our own (`VICTORY_BACK_OWN_VIEW`: 178 against 153 ms now, and both
  runs with it froze); windows of title memory cut at 1 MB (`VICTORY_WINDOW_KB=1024`:
  0.7 GB less memory, 180 MB copied a frame against 121).

### The update before (8.4 fps)

Nearly all of the gain is in threads that no longer wait for one another. The
same scene, the last minute of unattended runs (average fps / median ms):

| Run | What was new | fps | ms |
|---|---|---|---|
| m7 | before | 4.38 | 214 |
| m11 | copies from 64 KB on in the card's memory, not 1 MB (`VICTORY_CARD_BUFFERS_MIN_KB=64`); the guards of title pages without one lock for all | 6.03 | 158 |
| m15 | the recording threads' tables of descriptors and buffers without locks | 7.80 | 123 |
| m18, m19 | the journal of written pages without a lock, only changed copies asked for, fewer small allocations | 8.49, 8.43 | 114, 115 |

- **The recording threads** waited for locks 31 % of their time in our code:
  one lock held while any descriptor was made, one for the table of buffers.
  Descriptor slots now have 256 locks and a check without one; looks at the
  tables of buffers and textures share their locks, and each thread remembers
  what it found last. Their time in our code went from 43 % to 34 % of the
  clock, the waits in it from 31 % to 7 %.
- **The bus, not the card's speed.** With every small copy in the PC's memory
  the card took 73 ms for the title's lists, with those of 64 KB and more in
  its own 52 ms: it reads the rest over PCI Express (3.0 x8 here).
- **Objects let go while the card may use them** went through one list under
  one lock; each thread has its own list now.
- `crates/victory-gpu/src/turns.rs`: a switch set to `alt` (or `alt40`) goes on
  and off by itself every 20 (40) seconds and says so in the log, so that one
  run measures both in the same scene.
- Read handles for DirectStorage, joined texture reads and counters of what
  was dropped and why, in the log.
- Merged: fxceless/victory-fork up to `465b94f5` (joined parts, gathered
  indirect records, the frame rate in the picture), parts of Marvisak/VIctory's
  open #929, #911 and #936.

Tried, measured in turns, no gain, off by default: presents that return at once
(`VICTORY_ASYNC_PRESENT`: 163 against 187 ms in one run, 120 against 121 in
another); copied descriptor records looked for on a thread of their own
(`VICTORY_SYNC_BESIDE`: 133 against 140); the heap's buffer views made at the
submission (`VICTORY_HEAP_PASS_LATE`: 120 against 126; it makes new buffers
visible a frame sooner, which the picture does not show).

Switches used for the numbers above, besides the defaults:
`VICTORY_CARD_BUFFERS_MIN_KB=64 VICTORY_VIEW_KNOWN=1 VICTORY_INDIRECT_WINDOW=1
VICTORY_INDIRECT_GATHER=1 VICTORY_JOIN_PARTS=1 VICTORY_COPY_AHEAD=1
VICTORY_TITLE_PRIORITY_FLOOR=0 VICTORY_AMBIENT=0.2 VICTORY_LEVELS_ON_DEMAND=1
VICTORY_TEXTURE_ROOM_PERCENT=95 VICTORY_ASYNC_SUBMIT=1 VICTORY_QUEUE2=1
VICTORY_COPY_PLAIN=1 VICTORY_WRITE_BEHIND=1 VICTORY_TEXTURE_NO_WAIT=1
VICTORY_TEXTURE_SPILL=0 VICTORY_RELAY=2 VICTORY_WIDE_REFRESH=1
VICTORY_WIDE_MERGE=1 VICTORY_WIDE_TEXTURES=1 VICTORY_WIDE_THREADS=4
VICTORY_LAND_ASIDE=1 VICTORY_JOIN_COPIES=1 VICTORY_CARD_REHASH_RUNS=1
VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_CARD_BUFFERS_MB=1024
VICTORY_PIPELINE_WORKERS=10 VICTORY_PIPELINE_WORKERS_WORLD=3
VICTORY_PIPELINE_WAIT_WORLD_MS=40 VICTORY_DRIVER_PROCESSORS=3`.

Earlier the same day: people were black because the game's bounce light is ray
traced and does not run here (`VICTORY_AMBIENT=0.2` puts a constant in its
place) and because the card was full (`VICTORY_LEVELS_ON_DEMAND=1`).

### Corrections

- **"About 5.2 fps" in the cutscene, said earlier the same night, was three
  runs of six.** All six since textures' memory is asked of the runtime made
  4.72 on average, 4.97 without the run in which a fault (mended) turned it
  off; ten runs by now, 4.81.
- **The cutscene's "about 4.2 fps" in the update before was two runs.** The
  third clean run after that change (c27) made 3.41 fps; the three together
  3.96.
- **These are not the highest frame rates anyone has.** fxceless's fork, which
  much of this comes from, reports 8 to 10 fps in the same rooms on an
  i7-14700KF with an RTX 4070 (97 to 104 ms at about 3,850 draws). What can be
  said of this one is that the same scene on the same PC went from 4.4 to 8.4.
- **The audio row was too good**: a longer recording has one gap of 0.54 s.
- Page runs mapped as one view (`VICTORY_MAP_RUNS`) broke faces and are off; a
  read that failed uploaded zeros, which were black, and does not any more.
- **The new code did not raise the typical frame rate.** 367 ms a frame
  against 329 ms in the best run before it. Preparing a frame went from 153 to
  98 ms, and other waits took its place. The shots compared are not exactly
  the same ones.
- **With every texture loading it is slower, not faster:** 498 ms a frame.
  Before, textures that were never loaded cost nothing.
- "12 shaders still fail" counted what the game had asked for. Of the whole
  library 210 do not translate; most are never asked for.
- Earlier: the two switches tried before this update did not help, and the
  "3D lighting texture" was most likely an ordinary 2D one.

### Progress report, 2026-11-25 (earlier)

Measured on one PC: Windows 11, Ryzen 5 5500 (6 cores), RTX 5060 Ti 16 GB,
32 GB RAM, in the opening cutscene, with the Series X and Series S profiles.
The code for what follows is on `main` with this update.

| What | Before | Now |
|---|---|---|
| Picture in cutscenes | black, with sound (only the first of the planes the title presents was shown) | drawn |
| World frames | 2 to 5 s each; texture refreshes waited up to 4.1 of 4.2 s for title memory to hold still | 0.25 to 0.7 s in most shots, 0.14 s at best |
| Making a texture on the card | 0.5 to 1.2 ms each | about 0.05 ms (placed in shared blocks of card memory) |
| Upload buffer for a texture | 1.4 ms each | about 0.2 ms (used again) |
| Upscaler shaders | several did not translate | translate |
| Input | controller only | keyboard and mouse too, with a key file |
| Intro and loading screens | 30 fps | 30 fps, with dips while the world loads (not a milestone) |

What held the frame rate down at that point, as measured:

- The processor is at 100 %. NVIDIA's driver compiles shaders again in the
  background on four threads, about 3.5 of the 6 cores for minutes; the same
  scene went from 1-2 fps to 6.9 fps when that stopped by itself. Windows only
  lets a program switch it off in Developer Mode, so those threads are being
  kept on one core instead.
- Memory: the game takes 20 to 27 GB on top of everything else that runs, the
  system is at its commit limit, and the page file grows by 14 to 35 GB.
- Card memory is full (16 GB in use), and copies between title memory and the
  card take 60 to 230 ms a frame each way. The card itself is busy for 20 to
  200 ms of a frame.

Known problems: skin can be black for a moment after a cut; four shaders the
game asks for still fail, all compute programs; ray tracing is off, so there
are no ray traced shadows, reflections or bounce light.

No footage is published here: this repository holds nothing from the game
([docs/asset-policy.md](docs/asset-policy.md)).

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork. Upstream's history is kept intact below
  the single commit that carries this fork's changes.
- **[XCL](https://github.com/ivall/xcl)** by ivall and its contributors
  (GPL-2.0-only) was used as a reference for how the same title behaves under
  another layer. Fixes inspired by it were written anew in VIctory's code.
- The guard over every view of a memory page is code by **myokex**
  ([victory-amd](https://github.com/myokex/victory-amd), commit `c6dc4d8`), which
  adapts pull request #115 here by Spexivus, its correction by joroyt and a part of
  VIctory #1122 by tightfall. The read-beside fix for kernel reads into title
  memory is by **Joshua Odom** (commit `10ea38e` of the same fork family). Both
  are carried here unchanged apart from one merge conflict.
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

### Newest (2026-11-25, later the same day)

**Update, 2026-11-25 afternoon: what a frame is made of, measured, and two
changes that hold.** First rooms, about 3,850 draws a frame: 104 ms a frame
(9.6 frames a second) with the first two changes below, 97 ms (10.3)
with all of them, from about 120 ms in the morning. Still far from the console's 33 ms.
- How the title paces itself (new meters in `fence.rs` and
  `console_insn.rs`): it never asks for an event. Each frame begins with
  one small submission and a signal, and a title thread then waits for
  that fence value by reading it in memory with `monitorx` / `mwaitx`
  (AMD-only: about 280,000 emulated exceptions a second on an Intel
  processor, all from this wait). The value is set when the layer has
  finished the whole frame before. So a frame is the time the layer needs
  for one frame's burst of submissions plus about 20 ms of the title's
  own; putting presents behind the line, or giving the title's threads
  more processors, changes nothing (both measured).
- Two walls stand side by side and both are full: the layer's submit
  thread (about 95 ms of a 120 ms frame in the morning) and the card
  (about 102 ms busy, by a timestamp pair around every host list:
  `cardmeter.rs`). Taking time off one alone moved the frame by 0 to 7 %.
- `VICTORY_JOIN_PARTS=1` (off by default): the title's lists wait for
  memory values, which cut its ~15 submissions a frame into ~29 parts;
  parts that follow one another now go to the host as one submission
  (9 a frame), the same lists in the same order. About 6 %.
- `VICTORY_BACK_OWN_VIEW=1` (off by default): what shaders wrote goes
  back into title view memory through a view of the layer's own, so the
  copy that wrote it is not read in again. About 9 % (submit thread 93 ->
  81 ms a frame, 104 -> 80 MB a frame checked). A CORRECTION: an earlier
  run seemed to show washed-out frames with this switch; the same frames
  turn up in about 6 of 21 runs with no switch at all (cause not known),
  and `VICTORY_BACK_OWN_CHECK=1` counted 0 stores gone astray in 1.36
  million.
- Where the card's time goes now (`VICTORY_CARD_METER_PIPELINES=1`, a
  table by pipeline): no pass is large. About 82 ms of the title's lists
  are spread over ~3,400 pipeline bindings a frame; ~2,400 of the
  indirect commands still get a clamp pass of the layer's with draw
  windows on (960 are compute dispatches; about 1,000 are mesh records
  the title keeps scattered through a 100 MB argument buffer, which a
  window of records at one step cannot serve).
- `VICTORY_INDIRECT_GATHER=1` (off by default): a single indirect record
  that misses its window gets a slot in a scratch buffer of its list, and
  the list's prelude clamps all such records in one pass by a table of
  their offsets (the clamp shader with a gather bit), where each draw had
  a compute pass of the layer's in front of it. Proved by clamping every
  gathered record again at its draw and comparing on the card
  (`VICTORY_INDIRECT_WINDOW_CHECK=1`): 7.9 million records, none differed.
  About 600 of the ~1,600 draws that missed a window are served; the
  others come after a dispatch or a storing draw in their list. With
  `VICTORY_CARD_REHASH_RUNS=1`: 104 -> 97 ms a frame (10.3 frames a
  second at 3,800 draws).
- What the card's time is now, added up: of ~87 ms busy in a 97 ms
  frame, the layer's own lists are ~19 ms and its clamp passes 10-20 ms;
  about 50 ms are the title's own ~30 compute passes and its geometry at
  2560x1440 as translated. With every pass of the layer's gone, this
  scene would still be near 20 frames a second on this card: the
  translated shaders have to get cheaper too.
- The frame rate is drawn into the top left corner of every frame shown
  (`fpsbox.rs`; `VICTORY_FPS_COUNTER=0`: not): the window's caption has
  it too, but a capture of the picture does not carry the caption.
- `VICTORY_COPY_CHANGED=1` (off by default): a refresh of a copy in the
  card's memory sends on only the 256-byte chunks that differ from the
  copy's CPU side. The layer's sync lists on the card fall from 12 to 8
  ms a frame, the submit thread pays 3.6 ms for the compare and is then
  the limit: no net gain yet.
- HONEST LIMIT OF ALL THE ABOVE: those numbers are the scene standing
  still. While the camera moves the title streams assets, and about half
  the time goes to frames of ~490 ms in which the card is idle for ~330
  ms: some 850 remaps of title memory, 260 texture uploads and 450 disk
  reads in one frame. That is the next thing to measure and cut.
- The dips while moving, measured (a sampling profile of the opening
  cutscene, and a 370 s play session): the mean is 4.5 frames a second,
  with half the time in frames of ~490 ms in which the card is idle for
  ~330 ms. In those frames the title's render thread waits for its own
  jobs, not for the layer. One of those jobs is the title's page-mapping
  copies (`CopyPageMappingsBatchX` -> `xmem::map_alias`): the title maps
  every streamed texture page into its tiled heap, a range of one page
  each (over 4,096 different pool pages in a minute, about 850 pages in
  a frame that streams), and each is a view taken away and a view mapped
  in the kernel: 32 ms of every cutscene frame, 246 ms of a streaming
  frame, nearly all of it waiting on the process's address space.
- Two attempts on that, both taken back out: leaving alone pages that
  already show what is asked (counted: there are none, every remap is a
  real change), and mapping pool pages in a row as one view (1.8 to 4.2
  pages a view, remap time 140 -> 79 ms a frame, BUT faces came out black
  or red and one run drew garbage in 3 launches of 3, against none in 6
  launches without it). Reverted; why a longer view breaks texture
  streaming is not known yet.
- Measured and found to give nothing (switches left in, off):
  `VICTORY_REFRESH_ONCE`, `VICTORY_ASYNC_PRESENT`, `VICTORY_VIEW_KNOWN`,
  `VICTORY_SPREAD`, `VICTORY_QUEUE2=3`, `VICTORY_WINDOW_JUMP`. Each has an
  `=alt` form (twenty seconds on, twenty off, each change said in the
  log) so that one run measures both in the same scene.

**Update, the same night: the first room now renders right on the test
machine** (walls, textures, people, lamps), at 190 to 450 ms a frame (2 to 5
frames a second): the picture work has caught up, the frame rate has not.
What did it, on top of the list below:
- Descriptor records the title copies into its heap as plain bytes are
  mirrored before every submission (pull request #305 by Flyander, after
  upstream VIctory 86a5ed6). Measured here: about 40,000 records a frame
  that no call had announced; their host slots were empty or stale.
- A buffer view that does not start on an element of its buffer's host
  copy gets a copy of its own bytes instead of being refused (after
  upstream 86a5ed6): 3 to 6 such views a frame.
- Room on the card is kept for the render targets and buffer copies a
  world makes in its first frame: 98 buffer copies are made (945 MB) where
  the runs before made 1, and the processor no longer compares 550 to
  710 MB a frame looking for shader writes (25 MB now).
- Levels on demand ran on the card for the first time
  (`VICTORY_LEVELS_ON_DEMAND=1`, still off by default; with the fixes of
  pull request #747 by AscilCH): 3.1 GB of the card not allocated for
  texture levels the title does not hold, no device loss.
- Guarded blocks under storage writes are opened by default; 14 more
  shader programs translate; a program is translated with first-half
  interpolation pairs only if it was refused for one (with that on for
  every program, 8,551 working pixel programs came out changed).
- Frame rate, first cuts on the processor side: the descriptor sync uses a
  cheap fingerprint and leaves draw-table slots to their draws (the pass
  over the heap went from 70-220 ms a frame to about 3), and memory the
  guards already hold is not asked of the write watch (pull request #670
  by Jonirulah). Card copies' pieces can come down inside their own
  flights (pull request #648 by Jonirulah, `VICTORY_CARD_FETCH_INFLIGHT=1`,
  off by default: the copy-back thread's wait went from 181 ms a frame to
  0 here, but the card got busier, so no gain is claimed yet). The frame is
  still 250 to 380 ms: the card is the limit now, most of it the
  per-record clamp before each of some 6,700 indirect draws a frame.
- Pull request #220 (cheaper indirect draws) is IN this tree now, its
  windows off by default (`VICTORY_INDIRECT_WINDOW=1`). History, so nobody
  repeats it: with windows on it lost the device three times here. Its own
  checker (`VICTORY_INDIRECT_WINDOW_CHECK=1`) then showed one list drawing
  90 records of 2^24 vertices from a window, where the per-draw path lets
  eight through; a record of 2^23 vertices an instance or more now runs
  empty. Since that fix: eight runs with windows on, seven without a
  device loss and one that lost the device 44 s into the world (cause not
  found; see the driver watch below). One earlier loss after the fix had
  the in-flight fetch (#648) on as well; do not combine the two. The
  in-flight fetch itself measured slower here in two runs and stays off.
- Frame rate, second night (an RTX 4070; the first room went from about
  3.5-4 to 6-7 frames a second with ALL of the switches below; the
  defaults are unchanged and give about 4.5-5):
  - `VICTORY_ASYNC_SUBMIT=1`: submissions are prepared and sent by a
    thread of the layer, not by the title's render thread, which a
    profile showed inside the submit path for 257 of 292 ms a frame.
  - `VICTORY_QUEUE2=1`: fetches of what shaders wrote go to a copy queue
    (brought back, with the fix for a piece fetched beside a later
    flight); the copy-back thread's wait fell from about 125 to 7 ms a
    frame.
  - `VICTORY_INDIRECT_WINDOW=1`: see above.
  - `VICTORY_TEXTURE_NO_WAIT=1`: a texture refresh no longer waits while
    the title remaps memory; the loading phase had 5 readings over a
    second where it had 18.
  - `VICTORY_MERGE_GAP=64 VICTORY_MERGE_RUNS=4096`: title bytes put in
    under flights' shader writes are joined over small gaps (NOT exact:
    read the comment at `merge_gap` before using it).
  - On by default: fills of upload memory and read-only buffer copies no
    longer wait for flights and are checked against what the flights then
    wrote (`VICTORY_FILL_WAIT=1` waits as before; no mismatch in any run
    so far); up to 512 idle host lists and texture staging buffers are
    reused instead of made and freed every frame.
  - `VICTORY_PROFILE=1` (diagnostics builds): a sampling profiler inside
    the layer; it is how the above was found.
- Frame rate, third session: measuring first (same RTX 4070, first room).
  - A frame log (`VICTORY_FRAME_LOG`, on in diagnostics builds): every
    frame's length, and for the usual frame and each long one what it was
    made of (the render thread's time in the present call, the submit
    thread's work, copies, copy-back, recording, texture work, faults,
    disk). A card meter (`VICTORY_CARD_METER`) adds the card's time by
    kind of list from a timestamp at each list's start and end.
  - What they showed, windows off: a usual frame of 136 ms; the title's
    render thread waits 77 ms of it in the present call for the submit
    thread, which works 118 ms (copies of title memory 41, copy-back 40);
    the card is 85-98 % busy, of about 130 ms a frame 16 ms in our lists
    that bring card copies up to date, 10 ms in our compare lists and 102
    ms in the title's own lists with the per-record clamp. Frame time is
    linear in draws (about 32 microseconds a draw all-in): the title draws
    about twice as much on some frames, which are the long ones.
  - The regular dips of earlier test runs were the test harness: a saved
    frame (`VICTORY_CAPTURE_WORLD`, every 8th world frame) is read back
    and written as a PNG inside the present, 235 ms each.
    `VICTORY_CAPTURE_WORLD_EVERY=64` for runs that measure speed.
  - `VICTORY_INDIRECT_WINDOW=1` measured again with those instruments. A
    CORRECTION: the first figures given here (6.0 against 8.9 frames a
    second) compared two runs that stood in different scenes during the
    measured window (about 4,300 against 1,550 draws a frame) and are
    withdrawn. Compared at equal draws, by a line fitted to each run's
    frame times against its draws per frame: windows off 145 ms at 4,500
    draws and 271 ms at 10,000; windows on with `VICTORY_COPY_PLAIN=1`
    124 ms and 211 ms. On screen: 7-8 frames a second in the first rooms,
    10-12 in their lighter views.
  - A driver watch (on by default; `VICTORY_DRIVER_WATCH=0` off): the
    layer subscribes to the system's event log and logs every entry of a
    display driver as it is recorded; the first one turns draw windows
    off for the rest of the run. Every device loss here had such entries
    2.7 to 55 seconds before the device was removed, and no run without
    them lost its device.
  - A copy between two title textures that share one host texture is no
    longer recorded (the host does not allow a texture copied onto
    itself): 402 of them in a five-minute run.
  - The list of shader-visible heaps is a reader-writer lock (recording
    threads waited 48 ms a frame, summed, behind the heap sync); the heap
    sync looks at a changed page's records under one lock, fingerprint
    first; guarded view memory is left out of a submission's write-watch
    askings ahead (after triippywhale).
  - `VICTORY_COPY_PLAIN=1`: copies of title memory made in place by this
    process, with a handler that falls back to the kernel's copy on a
    missing page. Its first form also took the layer's one-byte "is this
    page there" reads, 1,000 to 1,300 faults a frame, and won nothing;
    reads under 256 bytes now stay with the kernel and a source that
    faulted is left alone for two seconds (4 to 9 faults a frame). The
    submit thread's kernel time fell from 44 to 20 ms a frame; the
    picture is the same.
  - The layer's submit and copy-back threads ask for the fast cores of a
    hybrid processor (`VICTORY_FAST_CORES=0`: not): the submit thread was
    found on a slow core in 6 % of its looks, now in none of 21,059.
    `VICTORY_DISK_CORES=slow` (off, not measured) asks the slow cores for
    the DirectStorage workers, which are as many as the logical
    processors and free to run anywhere.
  - Behind switches, built and unit-tested, not yet shown to pay:
    `VICTORY_REFRESH_ONCE=1`, `VICTORY_VIEW_KNOWN=1`,
    `VICTORY_BACK_OWN_VIEW=1` (picture to be checked),
    `VICTORY_ASYNC_PRESENT=1` (the present goes behind the queue's line;
    no gain while the card is the limit).
  - Tried and refused: `VICTORY_BVH_HOLD=1` holds back the 64 programs
    that walk ray-tracing structures nothing has built; the picture loses
    its indirect light with it, so it stays off.
  - The process opts out of the system's efficiency mode
    (`VICTORY_EFFICIENCY_MODE=1` leaves it); on a hybrid processor the
    frame log found the title's render and recording threads on fast
    cores in every look of a run, and the layer's submit thread on a slow
    core in 6 % of its looks.
  Still open: the per-record clamp is most of the card's time, and the
  copies between the PC's memory and the card's are most of the submit
  thread's; the cause of the one device loss with windows is not known.
  A first play session past the first rooms reached the open world and
  lost the device there with no driver entry (reason "device removed"):
  the card had no room for new textures from the opening cutscene on
  (9.2 of a 10.3 GB budget on a 12 GB card; 2,073 passes made no texture
  for lack of room; 44,613 textures let go and made again in 13 minutes).
  Cause of that removal not known either.
- Known and counted, not fixed yet: an indirect compute pass asks for
  about 77,500 thread groups in x where one dispatch runs 65,535 (the
  rest is cut off).

Memory sync between the title's memory and the copies on the card:
- **A write through one view of a physical page is seen by the copies taken
  through its other views** (`VICTORY_VIEW_GUARD=0` turns it off). The title
  shows one page at several addresses and a copy was guarded at one of them
  only. Measured on the test machine: 0.8 to 1.0 million such writes in a
  five-minute run, every one of which used to leave a copy stale. Since it
  went in, the first room had its walls in the later frames of 4 runs out of
  4 (2 of 5 before); the room's very first shot still comes up without walls
  in some runs.
- Kernel reads into title memory are read beside and copied in, so a block
  guarded again in between no longer fails the read.
- Writes of the storage layer into physical pages are counted where the title
  has those pages mapped (10,000 to 50,000 guarded blocks of copies lay under
  them in every run) and can open the guards there:
  `VICTORY_PAGE_WRITES=guards` (off by default; `1`, `slots`, `all` do more).
- Failed page-destination reads are retried (`VICTORY_DSTORAGE_RETRIES=0` = as before).

Card memory (the test card is full from the first frame of the world: 85 to
99 % of its budget for a whole run, about 26,000 texture copies let go and
3,400 made again in five minutes, 2.8 GB of render targets that are all in
use):
- Levels on demand, **off by default** (`VICTORY_LEVELS_ON_DEMAND=1`): a
  texture the title fills takes card memory only for the levels the title
  holds (2.2 to 3.0 GB of the copies were levels it does not hold). Its test
  runs on the software adapter; it has not run on a card yet.
- Refused texture slots are held back for a few seconds so that a pass
  reaches the slots behind them (`VICTORY_FULL_CARD_PASS=0` = as before);
  room can be kept for the card copies of title buffers
  (`VICTORY_CARD_ROOM_FOR_BUFFERS=1`, off).

Tools and diagnostics:
- `victory translate <shader file> --warm all --jobs N` translates every
  shader ahead of a run, on the processor only (about 4.5 to 6.5 minutes
  here); the shader cache is keyed on the translator's source, so a new
  build that does not touch the translator starts warm.
- Counters, all off or diagnostics-only: `VICTORY_MERGE_RACE=1` (title bytes
  a copy-back is about to put back to older values: none found in 138,000
  copy-backs), `VICTORY_WALLS_TRACE`, `VICTORY_CARD_ACCOUNTING`, and a census
  of the copies the texture sweep never lets go.

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

To install Rust, use rustup (`winget install --id Rustlang.Rustup -e`, or
the installer from https://rustup.rs). The first `cargo` command in this
repository then fetches the pinned toolchain. Building needs the MSVC C++
build tools: Visual Studio or its Build Tools with the "Desktop development
with C++" workload.

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

Set `VICTORY_WINDOW_MAX=1920x1080` before launching to limit the title's
top-level window to that bounding box. Smaller windows keep their size,
larger windows keep their proportions, and the monitor's work area still
limits them. This scales the displayed image; it does not change the
console render resolution or the swap-chain buffers. With the variable
unset, window sizing is unchanged.

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

Keyboard and mouse instead of controllers: set `VICTORY_KBM=1`. Pads, real and
virtual, are then left out, and the mouse turns the right stick. The title
itself has no mouse pointer, so its menus cannot be clicked: there the wheel
moves, the left button selects and the right one goes back.

| Variable | Effect |
|----------|--------|
| `VICTORY_KBM=1` | keyboard and mouse instead of controllers |
| `VICTORY_KEYBINDS=<file>` | bindings, one `key = action` per line under `[foot]`, `[drive]` and `[menu]`; saved changes take effect while the title runs. The bindings without a file are `DEFAULT` in `gameinput/kbm.rs` |
| `VICTORY_MOUSE=<n>` | mouse speed (1 unless set) |

It starts with the menu bindings. F10 switches between menu and play bindings,
Esc opens the menu and switches too, Caps Lock switches between foot and
drive, F9 lets go of the mouse.

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

### Windows shader-test validator

The shader tests use `dxil.dll` to validate Shader Model 6.6 containers.
An older Windows SDK validator can load successfully but reject them with
`Unknown shader model 'ps_6_6'`. If this happens, use a current x64 validator
from [Microsoft's DirectX Shader Compiler releases](https://github.com/microsoft/DirectXShaderCompiler/releases)
or a current Windows SDK. Set its path for the test session:

```powershell
$env:VICTORY_DXIL = 'C:\tools\dxc\bin\x64\dxil.dll'
cargo test -p victory-shader --lib --locked -- --test-threads=1
```

Without this override, the validator loader selects the newest installed
Windows SDK's `bin/<version>/x64/dxil.dll`, then falls back to the DLL search
path when no SDK copy is found. This is a test/offline-validation dependency;
the translated shaders do not require it at runtime.

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

The on/off switches below (`...=1`) are off when unset or set to `0`.

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
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
