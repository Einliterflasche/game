use std::{
    env,
    f32::consts::FRAC_PI_2,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::SystemTime,
};

use bevy::{
    camera::Exposure,
    core_pipeline::tonemapping::Tonemapping,
    dev_tools::fps_overlay::{FpsOverlayConfig, FpsOverlayPlugin},
    input::mouse::AccumulatedMouseMotion,
    light::{AtmosphereEnvironmentMapLight, CascadeShadowConfigBuilder, light_consts::lux},
    pbr::{Atmosphere, AtmosphereSettings, ScatteringMedium},
    post_process::bloom::Bloom,
    prelude::*,
    render::{
        render_resource::AsBindGroup,
        view::{ColorGrading, ColorGradingGlobal},
    },
    shader::ShaderRef,
    window::{CursorGrabMode, CursorOptions},
};

use bevy_replicon::prelude::*;
use bevy_replicon_renet::{
    RenetChannelsExt, RenetClient, RepliconRenetPlugins,
    netcode::{ClientAuthentication, NetcodeClientTransport},
    renet::ConnectionConfig,
};
use shared::{
    DEFAULT_PORT, InputMessage, PLAYER_CYLINDER_HEIGHT, PLAYER_RADIUS, PROTOCOL_ID, PendingCast,
    Player, PlayerInput, PlayerOwner, SPELL_RADIUS, SharedReplicationPlugin, Spell,
    WAND_TIP_OFFSET, WORLD_CUBES,
};

// --- Types ---

#[derive(Debug, Resource)]
struct CameraSettings {
    pitch_speed: f32,
    yaw_speed: f32,
    pitch_range: std::ops::Range<f32>,
}

/// Our own client id, captured at connect time. Used to identify which
/// replicated `Player` entity is ours.
#[derive(Resource, Clone, Copy)]
struct LocalClientId(u64);

/// Marker for the player entity that belongs to this client.
#[derive(Component)]
struct LocalPlayer;

/// Marker for the on-screen network status text node.
#[derive(Component)]
struct NetworkOverlay;

/// Cached handles for the player capsule visual. One mesh + one material shared
/// across every replicated player, so we don't allocate new GPU resources per spawn.
struct PlayerVisualHandles {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
}

/// Cached handles for the flame-orb visual. Same idea — one mesh + one material
/// shared across every spell that gets replicated to this client.
struct SpellVisualHandles {
    mesh: Handle<Mesh>,
    material: Handle<FlameOrbMaterial>,
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
            RepliconPlugins,
            RepliconRenetPlugins,
            SharedReplicationPlugin,
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
        // Match the server's tick rate so `send_input` fires at the same cadence
        // the server consumes inputs, instead of the default 64 Hz.
        .insert_resource(Time::<Fixed>::from_hz(128.0))
        .init_resource::<CameraSettings>()
        .add_systems(
            Startup,
            (setup_world, setup_network, lock_cursor, spawn_network_overlay),
        )
        .add_systems(
            Update,
            (
                update_network_overlay,
                (first_person_camera, capture_input)
                    .chain()
                    .run_if(local_player_exists),
            ),
        )
        .add_systems(FixedUpdate, send_input.run_if(local_player_exists))
        .add_observer(attach_player_visuals)
        .add_observer(attach_spell_visuals)
        .add_observer(mark_local_player)
        .run();
}

fn setup_world(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
) {
    // ground visual (no collider — the server is authoritative for physics and the
    // client doesn't need local collision).
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(1000.0, 1000.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
    ));
    let cube_material = materials.add(Color::srgb(0.7, 0.7, 0.7));
    for (size, pos) in WORLD_CUBES {
        commands.spawn((
            Mesh3d(meshes.add(Cuboid::new(*size, *size, *size))),
            MeshMaterial3d(cube_material.clone()),
            Transform::from_translation(*pos),
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
    commands
        .spawn((
            Camera3d::default(),
            Transform::from_translation(Vec3::Y * 2.0),
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
            Mesh3d(meshes.add(Cylinder {
                radius: 0.015,
                half_height: 0.2,
            })),
            MeshMaterial3d(materials.add(Color::srgb(0.4, 0.25, 0.1))),
            Transform::from_xyz(0.25, -0.15, -0.4)
                .with_rotation(Quat::from_rotation_x(FRAC_PI_2)),
        ));
}

fn setup_network(mut commands: Commands, channels: Res<RepliconChannels>) {
    // Server address: `cargo run -p client -- 192.168.1.5:5000` or default to localhost.
    let args: Vec<String> = env::args().skip(1).collect();
    let server_addr: SocketAddr = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT));

    let client = RenetClient::new(ConnectionConfig {
        server_channels_config: channels.server_configs(),
        client_channels_config: channels.client_configs(),
        ..Default::default()
    });

    let current_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock before UNIX epoch");
    let client_id = current_time.as_millis() as u64;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("failed to bind client UDP");
    let authentication = ClientAuthentication::Unsecure {
        client_id,
        protocol_id: PROTOCOL_ID,
        server_addr,
        user_data: None,
    };
    let transport = NetcodeClientTransport::new(current_time, authentication, socket)
        .expect("failed to build netcode client transport");

    commands.insert_resource(client);
    commands.insert_resource(transport);
    commands.insert_resource(LocalClientId(client_id));

    info!("connecting to {server_addr} as client id {client_id}");
}

fn lock_cursor(mut cursor: Single<&mut CursorOptions, With<Window>>) {
    cursor.grab_mode = CursorGrabMode::Locked;
    cursor.visible = false;
}

