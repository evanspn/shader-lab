// A soft glow along the TOP edge of the window, written the SHADERTOY way (origin bottom-left, y UP).
// Render or check it with `--origin bottom-left`; with the default (Ghostty, top-left) the glow ends up at the BOTTOM,
// which is exactly the mistake the flag exists to avoid.
//
// @motion none
// @coverage full
// @float opacity 0.8 0.0 1.0 "Opacity"
// @color glow #4df0ff "Glow color"
// @float height 0.25 0.05 0.6 "Height"

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;       // uv.y = 1 is the TOP here (Shadertoy convention)
    vec4 term = texture(iChannel0, uv);
    float behind = 1.0 - gp_textMask(fragCoord, term);
    float edge = smoothstep(1.0 - P_height, 1.0, uv.y);
    fragColor = vec4(term.rgb + P_glow * edge * 0.35 * behind, term.a);
}
