//! Tunable shader parameters: the `@color` / `@float` / `@preset` / `@motion` / `@coverage` annotation format.
//!
//! A shader marks the values that can be tuned with annotation comments:
//!
//! ```text
//! // @motion down                                  (down | up | left | right | radial | none, as seen on screen)
//! // @float opacity 0.6 0.0 1.0 "Opacity"
//! // @color wave_a #ff6b1a "Wave color"
//! // @float strength 0.16 0.0 0.5 "Strength"        (default, min, max)
//! // @preset ocean wave_a=#2fa8ff strength=0.18
//! ```
//!
//! and uses them as `P_opacity`, `P_wave_a` (a `vec3`) and `P_strength` (a `float`). [`render_ctx`] writes a header of
//! `const` declarations built from the chosen values at the top of the shader (plus `P_bg`, `gp_textMask`, `gp_yup`),
//! and, when the shader has an `opacity` parameter, wraps its `mainImage`. This is the format `ghostty-profiles` uses, so
//! shaders written for it work unchanged here.
//!
//! Nothing a user types is ever pasted into GLSL as text: each value is parsed (a hex color, or a finite number inside the
//! declared range) and the header is built from the parsed numbers, so a value can only ever choose numbers.

use std::collections::BTreeMap;

/// `#F60` / `ff6a00` / `"#FF6A00"` -> `#ff6a00`; `None` if it is not a color.
pub fn normalize_hex(value: &str) -> Option<String> {
    let v = value.trim().trim_matches('"');
    let v = v.strip_prefix('#').unwrap_or(v);
    if !v.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match v.len() {
        3 => Some(format!(
            "#{}",
            v.chars()
                .flat_map(|c| [c, c])
                .collect::<String>()
                .to_lowercase()
        )),
        6 => Some(format!("#{}", v.to_lowercase())),
        _ => None,
    }
}

/// `#rrggbb` -> (r, g, b).
pub fn hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let h = normalize_hex(hex)?;
    let n = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((n(1)?, n(3)?, n(5)?))
}

pub const MAX_PARAMS: usize = 16;
pub const MAX_PRESETS: usize = 16;
const BEGIN: &str = "// ==== ghostty-profiles parameters (generated from the profile's .params file; do not edit) ====";
const END: &str = "// ==== end of generated parameters ====";
const FOOTER_BEGIN: &str = "// ==== ghostty-profiles opacity wrapper (generated; do not edit) ====";
const FOOTER_END: &str = "// ==== end of opacity wrapper ====";

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Color,
    Float { min: f64, max: f64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    pub label: String,
    pub kind: Kind,
    /// Canonical default (`#rrggbb`, or a number as text).
    pub default: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub name: String,
    /// (param name, canonical value) for the params the preset sets; others keep their default.
    pub values: Vec<(String, String)>,
}

/// The direction a shader's effect is meant to move, ON SCREEN as the user sees it.
/// (Ghostty's `fragCoord` has its origin at the top-left with y pointing DOWN, so "down" is +y.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Down,
    Up,
    Left,
    Right,
    Radial,
    None,
}

impl Motion {
    pub fn name(self) -> &'static str {
        match self {
            Motion::Down => "down",
            Motion::Up => "up",
            Motion::Left => "left",
            Motion::Right => "right",
            Motion::Radial => "radial",
            Motion::None => "none",
        }
    }

    fn parse(s: &str) -> Option<Motion> {
        [
            Motion::Down,
            Motion::Up,
            Motion::Left,
            Motion::Right,
            Motion::Radial,
            Motion::None,
        ]
        .into_iter()
        .find(|m| m.name() == s)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schema {
    pub params: Vec<Param>,
    pub presets: Vec<Preset>,
    /// `// @motion down|up|left|right|radial|none`: the declared direction of the effect.
    pub motion: Option<Motion>,
    /// `// @coverage full`: the effect is meant to span the screen (waves, ribbons), so it is exempt from the
    /// "does not block the screen" budget that particle effects must meet.
    pub coverage_full: bool,
}

/// The name of the universal parameter that scales how strongly the effect shows (never the terminal's text).
pub const OPACITY: &str = "opacity";

fn valid_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        && n.len() <= 32
}

/// Preset names are labels, not GLSL identifiers, so they may also contain `-`.
fn valid_preset_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
        && n.len() <= 32
}

/// A float as short, stable text (4 decimals at most), e.g. `0.16`, `1`, `0.3333`.
pub fn format_number(n: f64) -> String {
    let r = (n * 10000.0).round() / 10000.0;
    let r = if r == 0.0 { 0.0 } else { r }; // no "-0"
    format!("{r}")
}

/// Check `text` for `param`: a color must be hex, a float must be a finite number in range.
/// Returns the canonical text to store.
pub fn validate_value(param: &Param, text: &str) -> Result<String, String> {
    let t = text.trim();
    match param.kind {
        Kind::Color => normalize_hex(t).ok_or_else(|| format!("'{t}' is not a color: use #rrggbb")),
        Kind::Float { min, max } => match t.parse::<f64>() {
            Ok(n) if n.is_finite() && n >= min && n <= max => Ok(format_number(n)),
            Ok(_) => Err(format!("must be between {min} and {max}")),
            Err(_) => Err(format!("'{t}' is not a number")),
        },
    }
}

