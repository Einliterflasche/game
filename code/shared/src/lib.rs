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
/// Camera height above the player's center. The eye sits a hair below the
/// top of the capsule so the camera doesn't poke out when standing under
/// low geometry.
pub const EYE_HEIGHT_OFFSET: f32 = PLAYER_CYLINDER_HEIGHT / 2.0 + PLAYER_RADIUS - 0.1;
pub const WAND_TIP_OFFSET: Vec3 = Vec3::new(0.25, -0.15, -0.7);

// Per-spell tuning lives in a module per spell. As spells diverge in shape and
// behavior they need very different parameters, so a single shared `SpellConfig`
// struct is the wrong abstraction — each module is the source of truth for its
// own spell and is consumed directly where the values are needed.

pub mod fire_bolt {
    pub const LIFETIME_SECS: f32 = 3.0;
    pub const SPEED: f32 = 12.0;
    pub const RADIUS: f32 = 0.1;
}

pub mod shield {
    use bevy::math::Vec3;

    pub const LIFETIME_SECS: f32 = 1.0;
    /// Distance from the caster's eye in the look direction.
    pub const FORWARD_OFFSET: f32 = 0.7;
    /// Vertical offset from the caster's eye in world space (negative = below
    /// the eye), to put the shield at chest height.
    pub const VERTICAL_OFFSET: f32 = -0.25;
    /// Full-extent size of the shield, applied as `Transform::scale` so both
    /// the visual sphere mesh (unit-diameter) and the unit-cuboid collider
    /// end up at these dimensions. Z is the thin axis (faces along the
    /// caster's look direction). Kept noticeably 3D rather than disc-flat:
    /// a fully flat sphere would collapse all normal-transforms toward the
    /// Z axis after the inverse-transpose pass, killing the fresnel rim
    /// glow that gives the shield its visible silhouette.
    pub const SIZE: Vec3 = Vec3::new(1.0, 0.8, 0.3);
}

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

/// Identifies which spell a given spell entity is. Replicated so the client
/// can pick the correct mesh / material when the entity arrives, and the same
/// component drives `cast_spell`'s spawn dispatch on the server. Acts as the
/// spell-entity marker (no separate `Spell` marker needed) — per-kind tuning
/// lives in the per-spell modules (`fire_bolt`, `shield`, …).
#[derive(Component, Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum SpellKind {
    FireBolt,
    Shield,
}

/// Identifies the owning client of a `Player` entity via their replicon `NetworkId`
/// (= the client's authentication id). Replicated so clients can figure out which
/// player entity belongs to them.
#[derive(Component, Clone, Copy, Serialize, Deserialize)]
pub struct PlayerOwner(pub u64);

/// The latest client-input tick the server has applied for this player.
///
/// Replicated back to the owning client so reconciliation can match a server
/// snapshot against the client's predicted-state history at the same tick.
#[derive(Component, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastProcessedInput(pub u32);

/// "This player has finished tracing a spell pattern and is holding the cast
/// ready to fire". Carries the matched `SpellKind` so other clients can render
/// a preview of the *correct* spell on the aiming player. The owning client
/// also has its own camera-attached preview for the first-person view.
///
/// Replicated to all clients. Inserted/removed only on transitions, never
/// mutated in place — `apply_input` swaps the component when the kind
/// changes (rare in practice: a player can only be aiming one spell at a time).
#[derive(Component, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Aiming(pub SpellKind);

/// World-space camera-forward vector for a player. Replicated so other
/// clients can position the aim-phase preview orb in front of them in the
/// direction they're looking. The player's `Transform.rotation` is driven by
/// Tnua based on movement direction (not look direction), so it can't be
/// reused for this — the camera pitch/yaw is per-client and only the owning
/// client knows it. Updated by the server from each `InputMessage`.
#[derive(Component, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LookDirection(pub Vec3);

impl Default for LookDirection {
    fn default() -> Self {
        Self(Vec3::NEG_Z)
    }
}

// --- Sim-only components ---

/// Server-side spell lifetime timer. Not replicated — the client just sees the
/// entity appear and disappear based on replicated spawns/despawns.
#[derive(Component)]
pub struct SpellLifetime {
    pub timer: Timer,
}

/// Marks a spell that travels in a straight line and dies on contact with
/// world geometry. Used to gate `despawn_expired_spells`'s contact-death rule
/// — non-projectile spells (e.g. the future shield) keep their colliders for
/// spell-vs-spell interaction but should not despawn just because they touch
/// the floor or their caster.
#[derive(Component)]
pub struct Projectile;