/// Spawns a small absolute-positioned text node showing connection state and RTT.
/// Sits in the top-right corner so it doesn't overlap the FPS overlay (top-left).
///
/// The node has a fixed width so the left edge stays put when the RTT digits
/// change (e.g. 9ms → 12ms) — otherwise a content-sized right-anchored node
/// would shift its left edge each time the character count changed, and every
/// character to the left of the digit would jitter.
fn spawn_network_overlay(mut commands: Commands) {
    commands.spawn((
        NetworkOverlay,
        Text::new("Net: ?"),
        TextFont {
            font_size: 18.0,
            ..default()
        },
        TextColor(Color::srgb(0.8, 0.9, 1.0)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(0.0),
            right: Val::Px(8.0),
            width: Val::Px(280.0),
            ..default()
        },
    ));
}

/// Updates the network overlay text from replicon's `ClientState` + `ClientStats`.
///
/// RTT and packet-loss numbers are zero-padded to a fixed digit count so the
/// label "ms" / "%" stays put as the value changes — proportional fonts give
/// digits a uniform width but spaces are narrower, so space-padding doesn't
/// work, only zero-padding does.
fn update_network_overlay(
    state: Res<State<ClientState>>,
    stats: Res<ClientStats>,
    mut text: Single<&mut Text, With<NetworkOverlay>>,
) {
    let status = match state.get() {
        ClientState::Disconnected => "Disconnected",
        ClientState::Connecting => "Connecting",
        ClientState::Connected => "Connected",
    };
    text.0 = if matches!(state.get(), ClientState::Connected) {
        let rtt_ms = (stats.rtt * 1000.0).round().clamp(0.0, 999.0) as u32;
        let loss_pct = (stats.packet_loss * 100.0).clamp(0.0, 99.9);
        format!("Net: {status}  RTT: {rtt_ms:03}ms  loss: {loss_pct:04.1}%")
    } else {
        format!("Net: {status}")
    };
}

fn local_player_exists(query: Query<(), With<LocalPlayer>>) -> bool {
    !query.is_empty()
}

fn capture_input(
    mut input: Single<&mut PlayerInput, With<LocalPlayer>>,
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
    let sprint = if key_input.pressed(KeyCode::ShiftLeft) {
        3.0
    } else {
        1.0
    };

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

/// Sends one tick of `PlayerInput` state to the server as an RPC, consuming one
/// queued cast event (FIFO) and the respawn latch. Remaining casts stay in the
/// queue for subsequent ticks, so rapid clicks are processed in order rather than
/// being dropped.
fn send_input(mut commands: Commands, mut input: Single<&mut PlayerInput, With<LocalPlayer>>) {
    let cast = (!input.cast_queue.is_empty()).then(|| input.cast_queue.remove(0));
    let respawn = std::mem::take(&mut input.respawn);

    commands.client_trigger(InputMessage {
        desired_motion: input.desired_motion,
        jump: input.jump,
        cast,
        respawn,
    });
}

fn first_person_camera(
    mut camera: Single<&mut Transform, With<Camera>>,
    player: Single<&Transform, (With<LocalPlayer>, Without<Camera>)>,
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

/// Attaches the cached capsule mesh + material to any entity that gains the `Player`
/// component, plus a local `PlayerInput` so input capture has somewhere to write.
fn attach_player_visuals(
    insert: On<Insert, Player>,
    handles: Local<PlayerVisualHandles>,
    mut commands: Commands,
) {
    commands.entity(insert.entity).insert((
        PlayerInput::default(),
        Mesh3d(handles.mesh.clone()),
        MeshMaterial3d(handles.material.clone()),
    ));
}

/// Attaches the cached flame-orb mesh + material to any entity that gains the
/// `Spell` component. Sharing handles avoids per-spell GPU resource allocation.
fn attach_spell_visuals(
    insert: On<Insert, Spell>,
    handles: Local<SpellVisualHandles>,
    mut commands: Commands,
) {
    commands.entity(insert.entity).insert((
        Mesh3d(handles.mesh.clone()),
        MeshMaterial3d(handles.material.clone()),
    ));
}

/// Marks the replicated Player entity whose `PlayerOwner` matches our own client id as the
/// `LocalPlayer`, so the camera and input systems can latch onto it.
fn mark_local_player(
    insert: On<Insert, PlayerOwner>,
    local: Option<Res<LocalClientId>>,
    owners: Query<&PlayerOwner>,
    mut commands: Commands,
) {
    let Some(local) = local else { return };
    let Ok(owner) = owners.get(insert.entity) else {
        return;
    };
    if owner.0 == local.0 {
        info!("local player identified: {}", insert.entity);
        commands.entity(insert.entity).insert(LocalPlayer);
    }
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

impl FromWorld for PlayerVisualHandles {
    fn from_world(world: &mut World) -> Self {
        let mesh = world.resource_mut::<Assets<Mesh>>().add(Capsule3d {
            radius: PLAYER_RADIUS,
            half_length: PLAYER_CYLINDER_HEIGHT / 2.0,
        });
        let material = world
            .resource_mut::<Assets<StandardMaterial>>()
            .add(Color::srgb(0.5, 0.5, 0.5));
        Self { mesh, material }
    }
}

impl FromWorld for SpellVisualHandles {
    fn from_world(world: &mut World) -> Self {
        let mesh = world
            .resource_mut::<Assets<Mesh>>()
            .add(Sphere { radius: SPELL_RADIUS });
        let material = world
            .resource_mut::<Assets<FlameOrbMaterial>>()
            .add(FlameOrbMaterial {
                core_color: LinearRgba::new(10.0, 6.0, 1.5, 1.0),
                flame_speed: 1.0,
            });
        Self { mesh, material }
    }
}
