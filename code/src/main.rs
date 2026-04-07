use std::f32::consts::FRAC_PI_2;

use avian3d::prelude::*;
use bevy::{
    camera::Exposure,
    core_pipeline::tonemapping::Tonemapping,
    dev_tools::fps_overlay::{FpsOverlayConfig, FpsOverlayPlugin},
    input::mouse::AccumulatedMouseMotion,
    light::{light_consts::lux, AtmosphereEnvironmentMapLight, CascadeShadowConfigBuilder},
    pbr::{Atmosphere, AtmosphereSettings, ScatteringMedium},
    post_process::bloom::Bloom,
    prelude::*,
    render::{render_resource::AsBindGroup, view::{ColorGrading, ColorGradingGlobal}},
    shader::ShaderRef,
    window::{CursorGrabMode, CursorOptions},
};
use bevy_tnua::builtins::{
    TnuaBuiltinJump, TnuaBuiltinJumpConfig, TnuaBuiltinWalk, TnuaBuiltinWalkConfig,
};
use bevy_tnua::prelude::*;
use bevy_tnua_avian3d::prelude::*;

// --- Types ---

const PLAYER_SPAWN: Vec3 = Vec3::new(0.0, 2.0, 0.0);
const PLAYER_RADIUS: f32 = 0.25;
const PLAYER_CYLINDER_HEIGHT: f32 = 1.0;
const WAND_TIP_OFFSET: Vec3 = Vec3::new(0.25, -0.15, -0.7);

const SPELL_SPEED: f32 = 12.0;
const SPELL_LIFETIME: f32 = 3.0;
const SPELL_RADIUS: f32 = 0.1;

#[derive(Component)]
struct Player;

#[derive(Component)]
struct Spell {
    timer: Timer,
}

#[derive(TnuaScheme)]
#[scheme(basis = TnuaBuiltinWalk)]
enum PlayerActions {
    Jump(TnuaBuiltinJump),
}

/// Per-tick player input, captured each frame in `Update` and consumed in `FixedUpdate`.
///
/// Held inputs (`desired_motion`, `jump`) reflect the latest sample. Edge events
/// (`cast_queue`, `respawn`) are latched until the simulation drains them, so a click
/// that lands between two fixed steps is never lost. This split is the minimum
/// structure that lets the simulation be a pure function of input — a prerequisite
/// for client-side prediction and server reconciliation in multiplayer.
#[derive(Resource, Default)]
struct PlayerInput {
    desired_motion: Vec3,
    jump: bool,
    cast_queue: Vec<PendingCast>,
    respawn: bool,
}

#[derive(Clone, Copy)]
struct PendingCast {
    origin: Vec3,
    direction: Vec3,
}

#[derive(Debug, Resource)]
struct CameraSettings {
    pitch_speed: f32,
    yaw_speed: f32,
    pitch_range: std::ops::Range<f32>,
}

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
struct FlameOrbMaterial {
    #[uniform(0)]
    core_color: LinearRgba,
    #[uniform(1)]
    flame_speed: f32,
}

