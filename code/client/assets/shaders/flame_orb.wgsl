#import bevy_pbr::{
    mesh_view_bindings::{globals, view},
    mesh_bindings::mesh,
    mesh_functions,
    forward_io::{Vertex, VertexOutput},
    view_transformations::position_world_to_clip,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> core_color: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> flame_speed: f32;

fn hash31(p: vec3<f32>) -> f32 {
    var q = fract(p * 0.1031);
    q += dot(q, q.zyx + 31.32);
    return fract((q.x + q.y) * q.z);
}

fn value_noise(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);

    return mix(
        mix(
            mix(hash31(i), hash31(i + vec3(1.0, 0.0, 0.0)), u.x),
            mix(hash31(i + vec3(0.0, 1.0, 0.0)), hash31(i + vec3(1.0, 1.0, 0.0)), u.x),
            u.y
        ),
        mix(
            mix(hash31(i + vec3(0.0, 0.0, 1.0)), hash31(i + vec3(1.0, 0.0, 1.0)), u.x),
            mix(hash31(i + vec3(0.0, 1.0, 1.0)), hash31(i + vec3(1.0, 1.0, 1.0)), u.x),
            u.y
        ),
        u.z
    );
}

fn fbm(p: vec3<f32>) -> f32 {
    var val = 0.0;
    var amp = 0.5;
    var pos = p;
    // 3 octaves: visually identical to 4 at the orb's screen size, ~25% cheaper.
    for (var i = 0; i < 3; i++) {
        val += amp * value_noise(pos);
        pos *= 2.2;
        amp *= 0.5;
    }
    return val;
}

@vertex
fn vertex(vertex: Vertex) -> VertexOutput {
    var out: VertexOutput;

    let world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
    let world_pos = (world_from_local * vec4(vertex.position, 1.0)).xyz;
    let t = globals.time * flame_speed;

    // Domain distortion — FBM feeds into FBM for organic swirling
    var p = world_pos * 5.0;
    p.y -= t * 2.0;
    let q = vec3(
        fbm(p + vec3(0.0, 0.0, t * 0.5)),
        fbm(p + vec3(0.3, 1.3, t * 0.5)),
        t * 0.5
    );
    let displacement = fbm(p + q * 0.5) * 2.0 - 0.8;
    let displaced_pos = vertex.position + vertex.normal * displacement * 0.08;

    out.world_position = mesh_functions::mesh_position_local_to_world(
        world_from_local, vec4(displaced_pos, 1.0)
    );
    out.position = position_world_to_clip(out.world_position.xyz);

#ifdef VERTEX_NORMALS
    out.world_normal = mesh_functions::mesh_normal_local_to_world(
        vertex.normal, vertex.instance_index
    );
#endif
#ifdef VERTEX_UVS_A
    out.uv = vertex.uv;
#endif
#ifdef VERTEX_OUTPUT_INSTANCE_INDEX
    out.instance_index = vertex.instance_index;
#endif

    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let t = globals.time * flame_speed;
    let world_pos = in.world_position.xyz;
    let normal = normalize(in.world_normal);
    let view_dir = normalize(view.world_position - world_pos);

    let facing = saturate(dot(view_dir, normal));
    let rim = 1.0 - facing;

    // Domain distortion — single FBM warp re-used on both axes. Used to be two
    // separate fbm calls; visually almost identical and one fewer fbm per pixel.
    var p = world_pos * 6.0;
    p.y -= t * 2.0;
    let warp = fbm(p + vec3(0.0, 0.0, t * 0.5));
    let q = vec3(warp, warp * 0.7 + 0.3, t * 0.5);
    let fire_val = fbm(p + q * 0.5);
    let flame = saturate(fire_val * 2.0 - 0.6 - rim * 0.25);

    // Golden color ramp
    let bright_gold = vec3(1.0, 0.95, 0.7);
    let gold        = vec3(1.0, 0.7, 0.15);
    let deep_gold   = vec3(0.9, 0.45, 0.03);
    let ember       = vec3(0.7, 0.08, 0.0);

    let c1 = mix(ember, deep_gold, smoothstep(0.0, 0.3, flame));
    let c2 = mix(c1, gold, smoothstep(0.3, 0.6, flame));
    let color = mix(c2, bright_gold, smoothstep(0.6, 1.0, flame));

    let hdr_color = color * core_color.rgb;

    // Ragged alpha — sharp cutoff for wispy edges. AlphaMode::Mask(0.5) means
    // the pipeline expects an opaque output, so we discard below the threshold
    // ourselves and emit alpha = 1.0 above it.
    let alpha = smoothstep(0.0, 0.08, flame);
    if (alpha < 0.5) {
        discard;
    }

    return vec4(hdr_color, 1.0);
}
