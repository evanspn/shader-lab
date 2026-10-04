// FIXTURE: reads iTime but never lets it affect the picture, so it is a static shader that pretends to animate.
// `shaderlab check` must FAIL it on `animation`.
//
// @motion none
// @float opacity 1.0 0.0 1.0 "Opacity"

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec4 term = texture(iChannel0, uv);
    float unused = iTime * 0.0;
    fragColor = vec4(term.rgb + vec3(0.02) * (1.0 - gp_textMask(fragCoord, term)) + vec3(unused), term.a);
}
