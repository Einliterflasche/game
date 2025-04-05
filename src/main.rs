use avian3d::prelude::*;
use bevy::prelude::*;
use industrial_mage::controls::{CharacterControllerBundle, CharacterControllerPlugin};

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            PhysicsPlugins::default().set(PhysicsInterpolationPlugin::interpolate_all()),
            CharacterControllerPlugin,
        ))
        .add_systems(Startup, (setup_camera, setup_world))
        .run();
}

fn setup_camera(mut commands: Commands) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(-2.5, 9.5, 9.0),
        // DistanceFog {
        //     color: Color::srgba(0.35, 0.48, 0.66, 1.0),
        //     directional_light_color: Color::srgba(1.0, 0.95, 0.85, 0.5),
        //     directional_light_exponent: 30.0,
        //     falloff: FogFalloff::from_visibility_colors(
        //         15.0, // distance in world units up to which objects retain visibility (>= 5% contrast)
        //         Color::srgb(0.35, 0.5, 0.66), // atmospheric extinction color (after light is lost due to absorption by atmospheric particles)
        //         Color::srgb(0.8, 0.844, 1.0), // atmospheric inscattering color (light gained due to scattering from the sun)
        //     ),
        // },
    ));
}

fn setup_world(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // circular base
    commands.spawn((
        Mesh3d(meshes.add(Circle::new(8.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
        Transform::from_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        RigidBody::Static,
        Collider::cuboid(16.0, 16.0, 0.001),
    ));
    // Player
    // TODO: visible mesh is offset compared to collider
    commands.spawn((
        CharacterControllerBundle::new(Collider::capsule(0.5, 2.0)),
        Mesh3d(meshes.add(Capsule3d::new(0.4, 1.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.8, 0.7, 0.6))),
        Transform::from_xyz(0.0, 5.0, 0.0),
        GravityScale(2.0),
    ));

    // A cube to move around
    commands.spawn((
        RigidBody::Dynamic,
        Collider::capsule(0.5, 2.0),
        Mesh3d(meshes.add(Cuboid::default())),
        MeshMaterial3d(materials.add(Color::srgb(0.8, 0.7, 0.6))),
        Transform::from_xyz(2.0, 2.0, 2.0),
    ));
    // light
    commands.spawn((
        DirectionalLight {
            color: Color::srgb(0.98, 0.95, 0.82),
            shadows_enabled: true,
            illuminance: 10_000.0,
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, 100.0).looking_at(Vec3::new(-0.15, -0.05, 0.25), Vec3::Y),
    ));
}
