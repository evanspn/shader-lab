# shaderlab

Render, preview and check Ghostty / Shadertoy-style terminal shaders **headlessly**.
It draws your shader over a synthetic terminal frame (text, a selection, a cursor)
on the GPU, writes PNGs, and runs objective checks so you can tell without a
terminal window whether a shader compiles, keeps text readable, really animates
and moves the way it says it does.

```
shaderlab render FILE         one frame to a PNG
shaderlab contact-sheet FILE  a grid: one row per preset, one column per time
shaderlab video FILE          many frames in one GPU session: mp4 (ffmpeg), GIF or PNG frames
shaderlab preview FILE        live in the terminal (Ratatui; kitty graphics / sixel / half-blocks), with hot reload; --window for a window
shaderlab check FILE...       objective checks; exit code 1 if any fails
```

## Install

```
cargo install --git https://github.com/evanspn/shader-lab
```

Needs a GPU adapter (Metal on macOS, Vulkan/GL on Linux, DX12/Vulkan on Windows).
With no adapter `render` / `check` exit with a clear error (never a panic);
`check --cpu-only` still validates that shaders compile.

## Orientation: read this first

The two conventions are opposites, and getting it wrong makes rain fall upward.

| | origin of `fragCoord` | y grows | "top of the screen" is |
|---|---|---|---|
| **Ghostty** | top-left | **down** | `fragCoord.y = 0` |
| **Shadertoy** | bottom-left | **up** | `fragCoord.y = iResolution.y` |

Output PNGs always have row 0 at the top. `--origin top-left` (the default) runs
the shader the way Ghostty does; `--origin bottom-left` runs it as written for
Shadertoy. In a generated header `gp_yup()` tells a shader which one is active.
`check` verifies a declared `@motion` against the motion it measures in the
picture, in the origin you chose.

## Video

```
shaderlab video FILE [--preset X] [--set k=v]... [--size 1280x720] [--fps 24] [--duration 10] [--start 0]
                     [--text sample|PATH.png] [--origin top-left|bottom-left]
                     [--format mp4|gif|frames] [--loop-seamless] [--out out.mp4]
```

All frames are rendered in one GPU session (the device and pipeline are created once; each frame only updates `iTime`,
`iTimeDelta` and `iFrame`), so 10 seconds at 720p takes a couple of seconds. Frame N has `iTime = start + N/fps`, and `iFrame`
counts output frames from 0.

* **mp4** streams the frames to `ffmpeg` (H.264, `yuv420p`, `+faststart`, an odd size is trimmed to even) so it plays on an iPhone and in
  iMessage. **This needs ffmpeg**: `brew install ffmpeg`.
* **gif** is the fallback when ffmpeg is not installed (or with `--format gif`): palette-quantized, capped at 20 fps and 640 px wide, and
  it says so.
* **frames** writes `frame-00000.png`... into the `--out` folder.
* `--format` is picked from the output extension, else mp4 when ffmpeg is installed, else gif.
* `--loop-seamless` cross-fades the last second into the first so the clip loops without a jump.

```
shaderlab video examples/shaders/rain-down.glsl --duration 10 --out rain.mp4
shaderlab video my.glsl --preset storm --size 1290x2796 --text blank-frame.png --out phone.mp4
SHADERLAB_NO_FFMPEG=1 shaderlab video my.glsl --out quick.gif      # force the GIF path
```

## Live preview (in the terminal)

```
shaderlab preview FILE [--preset X] [--set k=v]... [--protocol auto|kitty|sixel|halfblocks] [--fps 30] [--text sample|PATH.png] [--origin ...]
shaderlab preview FILE --window     # a separate window instead (winit)
```