/// Back-reference from a spell entity to the player who cast it, identified
/// by the caster's network ID (the same `u64` carried in `PlayerOwner`).
///
/// Replicated so the client can pick out "this is my shield" and run local
/// prediction on it instead of waiting for the server's authoritative
/// transform every tick. Server-side systems do `Entity` lookups by iterating
/// players and matching `PlayerOwner.0` — fine at this scale.
#[derive(Component, Clone, Copy, Serialize, Deserialize)]
pub struct SpellOwner(pub u64);

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

/// One queued cast captured on the client and applied on the server.
///
/// `direction` is meaningful for projectile spells (e.g. fire bolt). Spells
/// that ignore direction (e.g. shield, heal) still set it — usually to the
/// camera-forward — so the message stays the same shape regardless of kind.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct PendingCast {
    pub kind: SpellKind,
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
    /// `Some(kind)` while the client is in the aim phase (pattern traced,
    /// holding LMB before release). The server reconciles the player's
    /// `Aiming` component against this so other clients can render a preview
    /// of the *correct* spell. `None` clears the aim state.
    pub aiming: Option<SpellKind>,
    /// World-space camera-forward of the sending client. Server stores it
    /// on `LookDirection` so other clients can position the preview orb in
    /// front of the aiming player.
    pub look_forward: Vec3,
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
            .replicate::<SpellKind>()
            .replicate::<SpellOwner>()
            .replicate::<PlayerOwner>()
            .replicate::<LastProcessedInput>()
            .replicate::<Aiming>()
            .replicate::<LookDirection>()
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
                (cast_spell, follow_shield, tick_spell_lifecycles)
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
        LookDirection::default(),
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

fn cast_spell(
    mut commands: Commands,
    mut players: Query<(&PlayerOwner, &mut PlayerInput), With<Player>>,
) {
    for (owner, mut input) in &mut players {
        for cast in input.cast_queue.drain(..) {
            spawn_spell(&mut commands, *owner, cast);
        }
    }
}

/// Server-side spawn dispatch. Each `SpellKind` arm builds the entity for that
/// spell from the matching per-spell module's constants. New spells slot in as
/// new arms.
fn spawn_spell(commands: &mut Commands, caster: PlayerOwner, cast: PendingCast) {
    match cast.kind {
        SpellKind::FireBolt => {
            commands.spawn((
                cast.kind,
                SpellOwner(caster.0),
                Projectile,
                SpellLifetime {
                    timer: Timer::from_seconds(fire_bolt::LIFETIME_SECS, TimerMode::Once),
                },
                Replicated,
                Transform::from_translation(cast.origin),
                // Kinematic body: moved by its velocity each step, unaffected by forces
                // or collisions. Bolt spells are straight-line sensors, not physics projectiles.
                RigidBody::Kinematic,
                Collider::sphere(fire_bolt::RADIUS),
                Sensor,
                CollidingEntities::default(),
                LinearVelocity(cast.direction * fire_bolt::SPEED),
            ));
        }
        SpellKind::Shield => {
            // Initial transform is a placeholder — `follow_shield` overwrites
            // it before the first physics step. The non-uniform `scale` is
            // what gives the unit-cuboid collider AND the unit-sphere visual
            // (on the client) their shield dimensions: Avian propagates
            // `Transform.scale` to the collider via its `update_collider_scale`
            // system, and Bevy's renderer uses the same scale on the mesh.
            commands.spawn((
                cast.kind,
                SpellOwner(caster.0),
                SpellLifetime {
                    timer: Timer::from_seconds(shield::LIFETIME_SECS, TimerMode::Once),
                },
                Replicated,
                Transform::from_translation(cast.origin).with_scale(shield::SIZE),
                RigidBody::Kinematic,
                Collider::cuboid(1.0, 1.0, 1.0),
                Sensor,
                CollidingEntities::default(),
            ));
        }
    }
}

