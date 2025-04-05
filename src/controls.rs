use std::ops::Range;

use avian3d::{math::*, prelude::*};
use bevy::{ecs::query::Has, input::mouse::AccumulatedMouseMotion, prelude::*};

pub struct CharacterControllerPlugin;

impl Plugin for CharacterControllerPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<MovementAction>()
            .add_systems(
                FixedUpdate,
                (
                    keyboard_input,
                    update_grounded,
                    movement,
                    apply_movement_damping,
                )
                    .chain(),
            )
            .add_systems(Update, orbit_camera);
    }
}

/// An event sent for a movement input action.
#[derive(Event)]
pub enum MovementAction {
    /// Movement in the x-y plane.
    Move(Vec2),
    Jump,
}

/// A marker component indicating that an entity is using a character controller.
#[derive(Component)]
pub struct Player;

/// A marker component indicating that an entity is on the ground.
#[derive(Component)]
#[component(storage = "SparseSet")]
pub struct Grounded;
/// The acceleration used for character movement.
#[derive(Component)]
pub struct MovementAcceleration(Scalar);

/// The damping factor used for slowing down movement.
#[derive(Component)]
pub struct MovementDamping(Scalar);

/// The strength of a jump.
#[derive(Component)]
pub struct JumpImpulse(Scalar);

/// A bundle that contains the components needed for a basic
/// kinematic character controller.
#[derive(Bundle)]
pub struct CharacterControllerBundle {
    character_controller: Player,
    rigid_body: RigidBody,
    collider: Collider,
    /// This component get's us the shape that is our ground
    ground_caster: ShapeCaster,
    locked_axes: LockedAxes,
    movement: MovementBundle,
}

/// A bundle that contains components for character movement.
#[derive(Bundle)]
pub struct MovementBundle {
    acceleration: MovementAcceleration,
    damping: MovementDamping,
    jump_impulse: JumpImpulse,
}

impl MovementBundle {
    pub const fn new(acceleration: Scalar, damping: Scalar, jump_impulse: Scalar) -> Self {
        Self {
            acceleration: MovementAcceleration(acceleration),
            damping: MovementDamping(damping),
            jump_impulse: JumpImpulse(jump_impulse),
        }
    }
}

impl Default for MovementBundle {
    fn default() -> Self {
        Self::new(30.0, 0.95, 7.0)
    }
}

impl CharacterControllerBundle {
    pub fn new(collider: Collider) -> Self {
        // Create shape caster as a slightly smaller version of collider
        let mut caster_shape = collider.clone();
        caster_shape.set_scale(Vec3::ONE * 0.99, 100);

        Self {
            character_controller: Player,
            rigid_body: RigidBody::Dynamic,
            collider,
            ground_caster: ShapeCaster::new(caster_shape, Vec3::ZERO, Quat::default(), Dir3::NEG_Y)
                .with_max_distance(0.2),
            locked_axes: LockedAxes::ROTATION_LOCKED,
            movement: MovementBundle::default(),
        }
    }

    pub fn with_movement(
        mut self,
        acceleration: Scalar,
        damping: Scalar,
        jump_impulse: Scalar,
    ) -> Self {
        self.movement = MovementBundle::new(acceleration, damping, jump_impulse);
        self
    }
}

/// Sends [`MovementAction`] events based on keyboard input.
fn keyboard_input(
    mut movement_event_writer: EventWriter<MovementAction>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
) {
    let up = keyboard_input.pressed(KeyCode::KeyW);
    let down = keyboard_input.pressed(KeyCode::KeyS);
    let left = keyboard_input.pressed(KeyCode::KeyA);
    let right = keyboard_input.pressed(KeyCode::KeyD);

    let horizontal = right as i8 - left as i8;
    let vertical = up as i8 - down as i8;
    let direction = Vector2::new(horizontal as Scalar, vertical as Scalar).clamp_length_max(1.0);

    if direction != Vector2::ZERO {
        movement_event_writer.send(MovementAction::Move(direction));
    }

    if keyboard_input.just_pressed(KeyCode::Space) {
        movement_event_writer.send(MovementAction::Jump);
    }
}