fn label_and_rest(line: &str) -> Result<(&str, String), String> {
    // `... "Label"`: the label is the last quoted text; what is before it is the arguments
    let Some(first) = line.find('"') else {
        return Ok((line, String::new()));
    };
    let Some(last) = line.rfind('"').filter(|l| *l > first) else {
        return Err("unterminated label".into());
    };
    let label = &line[first + 1..last];
    if label.chars().any(char::is_control) || label.len() > 40 {
        return Err("the label must be plain text of at most 40 characters".into());
    }
    if !line[last + 1..].trim().is_empty() {
        return Err("nothing may follow the label".into());
    }
    Ok((line[..first].trim(), label.to_string()))
}

/// Read the `@color` / `@float` / `@preset` annotations of a shader. Any malformed annotation is an
/// error naming its line, so a typo is never silently ignored.
pub fn parse_schema(src: &str) -> Result<Schema, String> {
    let mut schema = Schema::default();
    type RawPreset = (usize, String, Vec<(String, String)>);
    let mut raw_presets: Vec<RawPreset> = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let n = i + 1;
        let t = raw.trim();
        let Some(rest) = t.strip_prefix("//") else {
            continue;
        };
        let rest = rest.trim();
        let (kind, args) = match rest.split_once(char::is_whitespace) {
            Some((k, a)) if k.starts_with('@') => (k, a.trim()),
            _ => continue,
        };
        let err = |m: String| format!("line {n}: {m}");
        match kind {
            "@color" => {
                let (head, label) = label_and_rest(args).map_err(&err)?;
                let toks: Vec<&str> = head.split_whitespace().collect();
                let [name, hex] = toks[..] else {
                    return Err(err("expected: @color NAME #rrggbb \"Label\"".into()));
                };
                if !valid_name(name) {
                    return Err(err(format!(
                        "bad name '{name}' (lowercase letters, digits, _)"
                    )));
                }
                let default =
                    normalize_hex(hex).ok_or_else(|| err(format!("'{hex}' is not a color")))?;
                schema.params.push(Param {
                    name: name.into(),
                    label: if label.is_empty() { name.into() } else { label },
                    kind: Kind::Color,
                    default,
                });
            }
            "@float" => {
                let (head, label) = label_and_rest(args).map_err(&err)?;
                let toks: Vec<&str> = head.split_whitespace().collect();
                let [name, def, min, max] = toks[..] else {
                    return Err(err("expected: @float NAME DEFAULT MIN MAX \"Label\"".into()));
                };
                if !valid_name(name) {
                    return Err(err(format!(
                        "bad name '{name}' (lowercase letters, digits, _)"
                    )));
                }
                let num = |s: &str| {
                    s.parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| err(format!("'{s}' is not a number")))
                };
                let (def, min, max) = (num(def)?, num(min)?, num(max)?);
                if min >= max {
                    return Err(err(format!("min {min} must be below max {max}")));
                }
                if def < min || def > max {
                    return Err(err(format!("default {def} is outside {min}..{max}")));
                }
                schema.params.push(Param {
                    name: name.into(),
                    label: if label.is_empty() { name.into() } else { label },
                    kind: Kind::Float { min, max },
                    default: format_number(def),
                });
            }
            "@motion" => {
                let word = args.trim();
                schema.motion = Some(Motion::parse(word).ok_or_else(|| {
                    err(format!(
                        "unknown motion '{word}' (down, up, left, right, radial, none)"
                    ))
                })?);
            }
            "@coverage" => {
                if args.trim() != "full" {
                    return Err(err(format!(
                        "unknown coverage '{}' (the only value is: full)",
                        args.trim()
                    )));
                }
                schema.coverage_full = true;
            }
            "@preset" => {
                let mut toks = args.split_whitespace();
                let Some(name) = toks.next() else {
                    return Err(err("expected: @preset NAME key=value ...".into()));
                };
                if !valid_preset_name(name) {
                    return Err(err(format!("bad preset name '{name}'")));
                }
                let mut values = Vec::new();
                for tok in toks {
                    let Some((k, v)) = tok.split_once('=') else {
                        return Err(err(format!("'{tok}' is not key=value")));
                    };
                    values.push((k.to_string(), v.to_string()));
                }
                raw_presets.push((n, name.to_string(), values));
            }
            _ => return Err(err(format!("unknown annotation {kind}"))),
        }
    }
    if schema.params.len() > MAX_PARAMS {
        return Err(format!("too many parameters (at most {MAX_PARAMS})"));
    }
    for (i, p) in schema.params.iter().enumerate() {
        if schema.params[..i].iter().any(|q| q.name == p.name) {
            return Err(format!("parameter '{}' is declared twice", p.name));
        }
    }
    // presets are checked against the declared parameters
    for (n, name, kvs) in raw_presets {
        if schema.presets.iter().any(|p| p.name == name) {
            return Err(format!("line {n}: preset '{name}' is declared twice"));
        }
        let mut values = Vec::new();
        for (k, v) in kvs {
            let Some(param) = schema.params.iter().find(|p| p.name == k) else {
                return Err(format!(
                    "line {n}: preset '{name}' sets unknown parameter '{k}'"
                ));
            };
            let canon = validate_value(param, &v)
                .map_err(|m| format!("line {n}: preset '{name}', {k}: {m}"))?;
            values.push((k, canon));
        }
        schema.presets.push(Preset { name, values });
    }
    if schema.presets.len() > MAX_PRESETS {
        return Err(format!("too many presets (at most {MAX_PRESETS})"));
    }
    Ok(schema)
}