// --- Impls & functions ---

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            PhysicsPlugins::default(),
            TnuaControllerPlugin::<PlayerActions>::new(FixedUpdate),
            TnuaAvian3dPlugin::new(FixedUpdate),
            FpsOverlayPlugin {
                config: FpsOverlayConfig {
                    text_color: Color::srgb(0.5, 1.0, 0.5),
                    text_config: TextFont {
                        font_size: 20.0,
                        ..default()
                    },
                    ..default()
                },
            },
            MaterialPlugin::<FlameOrbMaterial>::default(),
        ))
        .insert_resource(GlobalAmbientLight::NONE)
        .init_resource::<CameraSettings>()
        .init_resource::<PlayerInput>()
        .add_systems(Startup, (setup, lock_cursor))
        .add_systems(Update, (first_person_camera, capture_input).chain())
        .add_systems(
            FixedUpdate,
            (
                respawn_player,
                player_controls.in_set(TnuaUserControlsSystems),
                cast_spell,
                despawn_expired_spells,
            )
                .chain(),
        )
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut configs: ResMut<Assets<PlayerActionsConfig>>,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
) {
    // ground
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(1000.0, 1000.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
        RigidBody::Static,
        Collider::half_space(Vec3::Y),
    ));
    // player
    commands.spawn((
        Player,
        Mesh3d(meshes.add(Capsule3d {
            radius: PLAYER_RADIUS,
            half_length: PLAYER_CYLINDER_HEIGHT / 2.0,
        })),
        MeshMaterial3d(materials.add(Color::srgb(0.5, 0.5, 0.5))),
        Transform::from_translation(PLAYER_SPAWN),
        RigidBody::Dynamic,
        Collider::capsule(PLAYER_RADIUS, PLAYER_CYLINDER_HEIGHT),
        TnuaController::<PlayerActions>::default(),
        TnuaConfig::<PlayerActions>(configs.add(PlayerActionsConfig {
            basis: TnuaBuiltinWalkConfig {
                speed: 1.4,
                // Center of mass height above ground. Half the total capsule height
                // plus a small margin for the spring system.
                float_height: (PLAYER_CYLINDER_HEIGHT / 2.0 + PLAYER_RADIUS) + 0.01,
                ..default()
            },
            jump: TnuaBuiltinJumpConfig {
                height: 1.5,
                ..default()
            },
        })),
        TnuaAvian3dSensorShape(Collider::cylinder(PLAYER_RADIUS - 0.01, 0.0)),
        LockedAxes::new().lock_rotation_x().lock_rotation_z(),
    ));
    // reference cubes
    let cube_material = materials.add(Color::srgb(0.7, 0.7, 0.7));
    for (size, pos) in [
        (1.0, Vec3::new(3.0, 0.5, -3.0)),
        (2.0, Vec3::new(-4.0, 1.0, -5.0)),
        (3.0, Vec3::new(6.0, 1.5, -6.0)),
        (1.0, Vec3::new(-2.0, 0.5, 2.0)),
        (2.0, Vec3::new(5.0, 1.0, 3.0)),
    ] {
        commands.spawn((
            Mesh3d(meshes.add(Cuboid::new(size, size, size))),
            MeshMaterial3d(cube_material.clone()),
            Transform::from_translation(pos),
            RigidBody::Static,
            Collider::cuboid(size, size, size),
        ));
    }
    // sun
    commands.spawn((
        DirectionalLight {
            color: Color::srgb(1.0, 0.98, 0.94),
            illuminance: lux::RAW_SUNLIGHT,
            shadows_enabled: true,
            ..default()
        },
        Transform::IDENTITY.looking_to(Vec3::new(1.0, -0.3, 0.5), Vec3::Y),
        CascadeShadowConfigBuilder {
            first_cascade_far_bound: 5.0,
            maximum_distance: 50.0,
            ..default()
        }
        .build(),
    ));
    // camera
    commands.spawn((
        Camera3d::default(),
        Transform::from_translation(PLAYER_SPAWN + Vec3::Y * 0.65),
        Atmosphere::earthlike(scattering_mediums.add(ScatteringMedium::default())),
        AtmosphereSettings::default(),
        AtmosphereEnvironmentMapLight::default(),
        Exposure { ev100: 13.0 },
        Tonemapping::AcesFitted,
        Bloom::NATURAL,
        ColorGrading {
            global: ColorGradingGlobal {
                temperature: 0.03,
                post_saturation: 1.1,
                ..default()
            },
            ..default()
        },
    ))
    .with_child((
        Mesh3d(meshes.add(Cylinder { radius: 0.015, half_height: 0.2 })),
        MeshMaterial3d(materials.add(Color::srgb(0.4, 0.25, 0.1))),
        Transform::from_xyz(0.25, -0.15, -0.4)
            .with_rotation(Quat::from_rotation_x(FRAC_PI_2)),
    ));
}

fn lock_cursor(mut cursor: Single<&mut CursorOptions, With<Window>>) {
    cursor.grab_mode = CursorGrabMode::Locked;
    cursor.visible = false;
}

