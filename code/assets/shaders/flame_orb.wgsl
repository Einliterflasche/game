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
    for (var i = 0; i < 4; i++) {
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

    // Displace vertices along normal using noise — breaks the sphere silhouette
    var noise_pos = world_pos * 5.0;
    noise_pos.y -= t * 3.0;
    let displacement = fbm(noise_pos) * 2.0 - 0.8;
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

    // Primary flame noise — rises upward, large scale
    var noise_pos = world_pos * 6.0;
    noise_pos.y -= t * 3.0;
    let n1 = fbm(noise_pos);

    // Secondary turbulence — faster, different direction for chaotic look
    var turb_pos = world_pos * 10.0;
    turb_pos.y -= t * 5.0;
    turb_pos.x += t * 1.5;
    let n2 = fbm(turb_pos);

    // Combine: noise defines an irregular boundary
    let noise_val = n1 * 0.6 + n2 * 0.4;
    let noise_boundary = noise_val * 2.0 - 0.6;
    let flame = saturate(noise_boundary - rim * 0.25);

    // Golden color ramp
    let bright_gold = vec3(1.0, 0.95, 0.7);
    let gold        = vec3(1.0, 0.7, 0.15);
    let deep_gold   = vec3(0.9, 0.45, 0.03);
    let ember       = vec3(0.5, 0.15, 0.0);

    let c1 = mix(ember, deep_gold, smoothstep(0.0, 0.3, flame));
    let c2 = mix(c1, gold, smoothstep(0.3, 0.6, flame));
    let color = mix(c2, bright_gold, smoothstep(0.6, 1.0, flame));

    let hdr_color = color * core_color.rgb;

    // Ragged alpha — sharp cutoff for wispy edges
    let alpha = smoothstep(0.0, 0.08, flame);

    return vec4(hdr_color, alpha);
}
