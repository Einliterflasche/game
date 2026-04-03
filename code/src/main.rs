use std::f32::consts::FRAC_PI_2;

use avian3d::prelude::*;
use bevy::{
    dev_tools::fps_overlay::{FpsOverlayConfig, FpsOverlayPlugin},
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use bevy_tnua::builtins::{
    TnuaBuiltinJump, TnuaBuiltinJumpConfig, TnuaBuiltinWalk, TnuaBuiltinWalkConfig,
};
use bevy_tnua::prelude::*;
use bevy_tnua_avian3d::prelude::*;

// --- Types ---

#[derive(Component)]
struct Player;

#[derive(TnuaScheme)]
#[scheme(basis = TnuaBuiltinWalk)]
enum PlayerActions {
    Jump(TnuaBuiltinJump),
}

#[derive(Debug, Resource)]
struct CameraSettings {
    orbit_distance: f32,
    pitch_speed: f32,
    yaw_speed: f32,
    pitch_range: std::ops::Range<f32>,
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
        ))
        .init_resource::<CameraSettings>()
        .add_systems(Startup, setup)
        .add_systems(Update, (grab_cursor, player_controls, orbit_camera).chain())
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut configs: ResMut<Assets<PlayerActionsConfig>>,
) {
    // ground
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(50.0, 50.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
        RigidBody::Static,
        Collider::half_space(Vec3::Y),
    ));
    // player
    commands.spawn((
        Player,
        Mesh3d(meshes.add(Capsule3d {
            radius: 0.5,
            half_length: 0.5,
        })),
        MeshMaterial3d(materials.add(Color::srgb_u8(124, 144, 255))),
        Transform::from_xyz(0.0, 2.0, 0.0),
        RigidBody::Dynamic,
        Collider::capsule(0.5, 1.0),
        TnuaController::<PlayerActions>::default(),
        TnuaConfig::<PlayerActions>(configs.add(PlayerActionsConfig {
            basis: TnuaBuiltinWalkConfig {
                float_height: 1.5,
                ..default()
            },
            jump: TnuaBuiltinJumpConfig {
                height: 4.0,
                ..default()
            },
        })),
        TnuaAvian3dSensorShape(Collider::cylinder(0.49, 0.0)),
        LockedAxes::ROTATION_LOCKED,
    ));
    // light
    commands.spawn((
        PointLight {
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0),
    ));
    // camera
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 5.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn grab_cursor(
    mut cursor: Single<&mut CursorOptions, With<Window>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    key_input: Res<ButtonInput<KeyCode>>,
) {
    if mouse_buttons.just_pressed(MouseButton::Left) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
    if key_input.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

fn player_controls(
    mut controller: Single<&mut TnuaController<PlayerActions>, With<Player>>,
    camera: Single<&Transform, (With<Camera>, Without<Player>)>,
    key_input: Res<ButtonInput<KeyCode>>,
) {
    const BINDINGS: [(KeyCode, Vec3); 4] = [
        (KeyCode::KeyW, Vec3::Z),
        (KeyCode::KeyS, Vec3::NEG_Z),
        (KeyCode::KeyA, Vec3::X),
        (KeyCode::KeyD, Vec3::NEG_X),
    ];

    let input: Vec3 = BINDINGS
        .iter()
        .filter(|(key, _)| key_input.pressed(*key))
        .map(|(_, dir)| *dir)
        .sum();

    // Project camera direction onto XZ plane for camera-relative movement
    let forward = camera.forward().as_vec3();
    let forward = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
    let right = Vec3::Y.cross(forward).normalize_or_zero();

    let desired_motion = forward * input.z + right * input.x;
    let desired_forward = Dir3::new(desired_motion).ok();

    controller.initiate_action_feeding();

    controller.basis = TnuaBuiltinWalk {
        desired_motion,
        desired_forward,
        ..default()
    };

    if key_input.pressed(KeyCode::Space) {
        controller.action(PlayerActions::Jump(default()));
    }
}

fn orbit_camera(
    mut camera: Single<&mut Transform, With<Camera>>,
    player: Single<&Transform, (With<Player>, Without<Camera>)>,
    mouse_motion: Res<AccumulatedMouseMotion>,
    settings: Res<CameraSettings>,
    cursor: Single<&CursorOptions, With<Window>>,
) {
    if matches!(cursor.grab_mode, CursorGrabMode::None) {
        return;
    }

    let (yaw, pitch, _) = camera.rotation.to_euler(EulerRot::YXZ);
    let pitch = (pitch + mouse_motion.delta.y * settings.pitch_speed)
        .clamp(settings.pitch_range.start, settings.pitch_range.end);
    let yaw = yaw + mouse_motion.delta.x * settings.yaw_speed;
    camera.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);

    camera.translation = player.translation - camera.forward() * settings.orbit_distance;
}

impl Default for CameraSettings {
    fn default() -> Self {
        let pitch_limit = FRAC_PI_2 - 0.01;
        Self {
            orbit_distance: 10.0,
            pitch_speed: 0.003,
            yaw_speed: 0.004,
            pitch_range: -pitch_limit..pitch_limit,
        }
    }
}
