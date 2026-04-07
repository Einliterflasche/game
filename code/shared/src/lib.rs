use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy_tnua::builtins::{
    TnuaBuiltinJump, TnuaBuiltinJumpConfig, TnuaBuiltinWalk, TnuaBuiltinWalkConfig,
};
use bevy_tnua::prelude::*;
use bevy_tnua_avian3d::prelude::*;
use serde::{Deserialize, Serialize};

/// Re-exported for the client so it can order systems relative to Tnua's user
/// control set without depending on bevy_tnua directly.
pub use bevy_tnua::prelude::TnuaUserControlsSystems;

// --- Constants ---

pub const DEFAULT_PORT: u16 = 5000;
pub const PROTOCOL_ID: u64 = 0x5f00_7072_6a5f_6765; // arbitrary, shared server/client

pub const PLAYER_SPAWN: Vec3 = Vec3::new(0.0, 2.0, 0.0);
pub const PLAYER_RADIUS: f32 = 0.25;
pub const PLAYER_CYLINDER_HEIGHT: f32 = 1.0;
pub const WAND_TIP_OFFSET: Vec3 = Vec3::new(0.25, -0.15, -0.7);

pub const SPELL_SPEED: f32 = 12.0;
pub const SPELL_LIFETIME: f32 = 3.0;
pub const SPELL_RADIUS: f32 = 0.1;

/// Static level geometry: `(size, position)` for each reference cube.
/// Duplicated on the client (with meshes) and server (colliders only).
pub const WORLD_CUBES: &[(f32, Vec3)] = &[
    (1.0, Vec3::new(3.0, 0.5, -3.0)),
    (2.0, Vec3::new(-4.0, 1.0, -5.0)),
    (3.0, Vec3::new(6.0, 1.5, -6.0)),
    (1.0, Vec3::new(-2.0, 0.5, 2.0)),
    (2.0, Vec3::new(5.0, 1.0, 3.0)),
];

// --- Replicated components ---

/// Marker for a player character. Replicated from server to all clients.
#[derive(Component, Serialize, Deserialize)]
pub struct Player;

/// Marker for a flame-orb projectile. Replicated from server to all clients.
#[derive(Component, Serialize, Deserialize)]
pub struct Spell;

/// Identifies the owning client of a `Player` entity via their replicon `NetworkId`
/// (= the client's authentication id). Replicated so clients can figure out which
/// player entity belongs to them.
#[derive(Component, Clone, Copy, Serialize, Deserialize)]
pub struct PlayerOwner(pub u64);

/// The latest client-input tick the server has applied for this player.
///
/// Replicated back to the owning client so reconciliation can match a server
/// snapshot against the client's predicted-state history at the same tick.
#[derive(Component, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LastProcessedInput(pub u32);

// --- Sim-only components ---

/// Server-side spell lifetime timer. Not replicated — the client just sees the
/// entity appear and disappear based on replicated spawns/despawns.
#[derive(Component)]
pub struct SpellLifetime {
    pub timer: Timer,
}

#[derive(TnuaScheme)]
#[scheme(basis = TnuaBuiltinWalk)]
pub enum PlayerActions {
    Jump(TnuaBuiltinJump),
}

/// Per-tick player input, captured each frame in `Update` and consumed in `FixedUpdate`.
///
/// Held inputs (`desired_motion`, `jump`) reflect the latest sample. Edge events
/// (`cast_queue`, `respawn`) are latched until the simulation drains them, so a click
/// that lands between two fixed steps is never lost.
///
/// Lives as a component on the player entity. The client maintains its own local
/// copy on its LocalPlayer entity for input capture; the server has one per connected
/// player and consumes them during simulation.
#[derive(Component, Default)]
pub struct PlayerInput {
    pub desired_motion: Vec3,
    pub jump: bool,
    pub cast_queue: Vec<PendingCast>,
    pub respawn: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct PendingCast {
    pub origin: Vec3,
    pub direction: Vec3,
}

// --- Network events ---

/// A single tick of player input, sent from client to server.
///
/// Non-incremental: the server simply overwrites its copy of the player's
/// `PlayerInput` fields with each message. The client sends one of these per
/// FixedUpdate tick.
///
/// `tick` is the client's monotonic FixedUpdate counter at the time the input
/// was captured. The server records the latest applied value in
/// `LastProcessedInput`, which is replicated back so the client can reconcile
/// its predicted state against the matching authoritative snapshot.
#[derive(Event, Serialize, Deserialize, Clone)]
pub struct InputMessage {
    pub tick: u32,
    pub desired_motion: Vec3,
    pub jump: bool,
    pub cast: Option<PendingCast>,
    pub respawn: bool,
}

// --- Plugins ---

/// Registers component replication rules and network events.
/// Added by both client and server so they share the wire protocol.
pub struct SharedReplicationPlugin;

/// Physics, character controller, and `player_controls`. Added by **both**
/// client and server. The client uses it to predict the local player; the
/// server uses it to authoritatively simulate every player.
///
/// Avian only operates on entities that actually have `RigidBody`/`Collider`,
/// so on the client it's effectively a no-op except for the local player —
/// remote players carry only `Transform` and don't participate in physics.
pub struct PlayerPhysicsPlugin;

/// Server-only systems: respawn handling, spell spawning, spell expiration.
/// These mutate state the client must NOT predict (the server is authoritative
/// for spells and respawns).
pub struct ServerSimPlugin;

// --- Impls ---

impl Plugin for SharedReplicationPlugin {
    fn build(&self, app: &mut App) {
        app.replicate::<Player>()
            .replicate::<Spell>()
            .replicate::<PlayerOwner>()
            .replicate::<LastProcessedInput>()
            .replicate::<Transform>()
            .add_client_event::<InputMessage>(Channel::Unreliable);
    }
}

impl Plugin for PlayerPhysicsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            PhysicsPlugins::default(),
            TnuaControllerPlugin::<PlayerActions>::new(FixedUpdate),
            TnuaAvian3dPlugin::new(FixedUpdate),
        ))
        .add_systems(
            FixedUpdate,
            player_controls.in_set(TnuaUserControlsSystems),
        );
    }
}

