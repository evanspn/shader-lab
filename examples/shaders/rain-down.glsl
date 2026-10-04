// Thin rain falling DOWN the screen, behind the text. Written for Ghostty (origin top-left, y DOWN).
// Because y grows downward there, falling is simply +y: the streaks' phase advances with time.
//
// @motion down
// @float opacity 0.7 0.0 1.0 "Opacity"
// @color rain #6fb6ff "Rain color"
// @float density 0.10 0.02 0.5 "Density"
// @float speed 1.0 0.2 3.0 "Speed"

float hash(float n) { return fract(sin(n * 12.9898) * 43758.5453); }

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec4 term = texture(iChannel0, uv);
    float behind = 1.0 - gp_textMask(fragCoord, term);
    float col = floor(fragCoord.x / 6.0);
    float on = step(1.0 - P_density, hash(col));
    float speed = (300.0 + 500.0 * hash(col + 7.0)) * P_speed;          // pixels per second, downward
    float head = mod(iTime * speed + hash(col + 3.0) * 2000.0, iResolution.y + 400.0);
    float behindHead = head - fragCoord.y;                              // y grows DOWN: below the head is larger y
    float streak = on * step(0.0, behindHead) * step(behindHead, 90.0) * (1.0 - behindHead / 90.0);
    fragColor = vec4(term.rgb + P_rain * streak * behind * 0.8, term.a);
}