/// The sidecar's `name = value` lines (comments and blank lines are skipped). Values are NOT
/// validated here; [`resolve`] does that against the schema.
pub fn parse_values(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines().take(256) {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            m.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    m
}

/// One value per declared parameter, in order: the sidecar's value when it is valid, else the default.
pub fn resolve(schema: &Schema, values: &BTreeMap<String, String>) -> Vec<String> {
    schema
        .params
        .iter()
        .map(|p| {
            values
                .get(&p.name)
                .and_then(|v| validate_value(p, v).ok())
                .unwrap_or_else(|| p.default.clone())
        })
        .collect()
}

/// The sidecar text for resolved values (all of them, in schema order).
pub fn format_values(schema: &Schema, resolved: &[String]) -> String {
    let mut out = String::from(
        "# Shader parameters, edited from the ghostty-profiles TUI. One `name = value` per line.\n",
    );
    for (p, v) in schema.params.iter().zip(resolved) {
        out.push_str(&format!("{} = {v}\n", p.name));
    }
    out
}

/// The preset's values laid over the defaults, as resolved values.
pub fn preset_values(schema: &Schema, preset: &Preset) -> Vec<String> {
    schema
        .params
        .iter()
        .map(|p| {
            preset
                .values
                .iter()
                .find(|(k, _)| *k == p.name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| p.default.clone())
        })
        .collect()
}

/// The preset (if any) whose values equal `resolved` exactly.
pub fn matching_preset<'a>(schema: &'a Schema, resolved: &[String]) -> Option<&'a Preset> {
    schema
        .presets
        .iter()
        .find(|p| preset_values(schema, p) == resolved)
}

fn glsl_float(n: f64) -> String {
    format!("{n:.6}")
}

/// What a shader's header needs to know about the profile it is rendered for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderContext {
    /// Multiplies the shader's own `opacity` (the profile-wide effects opacity), 0..1.
    pub opacity_scale: f64,
    /// The terminal's background color: what a pixel with nothing drawn on it looks like.
    pub background: (u8, u8, u8),
    /// The profile has a background image, so the background is a picture, not one flat color.
    pub background_image: bool,
}

/// Ghostty's default background (`#282c34`), for a profile that sets none.
pub const DEFAULT_BACKGROUND: (u8, u8, u8) = (0x28, 0x2c, 0x34);

impl Default for RenderContext {
    fn default() -> Self {
        RenderContext {
            opacity_scale: 1.0,
            background: DEFAULT_BACKGROUND,
            background_image: false,
        }
    }
}

/// The text mask every effect uses to stay BEHIND the text. It is 1.0 where the terminal drew anything that
/// is not plain background (text of any color, the cursor, a selection, inverse video) and on a thin fringe
/// next to it, one to two pixels wide (so anti-aliased edges are covered), and 0.0 on plain background. The only neighbour reads are
/// twelve taps within 2 px (the 8 neighbours, and 4 axial ones 2 px out) and they feed the mask alone: a color is never blended with its neighbours.
/// With a background image the background is not one color, so a pixel is compared with a local estimate of
/// the picture taken from four wide taps instead.
const MASK_GLSL: &str = "\
float gp_dist(vec3 a, vec3 b) { vec3 d = abs(a - b); return max(max(d.r, d.g), d.b); }
float gp_isInk(vec4 c, vec3 ref) {
    // far from the background (either as is, or premultiplied by the window's opacity)
    return min(gp_dist(c.rgb, ref), gp_dist(c.rgb, ref * c.a));
}
float gp_textMask(vec2 fragCoord, vec4 term) {
    vec2 px = 1.0 / iResolution.xy;
    vec2 uv = fragCoord * px;
    // taps on pixel centres: the 8 neighbours at 1 px and the 4 axial ones at 2 px, so a 1-2 px fringe is covered
    vec4 a0 = texture(iChannel0, uv + vec2( 1.0,  0.0) * px);
    vec4 a1 = texture(iChannel0, uv + vec2(-1.0,  0.0) * px);
    vec4 a2 = texture(iChannel0, uv + vec2( 0.0,  1.0) * px);
    vec4 a3 = texture(iChannel0, uv + vec2( 0.0, -1.0) * px);
    vec4 a4 = texture(iChannel0, uv + vec2( 1.0,  1.0) * px);
    vec4 a5 = texture(iChannel0, uv + vec2(-1.0,  1.0) * px);
    vec4 a6 = texture(iChannel0, uv + vec2( 1.0, -1.0) * px);
    vec4 a7 = texture(iChannel0, uv + vec2(-1.0, -1.0) * px);
    vec4 b0 = texture(iChannel0, uv + vec2( 2.0,  0.0) * px);
    vec4 b1 = texture(iChannel0, uv + vec2(-2.0,  0.0) * px);
    vec4 b2 = texture(iChannel0, uv + vec2( 0.0,  2.0) * px);
    vec4 b3 = texture(iChannel0, uv + vec2( 0.0, -2.0) * px);
    if (P_bg_image > 0.5) {
        // a picture behind the text: compare with a local estimate of the picture from four wide taps
        vec3 ref = 0.25 * (texture(iChannel0, uv + vec2( 14.0, 0.0) * px).rgb + texture(iChannel0, uv + vec2(-14.0, 0.0) * px).rgb
                         + texture(iChannel0, uv + vec2(0.0,  14.0) * px).rgb + texture(iChannel0, uv + vec2(0.0, -14.0) * px).rgb);
        float d = gp_dist(term.rgb, ref);
        d = max(d, max(max(gp_dist(a0.rgb, ref), gp_dist(a1.rgb, ref)), max(gp_dist(a2.rgb, ref), gp_dist(a3.rgb, ref))));
        d = max(d, max(max(gp_dist(a4.rgb, ref), gp_dist(a5.rgb, ref)), max(gp_dist(a6.rgb, ref), gp_dist(a7.rgb, ref))));
        d = max(d, max(max(gp_dist(b0.rgb, ref), gp_dist(b1.rgb, ref)), max(gp_dist(b2.rgb, ref), gp_dist(b3.rgb, ref))));
        return smoothstep(0.08, 0.16, d);
    }
    float d = max(gp_isInk(term, P_bg), max(max(gp_isInk(a0, P_bg), gp_isInk(a1, P_bg)), max(gp_isInk(a2, P_bg), gp_isInk(a3, P_bg))));
    d = max(d, max(max(gp_isInk(a4, P_bg), gp_isInk(a5, P_bg)), max(gp_isInk(a6, P_bg), gp_isInk(a7, P_bg))));
    d = max(d, max(max(gp_isInk(b0, P_bg), gp_isInk(b1, P_bg)), max(gp_isInk(b2, P_bg), gp_isInk(b3, P_bg))));
    return smoothstep(0.02, 0.06, d);
}
";