/// Updates the [`Grounded`] status for character controllers.
fn update_grounded(mut commands: Commands, mut query: Query<(Entity, &ShapeHits), With<Player>>) {
    for (entity, hits) in &mut query {
        // The character is grounded if the shape caster has a hit with a normal
        // that isn't too steep.
        let is_grounded = hits.iter().next().is_some();
        if is_grounded {
            commands.entity(entity).insert(Grounded);
        } else {
            commands.entity(entity).remove::<Grounded>();
        }
    }
}

/// Responds to [`MovementAction`] events and moves character controllers accordingly.
fn movement(
    time: Res<Time>,
    mut movement_event_reader: EventReader<MovementAction>,
    mut controllers: Query<
        (
            &Transform,
            &MovementAcceleration,
            &JumpImpulse,
            &mut LinearVelocity,
            Has<Grounded>,
        ),
        With<Player>,
    >,
) {
    // Precision is adjusted so that the example works with
    // both the `f32` and `f64` features. Otherwise you don't need this.
    let delta_time = time.delta_secs();

    for event in movement_event_reader.read() {
        for (
            player,
            MovementAcceleration(movement_acceleration),
            JumpImpulse(jump_impulse),
            mut linear_velocity,
            is_grounded,
        ) in &mut controllers
        {
            match event {
                MovementAction::Move(direction) => {
                    linear_velocity.x += direction.x * movement_acceleration * delta_time;
                    linear_velocity.z -= direction.y * movement_acceleration * delta_time;
                }
                MovementAction::Jump => {
                    if is_grounded {
                        linear_velocity.y = *jump_impulse;
                    }
                }
            }
        }
    }
}

/// Slows down movement in the XZ plane.
fn apply_movement_damping(mut query: Query<(&MovementDamping, &mut LinearVelocity)>) {
    for (MovementDamping(damping_factor), mut linear_velocity) in &mut query {
        // We could use `LinearDamping`, but we don't want to dampen movement along the Y axis
        linear_velocity.x *= *damping_factor;
        linear_velocity.z *= *damping_factor;
    }
}

/// This system keeps the camera a set distance from the player,
fn orbit_camera(
    mut player_query: Query<&mut Transform, With<Player>>,
    mut camera_query: Query<&mut Transform, (With<Camera3d>, Without<Player>)>, // we need to signal to bevy that there is no camera that is also a player
    mouse_movement: Res<AccumulatedMouseMotion>,
) {
    const CAMERA_DISTANCE: f32 = 10.0;
    const SENSITIVITY: f32 = 0.01;
    const PITCH_RANGE: Range<f32> = -(PI / 2.0 - 0.01)..(PI / 2.0 - 0.01);

    // Negate y axis because bevy is Y-up but mouse coordinates are Y-down
    let mut mouse_movement = mouse_movement.delta;
    mouse_movement.y = -mouse_movement.y;

    // Retrieve player and camera (asserts that exactly one of each exist)
    let mut player = player_query.single_mut();
    let mut camera = camera_query.single_mut();

    let delta_yaw = -mouse_movement.x * SENSITIVITY;
    let delta_pitch = mouse_movement.y * SENSITIVITY;

    // Obtain the existing pitch, yaw, and roll values from the transform.
    let (yaw, pitch, roll) = camera.rotation.to_euler(EulerRot::YXZ);

    // Establish the new yaw and pitch, preventing the pitch value from exceeding our limits.
    let pitch = (pitch + delta_pitch).clamp(PITCH_RANGE.start, PITCH_RANGE.end);
    let yaw = yaw + delta_yaw;

    // Apply the rotation
    camera.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, roll);

    // Follow the player
    camera.translation = player.translation - camera.forward() * CAMERA_DISTANCE;

    // TODO: Player should look in the same direction as the cam
    // let mut just_in_front_of_player = player.translation + camera.forward().as_vec3();
    // just_in_front_of_player.y = player.translation.y;
    // player.look_at(just_in_front_of_player, Vec3::Y);
}