`preview` is a [Ratatui](https://ratatui.rs) UI inside your terminal: the shader running on the GPU over the synthetic terminal
frame takes most of the screen, and a side panel shows the name, preset, time, fps/ms, every parameter (colors with a swatch and hex,
numbers with a bar) and the presets. Save the shader in your editor and it reloads; a compile error appears in the panel while the
last good shader keeps running.

**How the picture reaches the terminal** (`--protocol auto` detects from the environment):

| protocol | when | notes |
| --- | --- | --- |
| `kitty` | Ghostty, kitty, WezTerm | the kitty graphics protocol: the frame is rendered at the pane's pixel size (capped at 960x540) and written to a temp file the terminal reads and deletes (`--kitty-transfer file`, the default when the terminal is on this machine; over ssh it falls back to zlib-compressed base64, capped at 640x360). Every frame uses the same image id AND placement id so the terminal replaces the picture in place, and each frame is one synchronized update (DEC 2026), so the cells and the picture change together. Nothing is deleted between frames; the image is deleted on exit. |
| `sixel` | foot, mlterm, iTerm2, or `TERM` naming sixel | 216-color cube, capped at 640x360. |
| `halfblocks` | everything else | truecolor `▀` characters (two pixels per cell, rendered 4x finer and averaged down). Works anywhere; text in the sample frame is blurry at cell resolution. |

The loop runs on a fixed 30 fps clock (`--fps`: 15, 24 and 60 also work). A frame it cannot keep up with is dropped, never answered with a burst of catch-up frames, and the panel shows the dropped-frame count.

Inside **tmux** the graphics protocols need passthrough (`set -g allow-passthrough on`), so tmux gets half-blocks unless you force
`--protocol kitty`. Detection is by environment variables, not by asking the terminal.

| key | does |
| --- | --- |
| space | pause / resume |
| `[` `]` | slower / faster |
| `R` | reset time to 0 |
| `T` | terminal frame / plain background |
| `p` / `P` | next / previous preset (and the defaults) |
| `O` | hide / show the effect (sets `opacity` to 0 and back) |
| up / down, `1`-`9` | pick a parameter |
| left / right (shift = bigger) | change it: numbers by a 20th of their range, colors by 15 degrees of hue |
| Enter | on a color: open the picker (hue / saturation / brightness bars; left/right or click and drag; Enter accepts, Esc cancels; the shader updates live) |
| `S` | save a PNG (`NAME-preview-N.png`) |
| `V` | record 5 seconds (mp4 with ffmpeg, else GIF) |
| `Q` / Ctrl-C | quit |

The mouse works too: click or drag a number's bar, click a color's swatch to open the picker, click a preset.
The terminal is restored (alternate screen, mouse, cursor, kitty image) on quit, Ctrl-C and panic. With no GPU it exits with a
clear error. Build without the terminal UI or the window with `--no-default-features --features render`.

## A living background: `shaderlab pane`

```
shaderlab pane FILE [--preset X] [--set k=v]... [--fps 30] [--scale auto|0.25..1.0] [--stats] [--pause-unfocused]
                    [--protocol auto|kitty|sixel|halfblocks] [--text none|sample] [--time-wrap SECONDS] [--log FILE]
```

The shader fills the WHOLE terminal pane: no side panel, no footer, no border, the cursor hidden. Leave it running in a split next to
your work:

1. In Ghostty, split the window (`cmd+d` for a split to the right, `cmd+shift+d` below).
2. In the new pane: `shaderlab pane ps3-visualizer.glsl --preset ps3-classic` (or any shader from `gpf` / `examples/shaders`).

Keys (no input is needed): `q` / Esc / Ctrl-C quit, `p` or space pause, `n` / `N` next / previous preset, `s` save a PNG (to
`$SHADERLAB_HOME/renders`), `?` a three-second help line. The pane follows resizes (the picture is re-rendered at the new size and
replaces the old one in place, no flash) and works at any shape, wide, tall or tiny. The terminal is restored, the image deleted
and the temp files removed on exit, Ctrl-C or panic; frame files left by a pane that was killed are cleaned at the next start.

**Smoothness.** Frames are drawn straight to RGBA8 and read back through a ring of three staging buffers without ever waiting on
the frame just submitted; a finished frame goes to a temp file the terminal reads (`t=t`, raw RGBA, one image id and one placement id
replaced in place, inside one synchronized update). A fixed-step clock drops a frame it cannot keep up with and never answers it with a
burst. **Adaptive quality** (`--scale auto`, the default) starts near 1080p and watches the 95th-percentile frame cost: over 85% of the
frame budget it lowers the render scale (1.0, 0.75, 0.5, 0.35 of the start), then the frame rate (30, 24, 18, 15), and after 10 s of
clear headroom it raises them again, frame rate first. `--stats` shows the size, scale, fps, p95 and dropped frames; `--log FILE`
appends a line per second (seconds, fps, p95 ms, scale, fps target, dropped, bytes per frame, RSS KB). `--pause-unfocused` stops
rendering while the pane or window is not focused (focus reporting, `CSI ? 1004 h`). `--time-wrap SECONDS` restarts `iTime`
from 0 after that long, for shaders that are not written to run for days.

## Regression checks: `shaderlab regress`

```
shaderlab regress [FILE|DIR...] [--only golden,orient,text,coverage,temporal,perf] [--fast] [--update]
scripts/regress.sh [--fast|--update]       # regress + cargo test, exit non-zero on any failure
scripts/install-hooks.sh                   # opt in: a pre-push hook that runs the fast subset
```

Every fix and every optimization is protected so a later change cannot quietly undo it. For each shader, each preset and each
`scene` lock it checks: **golden** (the picture at a fixed time over the synthetic terminal frame against `tests/golden/<shader>/*.png`:
mean and worst-16x16-block difference), **orient** (the same at 16:9, 4:3, 1:1, 9:16 and 3:1, so a flipped, stretched or rotated scene
shows), **text** (text pixels unchanged), **coverage** and **flat** (how much of the frame it draws and its largest single-colour block,
against the accepted values stored beside the golden), **temporal** (no frame pops, no lurch in average brightness, no step at the
usual time-wrap moments) and **perf** (p50 / p95 ms per 1080p frame against `perf-baseline.json`, only on the machine it was recorded
on; elsewhere it is skipped and says so). `--fast` is the quick subset: no perf, a short temporal run, no wrap probes.

**Updating goldens deliberately.** When a look changes on purpose, run `shaderlab regress --update`: it rewrites the goldens, the
accepted values and the baseline and prints what changed (`changed: mean 3.1, worst block 40.2`, `new`, or `unchanged`). Review the
images in the diff, then commit. Goldens are only the shader's output over the SYNTHETIC frame, kept tiny, and are the only images the
repo allows (`.gitignore` and `scripts/privacy-scan.sh` permit exactly `tests/golden/`).

CI (`.github/workflows/ci.yml`) runs only what needs no GPU: format, clippy, the build, the unit and parser tests and the privacy scan.
The GPU checks run locally with `scripts/regress.sh`. The test suite proves each class catches what it should: a changed colour, a
flipped scene, a flat block, a slowed shader, a popping frame, a lurch in brightness, painted-over text, and a leaked process or temp file
each FAIL on a deliberately broken variant (`tests/regress.rs`, `tests/pane_pty.rs`).

## Commands

```
shaderlab render FILE [--preset NAME] [--set name=value]... [--time 5]
                      [--size 1280x720] [--text sample|PATH.png]
                      [--origin top-left|bottom-left] [--out FILE.png]
shaderlab contact-sheet FILE [--times 0,2,5] [--presets all] [--size 320x180] [--out FILE.png]
shaderlab pane FILE [--fps 30] [--scale auto] [--stats]
shaderlab regress [FILE|DIR...] [--fast] [--update] [--only ...]
shaderlab check FILE|DIR... [--origin ...] [--epsilon 2] [--sharpness 0.85]
                            [--budget-ms 16.7] [--skip text,motion,animation,perf]
                            [--size WxH] [--text sample|PATH.png] [--cpu-only]
```

PNGs go where you say (`--out`), by default the current directory; nothing is
ever written next to your shaders.

```
shaderlab render examples/shaders/rain-down.glsl --time 3 --set density=0.3 --out rain.png
shaderlab contact-sheet examples/shaders/rain-down.glsl --times 0,2,5 --presets all --out sheet.png
shaderlab render examples/shaders/shadertoy-glow.glsl --origin bottom-left --out glow.png
shaderlab check examples/shaders/vignette.glsl examples/shaders/rain-down.glsl
shaderlab check examples/shaders          # the good ones pass; the fixtures fail on purpose
```

### What `check` verifies

| check | passes when |
|---|---|
| compiles | the shader compiles with defaults and every preset (errors name the line of **your** file) |
| finite | defaults and extreme parameter values produce no NaN / infinity |
| not blank | no variant paints an all-black or all-white frame |
| text | text pixels change by at most `--epsilon` (of 255) and the edge energy of the text is at least `--sharpness` of the original (catches blur and wash-over) |
| animation | the picture at `t` and `t+1s` differs if the shader uses `iTime`, is identical if it does not, and the same time gives the same picture |
| motion | a declared `@motion down/up/left/right/radial/none` matches the direction measured between frames |
| speed | reports ms per 1080p frame; fails above `--budget-ms` |

## Shader annotations

The format is identical to [ghostty-profiles](https://github.com/evanspn/ghostty-profiles):
comments at the top of the file declare tunable parameters.

```glsl
// @color name #hex "Label"            a colour parameter
// @float name default min max "Label" a number parameter
// @preset name key=value key=value    a named set of values
// @motion down                        none | down | up | left | right | radial
// @coverage full                      the effect spans the screen (checked against a budget)
// @float opacity 0.7 0.0 1.0 "Opacity"   scales the whole effect (mix of terminal and effect)
```

Parameters are substituted exactly like the real apply path: before your code
the tool inserts constants

* `P_<name>`: `vec3` for colours, `float` for numbers (e.g. `P_rain`, `P_density`)
* `P_bg`, `P_bg_image`: the profile background colour and whether it is an image
* `gp_textMask(fragCoord, term)` (0 on background, 1 on text; use `1.0 - gp_textMask(...)` to stay **behind** the text), with helpers `gp_dist` and `gp_isInk`
* `gp_yup()`: true when the origin is bottom-left

and `opacity` is applied by a generated wrapper around your `mainImage`.
Only validated numbers ever reach the GLSL source.

Plain Shadertoy shaders (no annotations, `mainImage(out vec4, in vec2)`, `iResolution`,
`iTime`, `iChannel0` = the terminal) work as they are.

## Limits

* It cannot prove how the real Ghostty renders a shader: Ghostty compiles the
  GLSL with its own pipeline (SPIR-V cross-compiled), and a shader that passes
  here can still behave differently or fail to compile there. Compile errors
  are not shown by Ghostty, so a final look in a real window is still wise.
* The terminal frame is synthetic (generic text, built-in 5x7 bitmap font), so
  text checks measure preservation, not how your own fonts would look.
* Motion measurement is a correlation over short intervals: it classifies a
  direction, it does not prove the effect is pretty.
* GPU tests skip with a message when there is no adapter; a skip is not a pass.

## Examples

`examples/shaders/` holds small original shaders (`vignette`, `rain-down`,
`shadertoy-glow`) and deliberately bad fixtures `check` must fail
(`blurry-wash`, `rain-wrong-way`, `claims-to-animate`, `broken`).

## As a library

`shaderlab` is also a crate: `params` (annotations and substitution), `frame`,
`gpu`, `check`, `sheet`.

## License

MIT, see [LICENSE](LICENSE).