/// The generated header: `const` declarations built only from parsed numbers.
pub fn header(schema: &Schema, resolved: &[String], ctx: &RenderContext) -> String {
    let mut out = format!("{BEGIN}\n");
    for (p, v) in schema.params.iter().zip(resolved) {
        match p.kind {
            Kind::Color => {
                let (r, g, b) = hex_rgb(v)
                    .or_else(|| hex_rgb(&p.default))
                    .unwrap_or((0, 0, 0));
                let f = |c: u8| glsl_float(c as f64 / 255.0);
                out.push_str(&format!(
                    "const vec3 P_{} = vec3({}, {}, {});\n",
                    p.name,
                    f(r),
                    f(g),
                    f(b)
                ));
            }
            Kind::Float { min, max } => {
                let n = v
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .unwrap_or(min)
                    .clamp(min, max);
                out.push_str(&format!("const float P_{} = {};\n", p.name, glsl_float(n)));
            }
        }
    }
    let f = |c: u8| glsl_float(c as f64 / 255.0);
    out.push_str(&format!(
        "const vec3 P_bg = vec3({}, {}, {});\n",
        f(ctx.background.0),
        f(ctx.background.1),
        f(ctx.background.2)
    ));
    out.push_str(&format!(
        "const float P_bg_image = {};\n",
        if ctx.background_image { "1.0" } else { "0.0" }
    ));
    out.push_str(MASK_GLSL);
    if has_opacity(schema) {
        // the shader's own mainImage becomes gp_effect; the generated footer wraps it (see `render`)
        out.push_str("#define mainImage gp_effect\n");
    }
    out.push_str(
        "// Ghostty's fragCoord has its origin at the TOP-left, so y grows DOWNWARD. gp_yup() gives the more\n\
         // familiar y-UP coordinates (origin bottom-left, up = +y, falling = -y) for effects with a direction.\n\
         vec2 gp_yup(vec2 fragCoord) { return vec2(fragCoord.x, iResolution.y - fragCoord.y); }\n",
    );
    out.push_str(END);
    out.push('\n');
    out
}

/// Does the shader declare the universal `opacity` parameter (a float)?
pub fn has_opacity(schema: &Schema) -> bool {
    schema
        .params
        .iter()
        .any(|p| p.name == OPACITY && matches!(p.kind, Kind::Float { .. }))
}

fn footer() -> String {
    format!(
        "{FOOTER_BEGIN}\n#undef mainImage\nvoid mainImage(out vec4 fragColor, in vec2 fragCoord) {{\n    \
         vec4 gp_base = texture(iChannel0, fragCoord / iResolution.xy);\n    vec4 gp_fx;\n    gp_effect(gp_fx, fragCoord);\n    \
         fragColor = vec4(mix(gp_base.rgb, gp_fx.rgb, P_opacity), gp_fx.a);\n}}\n{FOOTER_END}\n"
    )
}

/// The shader without a previously generated header (and opacity wrapper).
pub fn strip_header(src: &str) -> &str {
    let mut body = src;
    if body.starts_with(BEGIN)
        && let Some(i) = body.find(END)
    {
        let after = &body[i + END.len()..];
        body = after.strip_prefix('\n').unwrap_or(after);
    }
    if let Some(i) = body.rfind(FOOTER_BEGIN)
        && body[i..].contains(FOOTER_END)
    {
        body = &body[..i];
    }
    body
}

/// Does this text have any annotation (so it is worth parsing)?
pub fn has_annotations(src: &str) -> bool {
    src.lines().any(|l| {
        let t = l.trim().strip_prefix("//").map(str::trim);
        t.is_some_and(|t| {
            ["@color ", "@float ", "@preset ", "@motion ", "@coverage "]
                .iter()
                .any(|k| t.starts_with(k))
        })
    })
}

/// The shader with its header (and, when it has an `opacity` parameter, its opacity wrapper) regenerated
/// from `values`. A shader with no parameters is returned unchanged; one whose annotations are
/// malformed is an error (and is left alone by callers).
pub fn render(src: &str, values: &BTreeMap<String, String>) -> Result<String, String> {
    render_scaled(src, values, 1.0)
}