fn capture_input(
    mut input: ResMut<PlayerInput>,
    key_input: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    camera: Single<&Transform, With<Camera>>,
) {
    const BINDINGS: [(KeyCode, Vec3); 4] = [
        (KeyCode::KeyW, Vec3::Z),
        (KeyCode::KeyS, Vec3::NEG_Z),
        (KeyCode::KeyA, Vec3::X),
        (KeyCode::KeyD, Vec3::NEG_X),
    ];

    let local: Vec3 = BINDINGS
        .iter()
        .filter(|(key, _)| key_input.pressed(*key))
        .map(|(_, dir)| *dir)
        .sum::<Vec3>()
        .normalize_or_zero();

    let forward = camera.forward().as_vec3();
    let forward = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
    let right = Vec3::Y.cross(forward).normalize_or_zero();
    let sprint = if key_input.pressed(KeyCode::ShiftLeft) { 3.0 } else { 1.0 };

    input.desired_motion = (forward * local.z + right * local.x) * sprint;
    input.jump = key_input.pressed(KeyCode::Space);

    if mouse_buttons.just_pressed(MouseButton::Left) {
        input.cast_queue.push(PendingCast {
            origin: camera.transform_point(WAND_TIP_OFFSET),
            direction: camera.forward().as_vec3(),
        });
    }
    if key_input.just_pressed(KeyCode::Escape) {
        input.respawn = true;
    }
}

fn respawn_player(
    mut player: Single<(&mut Transform, &mut LinearVelocity, &mut AngularVelocity), With<Player>>,
    mut input: ResMut<PlayerInput>,
) {
    if !std::mem::take(&mut input.respawn) {
        return;
    }
    let (transform, linear_vel, angular_vel) = &mut *player;
    transform.translation = PLAYER_SPAWN;
    linear_vel.0 = Vec3::ZERO;
    angular_vel.0 = Vec3::ZERO;
}

fn player_controls(
    mut controller: Single<&mut TnuaController<PlayerActions>, With<Player>>,
    input: Res<PlayerInput>,
) {
    let desired_motion = input.desired_motion;
    let desired_forward = Dir3::new(desired_motion).ok();

    controller.initiate_action_feeding();

    controller.basis = TnuaBuiltinWalk {
        desired_motion,
        desired_forward,
        ..default()
    };

    if input.jump {
        controller.action(PlayerActions::Jump(default()));
    }
}

fn cast_spell(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FlameOrbMaterial>>,
    mut input: ResMut<PlayerInput>,
) {
    for cast in input.cast_queue.drain(..) {
        commands.spawn((
            Spell { timer: Timer::from_seconds(SPELL_LIFETIME, TimerMode::Once) },
            Mesh3d(meshes.add(Sphere { radius: SPELL_RADIUS })),
            MeshMaterial3d(materials.add(FlameOrbMaterial {
                core_color: LinearRgba::new(10.0, 6.0, 1.5, 1.0),
                flame_speed: 1.0,
            })),
            Transform::from_translation(cast.origin),
            RigidBody::Dynamic,
            Collider::sphere(SPELL_RADIUS),
            Sensor,
            CollidingEntities::default(),
            LinearVelocity(cast.direction * SPELL_SPEED),
            GravityScale(0.0),
        ));
    }
}

fn despawn_expired_spells(
    mut commands: Commands,
    mut spells: Query<(Entity, &mut Spell, &CollidingEntities)>,
    player: Single<Entity, With<Player>>,
    time: Res<Time>,
) {
    let player = *player;
    for (entity, mut spell, colliding) in &mut spells {
        spell.timer.tick(time.delta());
        let hit_something = colliding.iter().any(|&e| e != player);
        if spell.timer.is_finished() || hit_something {
            commands.entity(entity).despawn();
        }
    }
}

fn first_person_camera(
    mut camera: Single<&mut Transform, With<Camera>>,
    player: Single<&Transform, (With<Player>, Without<Camera>)>,
    mouse_motion: Res<AccumulatedMouseMotion>,
    settings: Res<CameraSettings>,
) {
    let (yaw, pitch, _) = camera.rotation.to_euler(EulerRot::YXZ);
    let pitch = (pitch - mouse_motion.delta.y * settings.pitch_speed)
        .clamp(settings.pitch_range.start, settings.pitch_range.end);
    let yaw = yaw - mouse_motion.delta.x * settings.yaw_speed;
    camera.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);

    let eye_offset = PLAYER_CYLINDER_HEIGHT / 2.0 + PLAYER_RADIUS - 0.1;
    camera.translation = player.translation + Vec3::Y * eye_offset;
}

impl Material for FlameOrbMaterial {
    fn vertex_shader() -> ShaderRef {
        "shaders/flame_orb.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "shaders/flame_orb.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }
}

impl Default for CameraSettings {
    fn default() -> Self {
        let pitch_limit = FRAC_PI_2 - 0.01;
        Self {
            pitch_speed: 0.003,
            yaw_speed: 0.004,
            pitch_range: -pitch_limit..pitch_limit,
        }
    }
}
