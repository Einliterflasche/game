use std::{
    collections::VecDeque,
    env,
    f32::consts::FRAC_PI_2,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::SystemTime,
};

use avian3d::prelude::*;
use bevy::{
    camera::Exposure,
    core_pipeline::tonemapping::Tonemapping,
    dev_tools::fps_overlay::{FpsOverlayConfig, FpsOverlayPlugin},
    input::mouse::AccumulatedMouseMotion,
    light::{
        AtmosphereEnvironmentMapLight, CascadeShadowConfigBuilder, DirectionalLightShadowMap,
        light_consts::lux,
    },
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
    DEFAULT_PORT, InputMessage, LastProcessedInput, PLAYER_CYLINDER_HEIGHT, PLAYER_RADIUS,
    PROTOCOL_ID, PendingCast, Player, PlayerActionsConfig, PlayerInput, PlayerOwner,
    PlayerPhysicsPlugin, SPELL_RADIUS, SharedReplicationPlugin, Spell, TnuaUserControlsSystems,
    WAND_TIP_OFFSET, WORLD_CUBES, insert_player_sim_bundle,
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

/// Whether the game window currently owns the mouse. `false` at startup —
/// the player must click to engage; ESC releases. Camera look + gameplay
/// input are gated on this so the cursor can be moved freely while paused.
#[derive(Resource, Default)]
struct CursorEngaged(bool);

/// Client-local monotonic tick counter, advanced once per FixedUpdate.
///
/// Stamped onto every outgoing `InputMessage` so the server can echo it back
/// via `LastProcessedInput`. Reconciliation (Step 2) will use this to align
/// server snapshots with the client's predicted-state history.
#[derive(Resource, Default, Clone, Copy)]
struct ClientTick(u32);

/// Marker for the player entity that belongs to this client.
#[derive(Component)]
struct LocalPlayer;

/// Ring buffer of `(client_tick, post-physics transform)` for the local player,
/// recorded each `FixedPostUpdate`. Reconciliation looks up the entry whose
/// tick matches the server's `LastProcessedInput` to compare predicted vs
/// authoritative state.
///
/// Capped at `PREDICTED_HISTORY_CAPACITY` entries (~2 s at 128 Hz). Older
/// entries fall off the front; if the server's ack is older than the buffer,
/// we treat it as "too old to reconcile" and accept the snapshot.
#[derive(Component, Default)]
struct PredictedHistory {
    entries: VecDeque<(u32, Transform)>,
}

/// Max number of (tick, transform) entries kept on `PredictedHistory`.
/// 256 ticks ≈ 2 s at 128 Hz — comfortably more than any plausible RTT.
const PREDICTED_HISTORY_CAPACITY: usize = 256;

/// Maximum distance the aim raycast travels from the camera looking for a
/// target the crosshair is over. Past this we fall back to a virtual point
/// at this distance along the camera ray. 100 m comfortably exceeds the spell
/// reach (`SPELL_SPEED * SPELL_LIFETIME = 36 m`) so anything farther is moot.
const AIM_RAY_MAX_DISTANCE: f32 = 100.0;

/// Distance threshold (meters) above which a server snapshot is treated as a
/// real mispredict and snapped to. Below this, the client keeps its prediction
/// and ignores the server's correction.
///
/// Steady-state drift in normal play is ~0.07 m (Tnua spring oscillation phase
/// difference between client and server), so 0.5 m gives ample headroom while
/// still catching real desyncs like walking through geometry or teleports.
const RECONCILE_SNAP_THRESHOLD: f32 = 0.5;

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
            PlayerPhysicsPlugin,
        ))
        .insert_resource(GlobalAmbientLight::NONE)
        // 1024² is plenty for a 50 m shadow range and ~quarters memory + bandwidth
        // versus the 2048² default.
        .insert_resource(DirectionalLightShadowMap { size: 1024 })
        // Match the server's tick rate so `send_input` fires at the same cadence
        // the server consumes inputs.
        .insert_resource(Time::<Fixed>::from_hz(64.0))
        .init_resource::<CameraSettings>()
        .init_resource::<ClientTick>()
        .init_resource::<CursorEngaged>()
        .add_systems(
            Startup,
            (
                setup_world,
                setup_network,
                spawn_network_overlay,
                spawn_crosshair,
            ),
        )
        .add_systems(
            PreUpdate,
            reconcile_local_player
                .after(ClientSystems::Receive)
                .run_if(local_player_exists),
        )
        .add_systems(
            Update,
            (
                update_network_overlay,
                cursor_toggle,
                (first_person_camera, capture_input)
                    .chain()
                    .after(cursor_toggle)
                    .run_if(local_player_exists)
                    .run_if(cursor_engaged),
            ),
        )
        .add_systems(
            FixedUpdate,
            send_input
                .before(TnuaUserControlsSystems)
                .run_if(local_player_exists),
        )
        .add_systems(
            FixedPostUpdate,
            record_predicted_state
                .after(PhysicsSystems::Writeback)
                .run_if(local_player_exists),
        )
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
    // Ground + cubes carry both the visual mesh AND the static collider, so the
    // client-side physics world (used to predict the local player) has something
    // to walk on. Server has its own collider-only copies.
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(1000.0, 1000.0))),
        MeshMaterial3d(materials.add(Color::WHITE)),
        RigidBody::Static,
        Collider::half_space(Vec3::Y),
    ));
    let cube_material = materials.add(Color::srgb(0.7, 0.7, 0.7));
    for (size, pos) in WORLD_CUBES {
        commands.spawn((
            Mesh3d(meshes.add(Cuboid::new(*size, *size, *size))),
            MeshMaterial3d(cube_material.clone()),
            Transform::from_translation(*pos),
            RigidBody::Static,
            Collider::cuboid(*size, *size, *size),
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
            num_cascades: 2,
            first_cascade_far_bound: 8.0,
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
            // Bloom + tonemapping already smooth the image; MSAA's per-edge
            // fragment-shader cost isn't worth it on the current pipeline.
            Msaa::Off,
            Atmosphere::earthlike(scattering_mediums.add(ScatteringMedium::default())),
            // Sun is static, so the LUTs barely change frame-to-frame. Halve every
            // axis vs the defaults — still imperceptible after the env-map blur.
            AtmosphereSettings {
                transmittance_lut_size: UVec2::new(128, 64),
                transmittance_lut_samples: 20,
                multiscattering_lut_dirs: 32,
                multiscattering_lut_samples: 10,
                sky_view_lut_size: UVec2::new(192, 108),
                sky_view_lut_samples: 8,
                aerial_view_lut_samples: 6,
                ..default()
            },
            // 128² cubemap is more than enough for IBL — the env map gets
            // heavily blurred when sampled. Default 512² wastes ~16× the work.
            AtmosphereEnvironmentMapLight {
                size: UVec2::new(128, 128),
                ..default()
            },
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

/// Engages the cursor on a left click and releases it on Escape. Runs every
/// frame regardless of player state — you should be able to free the mouse
/// even before connecting. The engaging click is consumed via
/// `clear_just_pressed` so the same press doesn't immediately fall through to
/// `capture_input` and fire a spell.
fn cursor_toggle(
    mut engaged: ResMut<CursorEngaged>,
    mut cursor: Single<&mut CursorOptions, With<Window>>,
    mut mouse_buttons: ResMut<ButtonInput<MouseButton>>,
    key_input: Res<ButtonInput<KeyCode>>,
) {
    if !engaged.0 {
        if mouse_buttons.just_pressed(MouseButton::Left) {
            engaged.0 = true;
            cursor.grab_mode = CursorGrabMode::Locked;
            cursor.visible = false;
            mouse_buttons.clear_just_pressed(MouseButton::Left);
        }
    } else if key_input.just_pressed(KeyCode::Escape) {
        engaged.0 = false;
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

fn cursor_engaged(engaged: Res<CursorEngaged>) -> bool {
    engaged.0
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

/// Spawns a tiny white square in the exact center of the screen — the GTA‑style
/// "single point" crosshair. Centered via `50%` + a negative half-size margin so
/// the dot's center, not its top-left, sits at the screen midpoint.
fn spawn_crosshair(mut commands: Commands) {
    const SIZE: f32 = 4.0;
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(50.0),
            width: Val::Px(SIZE),
            height: Val::Px(SIZE),
            margin: UiRect {
                left: Val::Px(-SIZE / 2.0),
                top: Val::Px(-SIZE / 2.0),
                ..default()
            },
            ..default()
        },
        BackgroundColor(Color::WHITE),
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
    local_player: Single<Entity, With<LocalPlayer>>,
    spatial_query: SpatialQuery,
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
    // Default cadence is the brisk pace (was the old "sprint"); Shift slows
    // down for sneaking. Tnua multiplies this against `walk.speed = 1.4` so
    // 3.0 ≈ 4.2 m/s default and 1.0 ≈ 1.4 m/s while sneaking.
    let speed_mult = if key_input.pressed(KeyCode::ShiftLeft) {
        1.0
    } else {
        3.0
    };

    input.desired_motion = (forward * local.z + right * local.x) * speed_mult;
    input.jump = key_input.pressed(KeyCode::Space);

    if mouse_buttons.just_pressed(MouseButton::Left) {
        // Aim ray from the camera (the crosshair is the camera ray, not the
        // wand ray). Exclude the local player's own collider so we don't
        // immediately hit the capsule we're standing inside.
        let camera_pos = camera.translation;
        let camera_fwd = camera.forward();
        let filter = SpatialQueryFilter::from_excluded_entities([*local_player]);
        let aim_distance = spatial_query
            .cast_ray(camera_pos, camera_fwd, AIM_RAY_MAX_DISTANCE, true, &filter)
            .map(|hit| hit.distance)
            .unwrap_or(AIM_RAY_MAX_DISTANCE);
        let target = camera_pos + camera_fwd * aim_distance;

        // The spell still leaves the wand tip, but its direction now aims at
        // the world point under the crosshair instead of being parallel to
        // the camera forward (which had a 25 cm parallax offset).
        let wand_tip = camera.transform_point(WAND_TIP_OFFSET);
        let direction = Dir3::new(target - wand_tip)
            .map(|d| d.as_vec3())
            .unwrap_or(camera_fwd.as_vec3());

        input.cast_queue.push(PendingCast {
            origin: wand_tip,
            direction,
        });
    }
}

/// Sends one tick of `PlayerInput` state to the server as an RPC, consuming one
/// queued cast event (FIFO) and the respawn latch. Remaining casts stay in the
/// queue for subsequent ticks, so rapid clicks are processed in order rather than
/// being dropped.
///
/// Advances `ClientTick` once per call (once per FixedUpdate) and stamps the
/// outgoing message with it. The server records the latest applied tick on
/// `LastProcessedInput` so the client can later align reconciliation against
/// its own predicted-state history at the same tick.
fn send_input(
    mut commands: Commands,
    mut input: Single<&mut PlayerInput, With<LocalPlayer>>,
    mut tick: ResMut<ClientTick>,
) {
    tick.0 = tick.0.wrapping_add(1);

    let cast = (!input.cast_queue.is_empty()).then(|| input.cast_queue.remove(0));
    let respawn = std::mem::take(&mut input.respawn);

    commands.client_trigger(InputMessage {
        tick: tick.0,
        desired_motion: input.desired_motion,
        jump: input.jump,
        cast,
        respawn,
    });
}

/// Records the local player's post-physics transform tagged with the current
/// `ClientTick`. Runs in `FixedPostUpdate`, so by the time it sees the
/// transform, both `player_controls` and Avian's integration step have run.
fn record_predicted_state(
    history: Single<(&mut PredictedHistory, &Transform), With<LocalPlayer>>,
    tick: Res<ClientTick>,
) {
    let (mut history, transform) = history.into_inner();
    history.entries.push_back((tick.0, *transform));
    while history.entries.len() > PREDICTED_HISTORY_CAPACITY {
        history.entries.pop_front();
    }
}

/// Compares the latest server snapshot for the local player against the
/// predicted state at the same client tick. If they're close, we keep the
/// prediction (overwriting the server's authoritative `Transform` with our
/// latest predicted one). If they diverge by more than `RECONCILE_SNAP_THRESHOLD`,
/// we accept the server's value as a hard snap, also writing it into Avian's
/// `Position` so the next physics step actually uses the corrected starting
/// point (without this, `transform_to_position` skips the sync because
/// `Position` was just written by the prior physics step, and the snap is
/// silently undone).
///
/// On snap we also clear the entire history — all in-flight predictions are
/// based on the pre-snap trajectory and would cause cascading mispredicts.
///
/// Runs in `PreUpdate` after replicon's receive set, so the `Transform` and
/// `LastProcessedInput` we read are the just-applied snapshot. Skips frames
/// where `LastProcessedInput` didn't change — `Query` (not `Single`) is needed
/// because the `Changed` filter returns zero matches on most frames.
fn reconcile_local_player(
    mut local: Query<
        (
            &mut Transform,
            &mut Position,
            &mut PredictedHistory,
            &LastProcessedInput,
        ),
        (With<LocalPlayer>, Changed<LastProcessedInput>),
    >,
) {
    let Ok((mut transform, mut position, mut history, ack)) = local.single_mut() else {
        return;
    };
    let acked_tick = ack.0;

    let Some(predicted_at_ack) = history
        .entries
        .iter()
        .find(|(t, _)| *t == acked_tick)
        .map(|(_, transform)| *transform)
    else {
        // Server's ack is older than our buffer (or hasn't matched any tick yet).
        // Accept the server transform as-is and keep predicting forward.
        return;
    };

    let drift = transform
        .translation
        .distance(predicted_at_ack.translation);

    if drift > RECONCILE_SNAP_THRESHOLD {
        info!("reconcile snap: drift={drift:.2}m at ack tick {acked_tick}");
        // Hard snap: server's transform is authoritative. We must also write
        // Avian's Position; otherwise the next physics step integrates from
        // the (still-stale) Position and reverts our snap.
        position.0 = transform.translation;
        history.entries.clear();
        return;
    }

    // Prediction matched. Restore the latest predicted transform so the local
    // player keeps moving forward without snapping back to the server's older
    // position. Drop history entries the server has already acked.
    if let Some((_, latest)) = history.entries.back().copied() {
        *transform = latest;
    }
    history.entries.retain(|(t, _)| *t > acked_tick);
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
/// component. Physics + `PlayerInput` are NOT inserted here — they're added only on
/// the local player by `mark_local_player`, since remote players are pure visuals.
fn attach_player_visuals(
    insert: On<Insert, Player>,
    handles: Local<PlayerVisualHandles>,
    mut commands: Commands,
) {
    commands.entity(insert.entity).insert((
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
/// `LocalPlayer`, then attaches the full physics + character-controller bundle so the
/// client can locally simulate it (client-side prediction). Remote players don't get
/// the bundle and remain pure visuals driven by replicated `Transform`.
fn mark_local_player(
    insert: On<Insert, PlayerOwner>,
    local: Option<Res<LocalClientId>>,
    owners: Query<&PlayerOwner>,
    mut configs: ResMut<Assets<PlayerActionsConfig>>,
    mut commands: Commands,
) {
    let Some(local) = local else { return };
    let Ok(owner) = owners.get(insert.entity) else {
        return;
    };
    if owner.0 != local.0 {
        return;
    }
    info!("local player identified: {}", insert.entity);
    let mut entity = commands.entity(insert.entity);
    entity.insert((LocalPlayer, PredictedHistory::default()));
    insert_player_sim_bundle(&mut entity, &mut configs);
}

impl Material for FlameOrbMaterial {
    fn vertex_shader() -> ShaderRef {
        "shaders/flame_orb.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "shaders/flame_orb.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // Mask gives us depth-write + early-Z so the orb stops paying full
        // overdraw cost on every overlapping fragment. The shader does its
        // own discard at this threshold.
        AlphaMode::Mask(0.5)
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
