// FIXTURE: declares that it falls DOWN but the streaks move UP the screen: the classic y-up/y-down mix-up (a Shadertoy
// shader dropped into Ghostty). `shaderlab check` must FAIL it on `motion`.
//
// @motion down
// @float opacity 0.7 0.0 1.0 "Opacity"

float hash(float n) { return fract(sin(n * 12.9898) * 43758.5453); }

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec4 term = texture(iChannel0, uv);
    float behind = 1.0 - gp_textMask(fragCoord, term);
    float col = floor(fragCoord.x / 6.0);
    float on = step(0.9, hash(col));
    float head = mod(iTime * 500.0 + hash(col + 3.0) * 2000.0, iResolution.y + 400.0);
    // BUG: the head is measured from the BOTTOM, but y grows downward in Ghostty, so this moves up
    float behindHead = fragCoord.y - (iResolution.y - head);
    float streak = on * step(0.0, behindHead) * step(behindHead, 90.0) * (1.0 - behindHead / 90.0);
    fragColor = vec4(term.rgb + vec3(0.4, 0.6, 1.0) * streak * behind * 0.8, term.a);
}
// regress: skip (a deliberate fixture: declares the wrong motion direction)
