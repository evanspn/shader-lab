// FIXTURE: a deliberately BAD shader. It blurs the whole terminal with a 9-tap kernel and washes it with light, the way
// an aurora that "blurs everything" does. `shaderlab check` must FAIL it on the text checks.
//
// @motion none
// @coverage full
// @float opacity 1.0 0.0 1.0 "Opacity"

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec2 px = 1.0 / iResolution.xy;
    vec3 sum = vec3(0.0);
    for (int j = -1; j <= 1; j++) {
        for (int i = -1; i <= 1; i++) {
            sum += texture(iChannel0, uv + vec2(float(i), float(j)) * px * 3.0).rgb;
        }
    }
    vec4 term = texture(iChannel0, uv);
    fragColor = vec4(sum / 9.0 + vec3(0.04), term.a);
}
