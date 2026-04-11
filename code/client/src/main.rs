use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    f32::consts::FRAC_PI_2,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::SystemTime,
};

use avian3d::prelude::*;
use bevy::{
    camera::{visibility::RenderLayers, Exposure},
    core_pipeline::tonemapping::Tonemapping,
    dev_tools::fps_overlay::{FpsOverlayConfig, FpsOverlayPlugin},
    gizmos::config::{DefaultGizmoConfigGroup, GizmoLineJoint},
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
        view::{ColorGrading, ColorGradingGlobal, Hdr},
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
    Aiming, DEFAULT_PORT, EYE_HEIGHT_OFFSET, InputMessage, LastProcessedInput, LookDirection,
    PLAYER_CYLINDER_HEIGHT, PLAYER_RADIUS, PROTOCOL_ID, PendingCast, Player, PlayerActionsConfig,
    PlayerInput, PlayerOwner, PlayerPhysicsPlugin, SPELL_RADIUS, SharedReplicationPlugin, Spell,
    TnuaUserControlsSystems, WAND_TIP_OFFSET, WORLD_CUBES, insert_player_sim_bundle,
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

// --- Spell-pattern casting tuning ---
//
// Spells require the player to draw a pattern (currently: a vertical line
// downward from the screen center) while holding LMB. Camera is locked while
// drawing; release evaluates the trace and either fires the spell or fizzles.
// All thresholds are fractions of window height for resolution independence.

/// Length of the hint line as a fraction of window height.
const HINT_LEN_FRAC: f32 = 0.20;
/// Width of the casting overlay's hint and trace lines, in **logical**
/// pixels. Multiplied by `Window::scale_factor()` each frame before being
/// written to the gizmo config (gizmo `line.width` is in physical pixels —
/// see `bevy_gizmos_render/src/lines.wgsl:131`), so the overlay reads the
/// same visual thickness on retina and non-retina displays.
const OVERLAY_LINE_WIDTH_LOGICAL_PX: f32 = 30.0;
/// Maximum allowed lateral deviation of any sample from the hint line, as a
/// fraction of window height. Smaller = stricter shape match.
const MAX_DEVIATION_FRAC: f32 = 0.025;
/// Trace must reach at least this fraction of `hint_len` below the start at
/// some point. Catches "scribble around the start then fling to the tip".
const MIN_REACH_FRAC: f32 = 0.90;
/// Minimum distance (in screen pixels) the cursor must travel since the last
/// recorded sample before we append a new one. Keeps the buffer bounded when
/// the player is mostly still.
const SAMPLE_MIN_DIST_PX: f32 = 2.0;
/// Hard cap on the trace buffer. `SAMPLE_MIN_DIST_PX` already throttles
/// growth, but a player wiggling in tight loops can still grow it without
/// bound — and `pattern_passes` is O(N) on the trace, so unbounded growth
/// is also unbounded per-frame work. 2048 samples is ~5 m of cursor travel
/// at 2 px/sample, far more than any plausible pattern.
const TRACE_MAX_SAMPLES: usize = 2048;
/// Duration of the red-flash fizzle animation on a failed cast.
const FIZZLE_DURATION_SECS: f32 = 0.3;

/// State machine for the spell-drawing gesture.
///
/// Flow: `Idle` → (LMB down) `Drawing` → (pattern satisfied) `Aiming` →
/// (LMB up) fire spell → `Idle`. Drawing → (LMB up without satisfying) →
/// `Fizzling` → (timer) `Idle`.
///
/// `Idle`: nothing happening; single click no longer fires anything alone.
/// `Drawing`: LMB held, camera rotation frozen, hint + trace visible. Origin
///   and direction are **not** cached — they're recomputed at fire time so
///   the spell launches from the player's current position rather than from
///   wherever they clicked.
/// `Aiming`: pattern was traced correctly and the player is still holding
///   LMB. Camera rotation re-enables so the player can aim. The casting
///   overlay is hidden and the regular crosshair returns. Releasing LMB
///   fires the spell using the current camera transform; clicking again
///   does nothing because LMB is already pressed.
/// `Fizzling`: LMB released without satisfying the pattern. The trace fades
///   out red. Camera rotation is unlocked during fizzling.
#[derive(Resource, Default)]
enum CastingState {
    #[default]
    Idle,
    Drawing {
        /// Cursor position in Camera2d coords (origin = screen center, +y up).
        cursor: Vec2,
        /// Sampled cursor positions, oldest first. Always starts with `Vec2::ZERO`.
        trace: Vec<Vec2>,
    },
    Aiming,
    Fizzling {
        trace: Vec<Vec2>,
        timer: Timer,
    },
}

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

/// Marker for the white-dot crosshair UI node. Used by
/// `update_crosshair_visibility` to hide it while a spell is being drawn —
/// the casting overlay (hint + trace) replaces it during that state.
#[derive(Component)]
struct Crosshair;

/// Marker for the local-only "armed spell" preview orb. Spawned as a child of
/// the camera while `CastingState::Aiming` so it rides the camera transform
/// exactly (same trick the wand cylinder uses). Despawned the moment the
/// player leaves the aim phase — either by firing, fizzling, or pressing Esc.
/// Not replicated: only the casting player should see their armed spell.
#[derive(Component)]
struct PreviewOrb;

/// Third-person preview orb for any non-local player who is currently in
/// the aim phase. Spawned as a top-level entity by `sync_remote_preview_orbs`
/// — *not* a child of the player — because the orb's position is computed
/// from the player's `LookDirection` (the camera-forward, world space), but
/// the player's `Transform` rotation is Tnua's movement-direction facing,
/// which is unrelated. Putting the orb in world space and updating it each
/// frame is simpler than juggling parent-frame inverse transforms.
///
/// Stores its owner so the reconcile pass knows which player's
/// `Transform` + `LookDirection` to read, and can despawn the orb when the
/// owner stops aiming or disconnects.
///
/// The local player gets *no* third-person preview — they have their own
/// camera-child `PreviewOrb` for the first-person view. Other clients spawn
/// these previews on the local player's behalf when they receive the
/// replicated `Aiming` marker.
#[derive(Component)]
struct RemotePreviewOrb {
    player: Entity,
}

/// Cached handles for the player capsule visual. One mesh + one material shared
/// across every replicated player, so we don't allocate new GPU resources per spawn.
struct PlayerVisualHandles {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
}

/// Cached handles for the flame-orb visual. Same idea — one mesh + one material
/// shared across every spell that gets replicated to this client. Lives as a
/// `Resource` (not a `Local`) so all sites — `attach_spell_visuals`,
/// `sync_preview_orb`, `sync_remote_preview_orbs` — share a single mesh and
/// material in `Assets`, instead of each `Local<T: FromWorld>` minting its own.
#[derive(Resource)]
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
        .init_resource::<CastingState>()
        .init_resource::<SpellVisualHandles>()
        .add_systems(
            Startup,
            (
                setup_world,
                setup_network,
                spawn_network_overlay,
                spawn_crosshair,
                setup_overlay_gizmos,
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
                (
                    // `first_person_camera` always runs so the camera
                    // translation keeps tracking the player. The rotation
                    // freeze during drawing is handled inside the function so
                    // the player doesn't get visually teleported on release.
                    first_person_camera,
                    capture_movement_input,
                    capture_casting_input,
                    tick_fizzle,
                    update_crosshair_visibility,
                    sync_preview_orb,
                    draw_casting_overlay.run_if(is_drawing_or_fizzling),
                )
                    .chain()
                    .after(cursor_toggle)
                    .run_if(local_player_exists)
                    .run_if(cursor_engaged),
                // Other players' aim-phase preview orbs. Intentionally NOT
                // gated on `cursor_engaged` or `local_player_exists` — remote
                // players keep playing regardless of whether *this* client is
                // paused, and pressing Esc on the viewer used to wrongly
                // freeze every other player's orb.
                sync_remote_preview_orbs,
                update_gizmo_line_width
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
            // Marker so the existing UI nodes (crosshair, network overlay)
            // keep targeting THIS camera after we add the order=1 Camera2d
            // overlay below. Without this, `DefaultUiCamera::get` picks the
            // highest-order camera and the UI silently retargets to the 2D one.
            IsDefaultUiCamera,
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

    // Pixel-perfect 2D overlay camera for the spell-casting gizmos (hint
    // line, draw cursor, trace). `order: 1` puts it after the 3D camera so it
    // composites over the rendered scene; `ClearColorConfig::None` means it
    // doesn't wipe what the 3D pass drew. Camera2d's default ortho projection
    // uses `ScalingMode::WindowSize` (1 unit = 1 logical px) with the origin
    // at the screen center, so `Vec2::ZERO` in gizmo coords is dead center.
    //
    // CRITICAL: `Hdr` and `Msaa::Off` must match the Camera3d above.
    // `prepare_view_targets` (bevy_render/src/view/mod.rs:1104) keys
    // intermediate textures on `(target, texture_usage, hdr, msaa)`. If any
    // of those differ, this camera gets its OWN intermediate texture which
    // starts uninitialized (black) and ClearColorConfig::None means we don't
    // clear it — so when it composites to the window it overwrites the 3D
    // output and the screen goes black.
    //
    // `RenderLayers::layer(1)` keeps the gizmo overlay on a dedicated layer.
    // Bevy's `linestrip_2d`/`line_2d` extend Vec2 → Vec3 (z=0) and queue into
    // the same gizmo buffer the 3D pipeline reads from, so without a layer
    // filter the casting gizmos would *also* be drawn at world position
    // (cursor.x, cursor.y, 0) by Camera3d — visible as a duplicate "zoomed
    // in" copy somewhere in the scene. Pairs with the GizmoConfigStore tweak
    // in `setup_overlay_gizmos` that puts the default gizmo group on layer 1.
    commands.spawn((
        Camera2d,
        Camera {
            order: 1,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        Hdr,
        Msaa::Off,
        RenderLayers::layer(1),
    ));
}

/// Routes the default gizmo config group to render layer 1 only and enables
/// rounded line joints so the trace reads as a continuous thick stroke
/// instead of a chain of independent quads. With the default
/// `GizmoLineJoint::None`, every consecutive pair of trace samples renders
/// as two unjoined quads — at the 30 px width the player asked for, the
/// quads' perpendicular edges visibly mismatch at every angle change and the
/// trace appears to "jump left and right". Round joints stitch the gaps.
///
/// The casting overlay's Camera2d is the only thing on layer 1, so gizmos
/// render exclusively through it and don't get duplicated by Camera3d. Line
/// width is set per-frame by `update_gizmo_line_width` because it depends on
/// the live window scale factor (can change at runtime when the window moves
/// between displays).
fn setup_overlay_gizmos(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<DefaultGizmoConfigGroup>();
    config.render_layers = RenderLayers::layer(1);
    config.line.joints = GizmoLineJoint::Round(8);
}

/// Keeps `GizmoConfig::line.width` in sync with the current window scale
/// factor so the overlay reads as `OVERLAY_LINE_WIDTH_LOGICAL_PX` *logical*
/// pixels regardless of display DPI. Caches the last applied scale in a
/// `Local` and early-returns when nothing changed — `scale_factor` only
/// flips when the window moves between displays, so on every other frame
/// this avoids touching `GizmoConfigStore` and tripping its change detection.
fn update_gizmo_line_width(
    mut store: ResMut<GizmoConfigStore>,
    window: Single<&Window>,
    mut last_scale: Local<f32>,
) {
    let scale = window.scale_factor().max(1.0);
    if *last_scale == scale {
        return;
    }
    *last_scale = scale;
    let (config, _) = store.config_mut::<DefaultGizmoConfigGroup>();
    config.line.width = OVERLAY_LINE_WIDTH_LOGICAL_PX * scale;
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
/// `capture_casting_input` and start a cast.
///
/// On disengage we also reset `CastingState` to `Idle`. Otherwise, hitting
/// Esc mid-draw would leave the state machine stuck in `Drawing` and the next
/// click after re-engaging would be treated as a continuation, not a new cast.
fn cursor_toggle(
    mut engaged: ResMut<CursorEngaged>,
    mut casting: ResMut<CastingState>,
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
        *casting = CastingState::Idle;
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

/// Spawns a tiny white circle in the exact center of the screen — the GTA‑style
/// "single point" crosshair. Centered via `50%` + a negative half-size margin so
/// the dot's center, not its top-left, sits at the screen midpoint.
/// `BorderRadius::MAX` rounds the square node into a circle.
fn spawn_crosshair(mut commands: Commands) {
    const SIZE: f32 = 4.0;
    commands.spawn((
        Crosshair,
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
            border_radius: BorderRadius::MAX,
            ..default()
        },
        BackgroundColor(Color::WHITE.with_alpha(0.8)),
    ));
}

/// Hides the static crosshair while a spell is being drawn (or fizzling) so
/// it doesn't sit in the middle of the casting overlay. Visible during
/// `Idle` (normal play) and `Aiming` (the crosshair is exactly what the
/// player needs while picking a target before release).
fn update_crosshair_visibility(
    state: Res<CastingState>,
    mut crosshair: Single<&mut Visibility, With<Crosshair>>,
) {
    let wanted = match *state {
        CastingState::Idle | CastingState::Aiming => Visibility::Inherited,
        CastingState::Drawing { .. } | CastingState::Fizzling { .. } => Visibility::Hidden,
    };
    crosshair.set_if_neq(wanted);
}

/// Reconciles a top-level `RemotePreviewOrb` for every aiming non-local
/// player. Mirror of `sync_preview_orb` for the third-person view: the
/// local first-person preview is a child of the camera, this one is a
/// world-space orb positioned at the equivalent "wand tip" of a remote
/// player using their replicated `LookDirection`.
///
/// Position math mirrors what the local first-person view does: the camera
/// sits `EYE_HEIGHT_OFFSET` above the player's center, and the wand tip is
/// `WAND_TIP_OFFSET` in the camera's local frame. We reconstruct the camera
/// transform with `Transform::from_translation(...).looking_to(look, Y)` and
/// transform the wand offset through it.
///
/// The local player is filtered out via `Without<LocalPlayer>` — they have
/// their own camera-child `PreviewOrb` for first-person and don't need this
/// one. Other connected clients see the local player's preview because *they*
/// run this same system and the local player isn't *their* `LocalPlayer`.
///
/// Reconcile pass: build a `player → desired_pos` map for currently-aiming
/// non-local players, then walk existing previews. Each existing preview
/// either has its transform updated (owner still aiming) or is despawned
/// (owner stopped aiming or disconnected — both look the same here:
/// not in the map). Spawn previews for any aiming player who doesn't yet
/// have one.
fn sync_remote_preview_orbs(
    aiming_players: Query<
        (Entity, &Transform, &LookDirection),
        (With<Player>, With<Aiming>, Without<LocalPlayer>),
    >,
    mut previews: Query<(Entity, &RemotePreviewOrb, &mut Transform), Without<Player>>,
    handles: Res<SpellVisualHandles>,
    mut commands: Commands,
) {
    let desired: HashMap<Entity, Vec3> = aiming_players
        .iter()
        .map(|(entity, player_xform, look)| {
            let camera_pos = player_xform.translation + Vec3::Y * EYE_HEIGHT_OFFSET;
            let camera_xform = Transform::from_translation(camera_pos)
                .looking_to(look.0, Vec3::Y);
            (entity, camera_xform.transform_point(WAND_TIP_OFFSET))
        })
        .collect();

    let mut already_spawned = HashSet::new();
    for (orb_entity, orb_owner, mut orb_xform) in &mut previews {
        if let Some(&pos) = desired.get(&orb_owner.player) {
            orb_xform.translation = pos;
            already_spawned.insert(orb_owner.player);
        } else {
            commands.entity(orb_entity).despawn();
        }
    }

    for (player, &pos) in &desired {
        if already_spawned.contains(player) {
            continue;
        }
        commands.spawn((
            RemotePreviewOrb { player: *player },
            Mesh3d(handles.mesh.clone()),
            MeshMaterial3d(handles.material.clone()),
            Transform::from_translation(pos),
        ));
    }
}

/// Keeps a single `PreviewOrb` entity attached to the camera while the
/// player is in `Aiming`, despawning it the moment the state changes. The
/// orb is a child of the `Camera3d` entity, so its local
/// `Transform::from_translation(WAND_TIP_OFFSET)` is automatically composed
/// with the camera's global transform — the orb tracks every walk and
/// camera turn for free, no per-frame position update needed.
///
/// Reconcile-style: each frame compare desired vs. actual and add or remove
/// as needed. Handles all four ways out of `Aiming` (fire, ESC reset,
/// future fizzle path, anything else) without scattering despawn calls.
fn sync_preview_orb(
    state: Res<CastingState>,
    existing: Query<Entity, With<PreviewOrb>>,
    camera: Single<Entity, With<Camera3d>>,
    handles: Res<SpellVisualHandles>,
    mut commands: Commands,
) {
    let should_exist = matches!(*state, CastingState::Aiming);
    let existing_orb = existing.iter().next();
    match (should_exist, existing_orb) {
        (true, None) => {
            commands.entity(*camera).with_children(|parent| {
                parent.spawn((
                    PreviewOrb,
                    Mesh3d(handles.mesh.clone()),
                    MeshMaterial3d(handles.material.clone()),
                    Transform::from_translation(WAND_TIP_OFFSET),
                ));
            });
        }
        (false, Some(orb)) => {
            commands.entity(orb).despawn();
        }
        _ => {}
    }
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

/// Captures movement input (WASD + jump + Shift sneak). Click handling lives
/// in `capture_casting_input`. WASD keeps working while drawing a spell so the
/// player can dodge mid-cast.
fn capture_movement_input(
    mut input: Single<&mut PlayerInput, With<LocalPlayer>>,
    key_input: Res<ButtonInput<KeyCode>>,
    camera: Single<&Transform, With<Camera3d>>,
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
}

/// State machine for the draw-to-cast gesture. The only writer of
/// `CastingState` and the only path that enqueues a `PendingCast`.
///
/// Click-down (any state except `Drawing`/`Aiming`): enter `Drawing` with the
/// cursor seeded at the screen center. Falls through into the drag /
/// pattern-check / release handling so a same-frame click+release (rare, but
/// possible — `just_pressed` and `just_released` are independent) still gets
/// evaluated.
///
/// Drag (`Drawing`): integrate raw mouse delta into the screen-space cursor.
/// Mouse delta is +y down (winit convention); Camera2d is +y up; we negate.
///
/// Pattern complete (`Drawing` → `Aiming`): once `pattern_passes` returns
/// true, transition to `Aiming` without firing. Camera rotation re-enables
/// so the player can aim with mouse motion before committing.
///
/// Release in `Aiming` → fire: recomputes origin (current wand tip) and
/// direction (current camera ray) at fire time. Camera was free during the
/// aim phase so this is the latest aim, not the click-down aim.
///
/// Release in `Drawing` without satisfying the pattern → `Fizzling`. By
/// construction we only reach this branch when `pattern_passes` returned
/// `false` for the current trace.
fn capture_casting_input(
    mut state: ResMut<CastingState>,
    mut input: Single<&mut PlayerInput, With<LocalPlayer>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    mouse_motion: Res<AccumulatedMouseMotion>,
    camera: Single<&Transform, With<Camera3d>>,
    window: Single<&Window>,
    local_player: Single<Entity, With<LocalPlayer>>,
    spatial_query: SpatialQuery,
) {
    if matches!(*state, CastingState::Aiming) {
        if mouse_buttons.just_released(MouseButton::Left) {
            let cast = build_cast(&camera, &spatial_query, *local_player);
            input.cast_queue.push(cast);
            *state = CastingState::Idle;
        }
        return;
    }

    if mouse_buttons.just_pressed(MouseButton::Left)
        && !matches!(*state, CastingState::Drawing { .. })
    {
        *state = CastingState::Drawing {
            cursor: Vec2::ZERO,
            trace: vec![Vec2::ZERO],
        };
    }

    let CastingState::Drawing { cursor, trace } = &mut *state else {
        return;
    };

    // Mouse delta is +y down (winit convention); Camera2d is +y up; negate.
    let scale = window.scale_factor().max(1.0);
    let delta = Vec2::new(mouse_motion.delta.x, -mouse_motion.delta.y) / scale;
    *cursor += delta;
    if let Some(last) = trace.last()
        && cursor.distance(*last) >= SAMPLE_MIN_DIST_PX
        && trace.len() < TRACE_MAX_SAMPLES
    {
        trace.push(*cursor);
    }

    if pattern_passes(trace, window.height()) {
        *state = CastingState::Aiming;
        // If LMB was already released this same frame (rare but possible —
        // a fast flick can pass the pattern *and* release in one tick),
        // fire immediately. Otherwise the next frame's `just_released` is
        // false and we'd be stranded in `Aiming` until the player clicked
        // again.
        if mouse_buttons.just_released(MouseButton::Left) {
            let cast = build_cast(&camera, &spatial_query, *local_player);
            input.cast_queue.push(cast);
            *state = CastingState::Idle;
        }
        return;
    }

    if mouse_buttons.just_released(MouseButton::Left) {
        let trace = std::mem::take(trace);
        *state = CastingState::Fizzling {
            trace,
            timer: Timer::from_seconds(FIZZLE_DURATION_SECS, TimerMode::Once),
        };
    }
}

/// Builds a `PendingCast` from the current camera state. Used by the aim
/// phase: at the moment the player releases LMB, we capture origin (current
/// wand tip) and direction (camera-ray to the world point under the
/// crosshair, with the local player's collider excluded so we don't hit our
/// own capsule).
fn build_cast(
    camera: &Transform,
    spatial_query: &SpatialQuery,
    local_player: Entity,
) -> PendingCast {
    let camera_pos = camera.translation;
    let camera_fwd = camera.forward();
    let filter = SpatialQueryFilter::from_excluded_entities([local_player]);
    let aim_distance = spatial_query
        .cast_ray(camera_pos, camera_fwd, AIM_RAY_MAX_DISTANCE, true, &filter)
        .map(|hit| hit.distance)
        .unwrap_or(AIM_RAY_MAX_DISTANCE);
    let target = camera_pos + camera_fwd * aim_distance;
    let wand_tip = camera.transform_point(WAND_TIP_OFFSET);
    let direction = Dir3::new(target - wand_tip)
        .map(|d| d.as_vec3())
        .unwrap_or(camera_fwd.as_vec3());
    PendingCast {
        origin: wand_tip,
        direction,
    }
}

/// Returns `true` when the trace satisfies the casting pattern (currently:
/// vertical line drawn down from screen center). Free function so it can be
/// called inline from the auto-fire branch without borrowing the `state`.
///
/// Two checks:
/// - Maximum lateral deviation must stay within tolerance. For a vertical
///   hint anchored at `Vec2::ZERO`, perpendicular distance collapses to
///   `|p.x|`, so this is just the max absolute x across all samples.
/// - The trace's lowest y must reach at least `MIN_REACH_FRAC * hint_len`
///   below the start. Catches "scribbled near the start without going down".
fn pattern_passes(trace: &[Vec2], window_height: f32) -> bool {
    let hint_len = HINT_LEN_FRAC * window_height;
    let max_deviation = MAX_DEVIATION_FRAC * window_height;
    let min_reach_y = -MIN_REACH_FRAC * hint_len;

    let max_lateral = trace
        .iter()
        .map(|p| p.x.abs())
        .fold(0.0_f32, f32::max);
    let min_y = trace.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);

    max_lateral < max_deviation && min_y <= min_reach_y
}

/// Advances the fizzle timer and returns to `Idle` once it's done. Runs every
/// frame; when the state isn't `Fizzling` it's a no-op.
fn tick_fizzle(mut state: ResMut<CastingState>, time: Res<Time>) {
    let CastingState::Fizzling { timer, .. } = &mut *state else {
        return;
    };
    timer.tick(time.delta());
    if timer.is_finished() {
        *state = CastingState::Idle;
    }
}

/// Renders the hint line and the trace as 2D gizmos. Runs only when state is
/// `Drawing` or `Fizzling`; nothing is drawn during `Aiming` (the regular
/// crosshair is what the player needs at that point).
fn draw_casting_overlay(
    state: Res<CastingState>,
    window: Single<&Window>,
    mut gizmos: Gizmos,
) {
    let h = window.height();
    let hint_len = HINT_LEN_FRAC * h;
    let start = Vec2::ZERO;
    let end = Vec2::new(0.0, -hint_len);

    match &*state {
        CastingState::Idle | CastingState::Aiming => {}
        CastingState::Drawing { trace, .. } => {
            gizmos.line_2d(start, end, Color::srgba(1.0, 1.0, 1.0, 0.30));
            if trace.len() >= 2 {
                gizmos.linestrip_2d(trace.iter().copied(), Color::srgb(0.55, 0.95, 1.0));
            }
        }
        CastingState::Fizzling { trace, timer } => {
            let alpha = 1.0 - timer.fraction();
            if trace.len() >= 2 {
                gizmos.linestrip_2d(
                    trace.iter().copied(),
                    Color::srgba(1.0, 0.2, 0.2, alpha),
                );
            }
        }
    }
}

fn is_drawing_or_fizzling(state: Res<CastingState>) -> bool {
    matches!(
        *state,
        CastingState::Drawing { .. } | CastingState::Fizzling { .. }
    )
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
    casting: Res<CastingState>,
    camera: Single<&Transform, With<Camera3d>>,
) {
    tick.0 = tick.0.wrapping_add(1);

    let cast = (!input.cast_queue.is_empty()).then(|| input.cast_queue.remove(0));
    let respawn = std::mem::take(&mut input.respawn);
    let aiming = matches!(*casting, CastingState::Aiming);
    let look_forward = camera.forward().as_vec3();

    commands.client_trigger(InputMessage {
        tick: tick.0,
        desired_motion: input.desired_motion,
        jump: input.jump,
        cast,
        respawn,
        aiming,
        look_forward,
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

/// Updates the camera each frame: translation always tracks the player so the
/// view doesn't lag behind the body, but rotation only consumes mouse motion
/// when the player isn't drawing a spell. Without the always-on translation,
/// freezing rotation during casting would also peg the camera to the
/// pre-cast position and the player would visually "teleport" forward on
/// release as the camera caught up.
fn first_person_camera(
    mut camera: Single<&mut Transform, With<Camera3d>>,
    player: Single<&Transform, (With<LocalPlayer>, Without<Camera3d>)>,
    mouse_motion: Res<AccumulatedMouseMotion>,
    settings: Res<CameraSettings>,
    casting: Res<CastingState>,
) {
    if !matches!(*casting, CastingState::Drawing { .. }) {
        let (yaw, pitch, _) = camera.rotation.to_euler(EulerRot::YXZ);
        let pitch = (pitch - mouse_motion.delta.y * settings.pitch_speed)
            .clamp(settings.pitch_range.start, settings.pitch_range.end);
        let yaw = yaw - mouse_motion.delta.x * settings.yaw_speed;
        camera.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);
    }

    camera.translation = player.translation + Vec3::Y * EYE_HEIGHT_OFFSET;
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
    handles: Res<SpellVisualHandles>,
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