/// Like [`render`], with the shader's opacity multiplied by `scale` (the profile's master opacity).
pub fn render_scaled(
    src: &str,
    values: &BTreeMap<String, String>,
    scale: f64,
) -> Result<String, String> {
    render_ctx(
        src,
        values,
        &RenderContext {
            opacity_scale: scale,
            ..RenderContext::default()
        },
    )
}

/// The full form: the shader rendered for a profile (its effects opacity, its background color, whether it has a
/// background image).
pub fn render_ctx(
    src: &str,
    values: &BTreeMap<String, String>,
    ctx: &RenderContext,
) -> Result<String, String> {
    let scale = ctx.opacity_scale;
    let body = strip_header(src);
    if !has_annotations(body) {
        return Ok(src.to_string());
    }
    let schema = parse_schema(body)?;
    if schema.params.is_empty() {
        return Ok(body.to_string());
    }
    let mut resolved = resolve(&schema, values);
    if let Some(i) = schema
        .params
        .iter()
        .position(|p| p.name == OPACITY && matches!(p.kind, Kind::Float { .. }))
    {
        let own = resolved[i].parse::<f64>().unwrap_or(1.0);
        resolved[i] = format!("{}", (own * scale.clamp(0.0, 1.0)).clamp(0.0, 1.0));
    }
    let tail = if has_opacity(&schema) {
        footer()
    } else {
        String::new()
    };
    Ok(format!(
        "{}{}{}{}",
        header(&schema, &resolved, ctx),
        body,
        if body.ends_with('\n') { "" } else { "\n" },
        tail
    ))
}

/// The parameter values for a run: a named preset (if given) over the defaults, then `name=value`
/// overrides, every one validated against the shader's declarations.
pub fn values_from_args(
    schema: &Schema,
    preset: Option<&str>,
    sets: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    if let Some(name) = preset {
        let p = schema
            .presets
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| {
                let known: Vec<&str> = schema.presets.iter().map(|p| p.name.as_str()).collect();
                format!(
                    "no preset '{name}' (this shader has: {})",
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                )
            })?;
        for (p, v) in schema.params.iter().zip(preset_values(schema, p)) {
            values.insert(p.name.clone(), v);
        }
    }
    for set in sets {
        let (name, text) = set
            .split_once('=')
            .ok_or_else(|| format!("--set wants name=value, got '{set}'"))?;
        let param = schema
            .params
            .iter()
            .find(|p| p.name == name.trim())
            .ok_or_else(|| format!("no parameter '{}'", name.trim()))?;
        values.insert(param.name.clone(), validate_value(param, text)?);
    }
    Ok(values)
}

