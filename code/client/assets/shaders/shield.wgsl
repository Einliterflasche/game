#import bevy_pbr::{
    mesh_view_bindings::{globals, view},
    mesh_functions,
    forward_io::{Vertex, VertexOutput},
    view_transformations::position_world_to_clip,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> edge_color: vec4<f32>;

@vertex
fn vertex(vertex: Vertex) -> VertexOutput {
    var out: VertexOutput;
    let world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
    out.world_position = mesh_functions::mesh_position_local_to_world(
        world_from_local, vec4(vertex.position, 1.0)
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

// Translucent shield surface: solid blue body, brighter fresnel rim. The rim
// glow comes from `pow(1 - |dot(view, normal)|, …)` which spikes at grazing
// angles, so the silhouette of the (scaled-sphere) ellipsoid lights up while
// the dead-on center reads as a uniform haze. `abs` keeps the rim visible if
// backface culling ever flips and we end up looking at the inside surface.
@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let world_pos = in.world_position.xyz;
    let normal = normalize(in.world_normal);
    let view_dir = normalize(view.world_position - world_pos);
    let facing = abs(dot(view_dir, normal));
    let fresnel = pow(1.0 - facing, 2.0);

    // Soft 6 Hz pulse so the shield reads as "magical / active" rather than a
    // static decal. Range 0.9–1.0, multiplied into the final alpha.
    let pulse = 0.9 + 0.1 * sin(globals.time * 6.0);

    // Base alpha is high enough that the shield is visible even when fresnel
    // collapses to zero on faces whose normals end up parallel to view_dir
    // after the non-uniform-scale normal transform.
    let base_alpha = 0.45;
    let alpha = saturate((base_alpha + fresnel * 0.5) * pulse);

    // Color rises from a tinted base to the configured edge_color at the rim.
    let base_color = edge_color.rgb * 0.7;
    let color = mix(base_color, edge_color.rgb * 1.4, fresnel);

    return vec4(color, alpha);
}
