use bevy::{prelude::*, window::WindowMode};
use bevy_third_person_camera::{
    ThirdPersonCamera, ThirdPersonCameraPlugin, ThirdPersonCameraTarget, Zoom,
};

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                // Make fullscreen
                primary_window: Some(Window {
                    mode: WindowMode::BorderlessFullscreen,
                    ..default()
                }),
                ..default()
            }),
            ThirdPersonCameraPlugin,
        ))
        .add_systems(Startup, (setup_camera, setup_light, setup_scene))
        .add_systems(Update, player_movement)
        .run();
}

#[derive(Component)]
struct Player;

#[derive(Component)]
struct Speed {
    value: f32,
}

fn setup_scene(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn(PbrBundle {
        mesh: meshes.add(Mesh::from(shape::Plane {
            size: 15.0,
            subdivisions: 300,
        })),
        material: materials.add(Color::GREEN.into()),
        ..default()
    });

    commands.spawn((
        Player,
        PbrBundle {
            mesh: meshes.add(Mesh::from(shape::Cylinder {
                height: 2.0,
                radius: 0.6,
                resolution: 50,
                segments: 50,
            })),
            material: materials.add(Color::rgb_u8(124, 144, 255).into()),
            transform: Transform::from_xyz(0.0, 0.5, 0.0),
            ..default()
        },
        Speed { value: 2.5 },
        ThirdPersonCameraTarget,
    ));
}

fn player_movement(
    time: Res<Time>,
    mut player_query: Query<(&mut Transform, &Speed), With<Player>>,
    camera_query: Query<&Transform, (With<Camera>, Without<Player>)>,
    keys: Res<Input<KeyCode>>,
) {
    use KeyCode as K;

    let (mut player_transform, player_speed) = player_query
        .get_single_mut()
        .expect("no or more than one player");

    let Ok(camera) = camera_query.get_single() else {
        return Err("couldn't fetch camera").unwrap();
    };

    let mut direction = Vec3::ZERO;

    if keys.pressed(K::W) {
        direction += camera.forward();
    }

    if keys.pressed(K::A) {
        direction += camera.left();
    }

    if keys.pressed(K::S) {
        direction += camera.back();
    }

    if keys.pressed(K::D) {
        direction += camera.right();
    }

    direction.y = 0.0;

    let movement = direction.normalize_or_zero() * player_speed.value * time.delta_seconds();
    player_transform.translation += movement;
}

fn setup_camera(mut commands: Commands) {
    commands.spawn((
        Camera3dBundle::default(),
        ThirdPersonCamera {
            zoom: Zoom::new(3.0, 15.0),
            ..default()
        },
    ));
}

fn setup_light(mut commands: Commands) {
    commands.spawn(PointLightBundle {
        point_light: PointLight {
            intensity: 1_500.0,
            shadows_enabled: true,
            ..default()
        },
        transform: Transform::from_xyz(4.0, 8.0, 4.0),
        ..default()
    });

    commands.insert_resource(AmbientLight {
        color: Color::BISQUE,
        brightness: 0.05,
    });
}
