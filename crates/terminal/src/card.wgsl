// Rounded block cards of `nulix up` (see cards.rs): a signed-distance rounded
// rectangle with a fill, an inner border and a one-pixel antialiased edge.

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) half_size: vec2<f32>,
    // x: corner radius, y: border width (pixels).
    @location(2) shape: vec2<f32>,
    @location(3) fill: vec3<f32>,
    @location(4) border: vec3<f32>,
}

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) local: vec2<f32>,
    @location(2) half_size: vec2<f32>,
    @location(3) shape: vec2<f32>,
    @location(4) fill: vec3<f32>,
    @location(5) border: vec3<f32>,
) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.local = local;
    output.half_size = half_size;
    output.shape = shape;
    output.fill = fill;
    output.border = border;
    return output;
}

// Distance from `p` to a rectangle of half size `half` with corner radius `r`
// (negative inside).
fn rounded_rect_distance(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let distance = rounded_rect_distance(input.local, input.half_size, input.shape.x);
    let coverage = clamp(0.5 - distance, 0.0, 1.0);
    let inside_border = clamp(0.5 - (distance + input.shape.y), 0.0, 1.0);
    let color = mix(input.border, input.fill, inside_border);
    return vec4<f32>(color, coverage);
}