impl Plugin for ServerSimPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            (
                respawn_player.before(TnuaUserControlsSystems),
                (cast_spell, despawn_expired_spells)
                    .chain()
                    .after(TnuaUserControlsSystems),
            ),
        );
    }
}

/// Spawns a player entity with all simulation-side components + replication marker.
/// Called by the server when a client connects. Visuals are attached by a
/// client-side observer once the entity is replicated.
pub fn spawn_player_components(
    commands: &mut EntityCommands,
    configs: &mut Assets<PlayerActionsConfig>,
    position: Vec3,
    owner: u64,
) {
    commands.insert((
        Player,
        PlayerOwner(owner),
        LastProcessedInput::default(),
        Replicated,
        Transform::from_translation(position),
    ));
    insert_player_sim_bundle(commands, configs);
}

/// Inserts the physics + character-controller bundle a player needs to be
/// simulated locally. Used by both the server (for every player) and the client
/// (for the local player only, after `mark_local_player` identifies it).
pub fn insert_player_sim_bundle(
    commands: &mut EntityCommands,
    configs: &mut Assets<PlayerActionsConfig>,
) {
    commands.insert((
        PlayerInput::default(),
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
}

/// Spawns the static level colliders (ground + reference cubes). No meshes.
/// Used by the server; the client spawns its own visual version.
pub fn spawn_world_colliders(commands: &mut Commands) {
    commands.spawn((
        RigidBody::Static,
        Collider::half_space(Vec3::Y),
        Transform::default(),
    ));
    for (size, pos) in WORLD_CUBES {
        commands.spawn((
            RigidBody::Static,
            Collider::cuboid(*size, *size, *size),
            Transform::from_translation(*pos),
        ));
    }
}

fn respawn_player(
    mut players: Query<
        (
            &mut Transform,
            &mut LinearVelocity,
            &mut AngularVelocity,
            &mut PlayerInput,
        ),
        With<Player>,
    >,
) {
    for (mut transform, mut linear_vel, mut angular_vel, mut input) in &mut players {
        if !std::mem::take(&mut input.respawn) {
            continue;
        }
        transform.translation = PLAYER_SPAWN;
        linear_vel.0 = Vec3::ZERO;
        angular_vel.0 = Vec3::ZERO;
    }
}

fn player_controls(
    mut players: Query<(&mut TnuaController<PlayerActions>, &PlayerInput), With<Player>>,
) {
    for (mut controller, input) in &mut players {
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
}

fn cast_spell(mut commands: Commands, mut players: Query<&mut PlayerInput, With<Player>>) {
    for mut input in &mut players {
        for cast in input.cast_queue.drain(..) {
            commands.spawn((
                Spell,
                SpellLifetime {
                    timer: Timer::from_seconds(SPELL_LIFETIME, TimerMode::Once),
                },
                Replicated,
                Transform::from_translation(cast.origin),
                // Kinematic body: moved by its velocity each step, unaffected by forces
                // or collisions. Spells are straight-line sensors, not physics projectiles.
                RigidBody::Kinematic,
                Collider::sphere(SPELL_RADIUS),
                Sensor,
                CollidingEntities::default(),
                LinearVelocity(cast.direction * SPELL_SPEED),
            ));
        }
    }
}

fn despawn_expired_spells(
    mut commands: Commands,
    mut spells: Query<(Entity, &mut SpellLifetime, &CollidingEntities)>,
    players: Query<(), With<Player>>,
    time: Res<Time>,
) {
    for (entity, mut lifetime, colliding) in &mut spells {
        lifetime.timer.tick(time.delta());
        // Despawn if the spell touches anything that isn't a player. Spells pass
        // through players for MVP (no damage implemented yet).
        let hit_something = colliding.iter().any(|e| !players.contains(*e));
        if lifetime.timer.is_finished() || hit_something {
            commands.entity(entity).despawn();
        }
    }
}
