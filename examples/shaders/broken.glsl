// FIXTURE: does not compile (an undefined name and a missing semicolon). `shaderlab check` must FAIL it on `compiles`
// and say which line.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec4 term = texture(iChannel0, uv);
    float x = no_such_function(uv.x)
    fragColor = vec4(term.rgb + x, term.a);
}
// regress: skip (a deliberate fixture: does not compile)
