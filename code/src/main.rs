use std::f32::consts::FRAC_PI_2;

use bevy::{
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};

#[derive(Component)]
struct Player;

#[derive(Debug, Resource)]
struct CameraSettings {
    orbit_distance: f32,
    pitch_speed: f32,
    yaw_speed: f32,
    pitch_range: std::ops::Range<f32>,
    move_speed: f32,
}

impl Default for CameraSettings {
    fn default() -> Self {
        let pitch_limit = FRAC_PI_2 - 0.01;
        Self {
            orbit_distance: 10.0,
            pitch_speed: 0.003,
            yaw_speed: 0.004,
            pitch_range: -pitch_limit..pitch_limit,
            move_speed: 5.0,
        }
    }
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .init_resource::<CameraSettings>()
        .add_systems(Startup, setup)
        .add_systems(Update, (grab_cursor, player_movement, orbit_camera).chain())
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // circular base
    commands.spawn((
        Mesh3d(meshes.add(Circle::new(4.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
        Transform::from_rotation(Quat::from_rotation_x(-FRAC_PI_2)),
    ));
    // player cube
    commands.spawn((
        Player,
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(materials.add(Color::srgb_u8(124, 144, 255))),
        Transform::from_xyz(0.0, 0.5, 0.0),
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

fn player_movement(
    mut player: Single<&mut Transform, With<Player>>,
    camera: Single<&Transform, (With<Camera>, Without<Player>)>,
    key_input: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    settings: Res<CameraSettings>,
) {
    let mut input = Vec3::ZERO;
    if key_input.pressed(KeyCode::KeyW) {
        input.z += 1.0;
    }
    if key_input.pressed(KeyCode::KeyS) {
        input.z -= 1.0;
    }
    if key_input.pressed(KeyCode::KeyA) {
        input.x += 1.0;
    }
    if key_input.pressed(KeyCode::KeyD) {
        input.x -= 1.0;
    }

    if input == Vec3::ZERO {
        return;
    }

    // Move relative to camera's facing direction (projected onto XZ plane)
    let forward = camera.forward().as_vec3();
    let forward = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
    let right = Vec3::Y.cross(forward).normalize_or_zero();

    let movement = (forward * input.z + right * input.x).normalize_or_zero();
    player.translation += movement * settings.move_speed * time.delta_secs();
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

    let delta = mouse_motion.delta;
    let delta_pitch = delta.y * settings.pitch_speed;
    let delta_yaw = delta.x * settings.yaw_speed;

    let (yaw, pitch, _roll) = camera.rotation.to_euler(EulerRot::YXZ);
    let pitch = (pitch + delta_pitch).clamp(settings.pitch_range.start, settings.pitch_range.end);
    let yaw = yaw + delta_yaw;
    camera.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);

    let target = player.translation;
    camera.translation = target - camera.forward() * settings.orbit_distance;
}
