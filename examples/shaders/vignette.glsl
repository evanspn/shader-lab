// A gentle vignette that only darkens plain background: text, cursor and selections keep their pixels.
// Written for Ghostty (origin top-left, y down). It needs no orientation: a vignette is symmetric.
//
// @motion none
// @coverage full
// @float opacity 1.0 0.0 1.0 "Opacity"
// @float amount 0.35 0.0 0.8 "Amount"
// @preset subtle amount=0.2
// @preset heavy amount=0.6

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec4 term = texture(iChannel0, uv);
    float behind = 1.0 - gp_textMask(fragCoord, term);
    float vig = 1.0 - P_amount * smoothstep(0.25, 0.75, length(uv - 0.5)) * behind;
    fragColor = vec4(term.rgb * vig, term.a);
}