/// The value sets worth checking: defaults, every preset, all colors black, all colors white,
/// every number at its minimum and at its maximum.
pub fn check_variants(schema: &Schema) -> Vec<(String, BTreeMap<String, String>)> {
    let mut out: Vec<(String, BTreeMap<String, String>)> =
        vec![("defaults".into(), BTreeMap::new())];
    for p in &schema.presets {
        let vals = preset_values(schema, p);
        out.push((
            format!("preset {}", p.name),
            schema
                .params
                .iter()
                .zip(vals)
                .map(|(p, v)| (p.name.clone(), v))
                .collect(),
        ));
    }
    let make = |f: &dyn Fn(&Param) -> String| -> BTreeMap<String, String> {
        schema
            .params
            .iter()
            .map(|p| (p.name.clone(), f(p)))
            .collect()
    };
    if !schema.params.is_empty() {
        out.push((
            "colors black".into(),
            make(&|p| {
                if p.kind == Kind::Color {
                    "#000000".into()
                } else {
                    p.default.clone()
                }
            }),
        ));
        out.push((
            "colors white".into(),
            make(&|p| {
                if p.kind == Kind::Color {
                    "#ffffff".into()
                } else {
                    p.default.clone()
                }
            }),
        ));
        out.push((
            "numbers at min".into(),
            make(&|p| match p.kind {
                Kind::Float { min, .. } => format_number(min),
                Kind::Color => p.default.clone(),
            }),
        ));
        out.push((
            "numbers at max".into(),
            make(&|p| match p.kind {
                Kind::Float { max, .. } => format_number(max),
                Kind::Color => p.default.clone(),
            }),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "// @color wave_a #ff6b1a \"Wave color\"\n// @float strength 0.16 0.0 0.5 \"Strength\"\n// @preset ocean wave_a=#2fa8ff strength=0.3\nvoid mainImage(out vec4 c, in vec2 p) { c = vec4(P_wave_a * P_strength, 1.0); }\n";

    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_colors_floats_labels_and_presets() {
        let s = parse_schema(SRC).unwrap();
        assert_eq!(s.params.len(), 2);
        assert_eq!(
            s.params[0],
            Param {
                name: "wave_a".into(),
                label: "Wave color".into(),
                kind: Kind::Color,
                default: "#ff6b1a".into()
            }
        );
        assert_eq!(s.params[1].kind, Kind::Float { min: 0.0, max: 0.5 });
        assert_eq!(s.params[1].default, "0.16");
        assert_eq!(
            s.presets,
            vec![Preset {
                name: "ocean".into(),
                values: vec![
                    ("wave_a".into(), "#2fa8ff".into()),
                    ("strength".into(), "0.3".into())
                ]
            }]
        );
        // a shader with no annotations has an empty schema
        assert_eq!(
            parse_schema("void mainImage() {}\n").unwrap(),
            Schema::default()
        );
        // short hex and uppercase are normalized
        let s = parse_schema("// @color c #F60 \"C\"\n").unwrap();
        assert_eq!(s.params[0].default, "#ff6600");
        // a missing label falls back to the name
        assert_eq!(
            parse_schema("// @color c #fff\n").unwrap().params[0].label,
            "c"
        );
    }

    #[test]
    fn bad_annotations_are_rejected_with_the_line_number() {
        for (src, expect) in [
            ("// @color c nothex \"C\"\n", "not a color"),
            ("// @color c #12345 \"C\"\n", "not a color"),
            ("// @color C #ffffff \"C\"\n", "bad name"),
            ("// @color 1c #ffffff \"C\"\n", "bad name"),
            ("// @color c\n", "expected"),
            ("// @color c #fff #000 \"C\"\n", "expected"),
            ("// @float s abc 0 1 \"S\"\n", "not a number"),
            ("// @float s 2 0 1 \"S\"\n", "outside"),
            ("// @float s 0.5 1 0 \"S\"\n", "must be below"),
            ("// @float s 0.5 0 0 \"S\"\n", "must be below"),
            ("// @float s NaN 0 1 \"S\"\n", "not a number"),
            ("// @float s 0.5 0 inf \"S\"\n", "not a number"),
            ("// @float s 0.5 0 1\n", "ok-or-error"),
            ("// @float s 0.5 0 1 \"unterminated\n", "unterminated"),
            ("// @float s 0.5 0 1 \"S\" extra\n", "nothing may follow"),
            ("// @bogus x\n", "unknown annotation"),
            (
                "// @color a #fff \"A\"\n// @color a #000 \"B\"\n",
                "declared twice",
            ),
            (
                "// @color a #fff \"A\"\n// @preset p b=#000\n",
                "unknown parameter",
            ),
            (
                "// @color a #fff \"A\"\n// @preset p a=zzz\n",
                "not a color",
            ),
            ("// @float s 0.5 0 1 \"S\"\n// @preset p s=9\n", "between"),
            (
                "// @color a #fff \"A\"\n// @preset p a=#000\n// @preset p a=#111\n",
                "declared twice",
            ),
            ("// @color a #fff \"A\"\n// @preset p a\n", "not key=value"),
        ] {
            let r = parse_schema(src);
            if expect == "ok-or-error" {
                // a float without a label is fine: the name stands in
                assert!(r.is_ok(), "{src}");
                continue;
            }
            let e = r.expect_err(src);
            assert!(e.contains(expect), "{src:?}: {e}");
            assert!(
                e.contains("line ") || e.contains("declared twice") || e.contains("too many"),
                "{e}"
            );
        }
        // the line number is the right one
        let e = parse_schema("void f() {}\n\n// @color c nope \"C\"\n").unwrap_err();
        assert!(e.starts_with("line 3:"), "{e}");
        // preset names may use '-', parameter names may not (they become GLSL identifiers)
        assert!(parse_schema("// @color a #fff \"A\"\n// @preset deep-space a=#000\n").is_ok());
        assert!(parse_schema("// @color deep-a #fff \"A\"\n").is_err());
        assert!(parse_schema("// @color a #fff \"A\"\n// @preset -x a=#000\n").is_err());
        // ordinary comments that merely mention @ are not annotations
        assert!(parse_schema("// email me @ home\n// see @ the docs\n").is_ok());
    }

    #[test]
    fn limits_on_how_many_parameters_and_presets() {
        let many: String = (0..17)
            .map(|i| format!("// @color c{i} #000000 \"x\"\n"))
            .collect();
        assert!(
            parse_schema(&many)
                .unwrap_err()
                .contains("too many parameters")
        );
        let ok: String = (0..16)
            .map(|i| format!("// @color c{i} #000000 \"x\"\n"))
            .collect();
        assert!(parse_schema(&ok).is_ok());
        let presets: String = std::iter::once("// @color a #000000 \"A\"\n".to_string())
            .chain((0..17).map(|i| format!("// @preset p{i} a=#fff\n")))
            .collect();
        assert!(
            parse_schema(&presets)
                .unwrap_err()
                .contains("too many presets")
        );
    }

    #[test]
    fn values_are_validated_against_the_declared_kind_and_range() {
        let s = parse_schema(SRC).unwrap();
        assert_eq!(
            validate_value(&s.params[0], " #2FA8FF ").unwrap(),
            "#2fa8ff"
        );
        assert_eq!(validate_value(&s.params[0], "f60").unwrap(), "#ff6600");
        for bad in [
            "",
            "red",
            "#12",
            "#12345",
            "#gggggg",
            "rgb(1,2,3)",
            "#fff; evil()",
        ] {
            assert!(validate_value(&s.params[0], bad).is_err(), "{bad}");
        }
        assert_eq!(validate_value(&s.params[1], "0.25").unwrap(), "0.25");
        assert_eq!(
            validate_value(&s.params[1], "0.123456").unwrap(),
            "0.1235",
            "rounded to 4 places"
        );
        assert_eq!(validate_value(&s.params[1], "0").unwrap(), "0");
        assert_eq!(validate_value(&s.params[1], "-0").unwrap(), "0");
        for bad in [
            "",
            "abc",
            "0.6",
            "-0.1",
            "NaN",
            "inf",
            "1e999",
            "0.1.2",
            "0.1; evil()",
            "0x10",
        ] {
            assert!(validate_value(&s.params[1], bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn render_substitutes_values_and_round_trips() {
        let out = render(SRC, &vals(&[("wave_a", "#00ff80"), ("strength", "0.3")])).unwrap();
        assert!(out.starts_with(BEGIN), "{out}");
        assert!(
            out.contains("const vec3 P_wave_a = vec3(0.000000, 1.000000, 0.501961);"),
            "{out}"
        );
        assert!(out.contains("const float P_strength = 0.300000;"), "{out}");
        assert!(
            out.ends_with(SRC),
            "the shader's own code follows untouched"
        );
        // defaults fill whatever the sidecar does not say
        let d = render(SRC, &BTreeMap::new()).unwrap();
        assert!(
            d.contains("P_wave_a = vec3(1.000000, 0.419608, 0.101961)")
                && d.contains("P_strength = 0.160000"),
            "{d}"
        );
        // rendering again from the rendered text replaces the header, it does not stack headers
        let again = render(&out, &vals(&[("wave_a", "#0000ff")])).unwrap();
        assert_eq!(again.matches(BEGIN).count(), 1);
        assert!(
            again.contains("vec3(0.000000, 0.000000, 1.000000)") && !again.contains("0.501961")
        );
        assert_eq!(strip_header(&again), SRC);
        // stable: same input, same output
        assert_eq!(
            render(&out, &vals(&[("wave_a", "#00ff80"), ("strength", "0.3")])).unwrap(),
            out
        );
        // a shader with no parameters is returned untouched
        assert_eq!(
            render("void mainImage() {}\n", &BTreeMap::new()).unwrap(),
            "void mainImage() {}\n"
        );
        // a broken annotation is an error, not a half-rendered file
        assert!(render("// @color c nope \"C\"\n", &BTreeMap::new()).is_err());
    }

    #[test]
    fn a_hostile_sidecar_can_only_ever_choose_numbers() {
        let evil = vals(&[
            ("wave_a", "#fff); evil(); //"),
            ("strength", "0.1; discard; //"),
            ("injected", "float x = 1.0;"),
            ("wave_a\nconst", "x"),
        ]);
        let out = render(SRC, &evil).unwrap();
        assert!(
            !out.contains("evil")
                && !out.contains("discard")
                && !out.contains("injected")
                && !out.contains("float x"),
            "{out}"
        );
        // the invalid values fell back to the defaults
        assert!(
            out.contains("P_wave_a = vec3(1.000000, 0.419608, 0.101961)")
                && out.contains("P_strength = 0.160000")
        );
        // every line of the generated header is a comment or one of the two declaration shapes
        let fixed: Vec<&str> = MASK_GLSL.lines().collect();
        for line in header(
            &parse_schema(SRC).unwrap(),
            &resolve(&parse_schema(SRC).unwrap(), &evil),
            &RenderContext::default(),
        )
        .lines()
        {
            let ok = line.starts_with("//")
                || line.starts_with("const vec3 P_")
                || line.starts_with("const float P_")
                || line.starts_with("#define mainImage")
                || line.starts_with("vec2 gp_yup(")
                || fixed.contains(&line);
            assert!(ok, "an unexpected line in the generated header: {line}");
        }
        // the sidecar parser tolerates junk, binary-ish text and huge files without panicking
        let junk = format!("\u{0}=\u{1}\n=\n==\n{}\n", "x".repeat(10_000));
        let _ = parse_values(&junk);
        assert!(parse_values(&"a = 1\n".repeat(10_000)).len() <= 1);
    }

    #[test]
    fn sidecar_values_round_trip_and_presets_apply() {
        let s = parse_schema(SRC).unwrap();
        let resolved = resolve(
            &s,
            &vals(&[
                ("wave_a", "#00FF80"),
                ("strength", "0.3333333"),
                ("bogus", "1"),
            ]),
        );
        assert_eq!(resolved, vec!["#00ff80", "0.3333"]);
        let text = format_values(&s, &resolved);
        assert!(text.contains("wave_a = #00ff80") && text.contains("strength = 0.3333"));
        assert_eq!(
            resolve(&s, &parse_values(&text)),
            resolved,
            "write then read gives the same values"
        );
        // presets: their values over the defaults; the matching one is recognised
        let ocean = &s.presets[0];
        let pv = preset_values(&s, ocean);
        assert_eq!(pv, vec!["#2fa8ff", "0.3"]);
        assert_eq!(
            matching_preset(&s, &pv).map(|p| p.name.as_str()),
            Some("ocean")
        );
        assert_eq!(matching_preset(&s, &resolved), None);
        // a preset that sets only some params leaves the rest at their defaults
        let s2 = parse_schema(
            "// @color a #111111 \"A\"\n// @color b #222222 \"B\"\n// @preset p a=#333333\n",
        )
        .unwrap();
        assert_eq!(
            preset_values(&s2, &s2.presets[0]),
            vec!["#333333", "#222222"]
        );
    }

    #[test]
    fn header_is_idempotent_to_strip_and_ignores_text_lookalikes() {
        let s = parse_schema(SRC).unwrap();
        let h = header(
            &s,
            &resolve(&s, &BTreeMap::new()),
            &RenderContext::default(),
        );
        let full = format!("{h}{SRC}");
        assert_eq!(strip_header(&full), SRC);
        assert_eq!(strip_header(SRC), SRC, "nothing to strip");
        // the marker in the middle of a file is not a header
        let sneaky = format!("void f() {{}}\n{BEGIN}\nconst float x = 1.0;\n{END}\n");
        assert_eq!(strip_header(&sneaky), sneaky);
        // a begin marker with no end is left alone
        let broken = format!("{BEGIN}\nconst float P_a = 1.0;\n");
        assert_eq!(strip_header(&broken), broken);
    }

    #[test]
    fn motion_and_coverage_annotations_are_parsed_and_bad_ones_rejected() {
        let s =
            parse_schema("// @motion down\n// @coverage full\n// @float opacity 0.6 0 1 \"O\"\n")
                .unwrap();
        assert_eq!(s.motion, Some(Motion::Down));
        assert!(s.coverage_full);
        for m in ["down", "up", "left", "right", "radial", "none"] {
            assert_eq!(
                parse_schema(&format!("// @motion {m}\n"))
                    .unwrap()
                    .motion
                    .map(Motion::name),
                Some(m)
            );
        }
        assert_eq!(parse_schema("void f() {}\n").unwrap().motion, None);
        assert!(!parse_schema("void f() {}\n").unwrap().coverage_full);
        assert!(
            parse_schema("// @motion sideways\n")
                .unwrap_err()
                .contains("unknown motion")
        );
        assert!(
            parse_schema("// @coverage half\n")
                .unwrap_err()
                .contains("unknown coverage")
        );
    }

    const OP: &str = "// @float opacity 0.6 0.0 1.0 \"Opacity\"\n// @float strength 0.5 0.0 1.0 \"S\"\nvoid mainImage(out vec4 fragColor, in vec2 fragCoord) { fragColor = texture(iChannel0, fragCoord / iResolution.xy) + vec4(P_strength); }\n";

    #[test]
    fn a_shader_with_an_opacity_parameter_gets_the_wrapper_and_the_master_scale() {
        let out = render(OP, &BTreeMap::new()).unwrap();
        assert!(
            out.contains("#define mainImage gp_effect"),
            "the shader's own function is renamed"
        );
        assert!(
            out.contains("#undef mainImage")
                && out.contains("void mainImage(out vec4 fragColor, in vec2 fragCoord)"),
            "and wrapped"
        );
        assert!(
            out.contains("mix(gp_base.rgb, gp_fx.rgb, P_opacity)"),
            "opacity scales the effect, never the base"
        );
        assert!(out.contains("const float P_opacity = 0.600000;"));
        // the master scale multiplies the shader's own opacity; it is clamped to 0..1
        let half = render_scaled(OP, &BTreeMap::new(), 0.5).unwrap();
        assert!(half.contains("P_opacity = 0.300000"), "{half}");
        assert!(
            render_scaled(OP, &BTreeMap::new(), 7.0)
                .unwrap()
                .contains("P_opacity = 0.600000")
        );
        assert!(
            render_scaled(OP, &BTreeMap::new(), -1.0)
                .unwrap()
                .contains("P_opacity = 0.000000")
        );
        // rendering twice never stacks wrappers or headers, and stripping gives back the source
        let again = render(&out, &BTreeMap::new()).unwrap();
        assert_eq!(again, out, "idempotent");
        assert_eq!(again.matches("opacity wrapper (generated").count(), 1);
        assert_eq!(strip_header(&out), OP);
        // a shader without an opacity parameter gets no wrapper
        assert!(!render(SRC, &BTreeMap::new()).unwrap().contains("gp_effect"));
    }

    #[test]
    fn the_header_tells_the_shader_the_background_and_defines_the_text_mask_and_the_y_up_helper() {
        let ctx = RenderContext {
            opacity_scale: 1.0,
            background: (0x2c, 0x2c, 0x2c),
            background_image: false,
        };
        let out = render_ctx(OP, &BTreeMap::new(), &ctx).unwrap();
        assert!(
            out.contains("const vec3 P_bg = vec3(0.172549, 0.172549, 0.172549);")
                && out.contains("const float P_bg_image = 0.0;")
        );
        assert!(
            out.contains("float gp_textMask(vec2 fragCoord, vec4 term)")
                && out.contains("vec2 gp_yup(vec2 fragCoord)")
        );
        let img = render_ctx(
            OP,
            &BTreeMap::new(),
            &RenderContext {
                background_image: true,
                ..ctx
            },
        )
        .unwrap();
        assert!(img.contains("P_bg_image = 1.0"));
        // the default context is Ghostty's own default background
        assert!(
            render(OP, &BTreeMap::new())
                .unwrap()
                .contains("P_bg = vec3(0.156863, 0.172549, 0.203922)")
        );
        // a different background rewrites the header in place
        let other = render_ctx(
            &out,
            &BTreeMap::new(),
            &RenderContext {
                background: (0, 0, 0),
                ..ctx
            },
        )
        .unwrap();
        assert!(
            other.contains("P_bg = vec3(0.000000, 0.000000, 0.000000)")
                && !other.contains("0.172549")
        );
        assert_eq!(other.matches("const vec3 P_bg").count(), 1);
    }

    #[test]
    fn format_number_is_short_and_stable() {
        for (n, s) in [
            (0.16, "0.16"),
            (1.0, "1"),
            (0.0, "0"),
            (-0.0, "0"),
            (0.33333, "0.3333"),
            (12.5, "12.5"),
            (0.00004, "0"),
        ] {
            assert_eq!(format_number(n), s, "{n}");
        }
    }
}
