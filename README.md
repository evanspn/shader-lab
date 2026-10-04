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
shaderlab preview FILE        a live window with hot reload and keys
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

## Live preview

```
shaderlab preview FILE [--preset X] [--set k=v]... [--size 1280x720] [--text sample|PATH.png] [--origin ...]
```

A window shows the shader running in real time over the synthetic terminal frame. Save the file in your editor and it reloads; a
compile error shows in the window title and in the terminal while the last good shader keeps running.

| key | does |
| --- | --- |
| space | pause / resume |
| `[` `]` | slower / faster |
| `R` | reset time to 0 |
| `T` | terminal frame / plain background |
| `P` | cycle the presets (and back to the defaults) |
| `1`-`9`, up/down | pick a parameter (prints its name and value) |
| left/right | change the picked parameter (numbers by a 20th of their range, colours by 15 degrees of hue) |
| `S` | save a PNG (`NAME-preview-N.png`) |
| `V` | record 5 seconds (mp4 with ffmpeg, else GIF) |
| `Q` / Esc | quit |

The title shows the preset, time, speed and frames per second. With no display or GPU it exits with a clear error. The preview is
on by default; to build without the window libraries use `cargo install --git https://github.com/evanspn/shader-lab --no-default-features --features render`.

## Commands

```
shaderlab render FILE [--preset NAME] [--set name=value]... [--time 5]
                      [--size 1280x720] [--text sample|PATH.png]
                      [--origin top-left|bottom-left] [--out FILE.png]
shaderlab contact-sheet FILE [--times 0,2,5] [--presets all] [--size 320x180] [--out FILE.png]
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