/// Builds the shield transform (position + rotation + scale) from a caster's
/// world-space body translation and look direction. Single source of truth so
/// the server's `follow_shield` and the client's local-prediction system stay
/// pixel-identical, otherwise the local shield would visibly snap each
/// replication tick when the two formulas drift.
pub fn compute_shield_transform(player_translation: Vec3, look: Vec3) -> Transform {
    let eye_pos = player_translation + Vec3::Y * EYE_HEIGHT_OFFSET;
    let eye_xform = Transform::from_translation(eye_pos).looking_to(look, Vec3::Y);
    // Local offset: shield slightly below the eye line, in front of it.
    // -Z is "forward" in Bevy camera convention so the negative Z reaches outward.
    let local_offset = Vec3::new(0.0, shield::VERTICAL_OFFSET, -shield::FORWARD_OFFSET);
    Transform {
        translation: eye_xform.transform_point(local_offset),
        rotation: eye_xform.rotation,
        scale: shield::SIZE,
    }
}

/// Reposes every active shield to ride its caster's "in front of the eyes"
/// vector each tick. Shields are top-level entities (not children of the
/// player) because the player's `Transform.rotation` is driven by Tnua to
/// face the movement direction, which is unrelated to the camera-forward we
/// want the shield to face — Tnua-rotated parents would point the shield
/// sideways the moment the player strafed.
///
/// Caster lookup walks every player matching `SpellOwner.0 == PlayerOwner.0`.
/// O(P × S) but P and S are tiny — not worth a HashMap.
fn follow_shield(
    casters: Query<(&PlayerOwner, &Transform, &LookDirection), (With<Player>, Without<SpellKind>)>,
    mut shields: Query<(&SpellKind, &SpellOwner, &mut Transform), Without<Player>>,
) {
    for (kind, owner, mut shield_xform) in &mut shields {
        if !matches!(kind, SpellKind::Shield) {
            continue;
        }
        let Some((_, caster_xform, look)) =
            casters.iter().find(|(po, _, _)| po.0 == owner.0)
        else {
            continue;
        };
        *shield_xform = compute_shield_transform(caster_xform.translation, look.0);
    }
}

/// Per-tick spell entity housekeeping. Despawns each spell for one of three
/// reasons, in priority order, with `continue` after the first hit so a single
/// entity is never queued for despawn twice:
///
/// 1. **Lifetime expired** — the universal timer ran out.
/// 2. **Blocked by another spell** — `despawns_on_spell_contact` says this
///    spell dies when touching one of the kinds it's currently colliding
///    with. Asymmetric, so `(FireBolt, Shield)` despawns the bolt while
///    leaving the shield alive on the same contact.
/// 3. **Projectile hit world** — projectile spells die on contact with
///    anything that isn't a player and isn't another spell. Players pass
///    through (no damage system yet); spells are governed by rule 2.
///
/// Combined into one system rather than split (timer / spell-vs-spell /
/// world) so the `continue` chain serializes the despawn decisions and avoids
/// the cross-system race where two systems each queue a despawn for the same
/// entity in the same tick.
fn tick_spell_lifecycles(
    mut commands: Commands,
    mut spells: Query<(
        Entity,
        &SpellKind,
        &mut SpellLifetime,
        &CollidingEntities,
        Has<Projectile>,
    )>,
    spell_kinds: Query<&SpellKind>,
    players: Query<(), With<Player>>,
    time: Res<Time>,
) {
    for (entity, kind, mut lifetime, colliding, is_projectile) in &mut spells {
        lifetime.timer.tick(time.delta());

        if lifetime.timer.is_finished() {
            commands.entity(entity).despawn();
            continue;
        }

        let blocked_by_spell = colliding.iter().any(|other| {
            spell_kinds
                .get(*other)
                .map(|other_kind| despawns_on_spell_contact(*kind, *other_kind))
                .unwrap_or(false)
        });
        if blocked_by_spell {
            commands.entity(entity).despawn();
            continue;
        }

        if is_projectile {
            let hit_world = colliding
                .iter()
                .any(|e| !players.contains(*e) && !spell_kinds.contains(*e));
            if hit_world {
                commands.entity(entity).despawn();
            }
        }
    }
}

/// Spell-vs-spell interaction table. Returns `true` when a spell of
/// `self_kind` should despawn on contact with a spell of `other_kind`. The
/// relation is intentionally asymmetric: the bolt dying against the shield
/// leaves the shield alive, and a future shield-breaker would despawn the
/// shield without itself dying.
///
/// New interactions are added as new arms in the `matches!`. The default
/// (anything not listed) is "pass through, both unaffected".
fn despawns_on_spell_contact(self_kind: SpellKind, other_kind: SpellKind) -> bool {
    matches!((self_kind, other_kind), (SpellKind::FireBolt, SpellKind::Shield))
}
