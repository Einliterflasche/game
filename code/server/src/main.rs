use std::{
    net::{Ipv4Addr, UdpSocket},
    time::{Duration, SystemTime},
};

use bevy::{
    app::ScheduleRunnerPlugin, asset::AssetPlugin, log::LogPlugin, prelude::*,
    state::app::StatesPlugin, time::Fixed,
};

/// Server simulation + replication tick rate. Higher = smoother movement on the
/// client (less stepping between snapshots) and lower input lag, at the cost of
/// more CPU and bandwidth. 64 Hz is the Bevy default and matches Overwatch.
const SERVER_HZ: f64 = 64.0;
use bevy_replicon::{
    prelude::*, shared::backend::connected_client::NetworkId,
};
use bevy_replicon_renet::{
    RenetChannelsExt, RenetServer, RepliconRenetPlugins,
    netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig},
    renet::ConnectionConfig,
};
use shared::{
    Aiming, DEFAULT_PORT, InputMessage, LastProcessedInput, LookDirection, PLAYER_SPAWN,
    PROTOCOL_ID, PlayerActionsConfig, PlayerInput, PlayerPhysicsPlugin, ServerSimPlugin,
    SharedReplicationPlugin, spawn_player_components, spawn_world_colliders,
};

fn main() {
    App::new()
        .add_plugins((
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
                1.0 / SERVER_HZ,
            ))),
            LogPlugin::default(),
            TransformPlugin,
            AssetPlugin::default(),
            StatesPlugin,
        ))
        .add_plugins((
            RepliconPlugins,
            RepliconRenetPlugins,
            SharedReplicationPlugin,
            PlayerPhysicsPlugin,
            ServerSimPlugin,
        ))
        // Tick FixedUpdate (physics + sim systems) at the same rate as the main
        // loop, so replication snapshots are produced once per loop iteration.
        .insert_resource(Time::<Fixed>::from_hz(SERVER_HZ))
        .add_systems(Startup, (setup_network, setup_world))
        .add_observer(spawn_player_on_connect)
        .add_observer(apply_input)
        .run();
}

fn setup_network(mut commands: Commands, channels: Res<RepliconChannels>) {
    let server = RenetServer::new(ConnectionConfig {
        server_channels_config: channels.server_configs(),
        client_channels_config: channels.client_configs(),
        ..Default::default()
    });

    let current_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock before UNIX epoch");
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DEFAULT_PORT))
        .expect("failed to bind server UDP socket");
    let server_config = ServerConfig {
        current_time,
        max_clients: 8,
        protocol_id: PROTOCOL_ID,
        authentication: ServerAuthentication::Unsecure,
        public_addresses: Default::default(),
    };
    let transport = NetcodeServerTransport::new(server_config, socket)
        .expect("failed to build netcode server transport");

    commands.insert_resource(server);
    commands.insert_resource(transport);

    info!("server listening on UDP :{DEFAULT_PORT}");
}

fn setup_world(mut commands: Commands) {
    spawn_world_colliders(&mut commands);
    info!("server world initialized");
}

/// When a client connects, replicon creates an entity with `ConnectedClient` + `NetworkId`.
/// We reuse that entity as the player character by inserting the full player bundle on it.
fn spawn_player_on_connect(
    add: On<Add, NetworkId>,
    mut commands: Commands,
    network_ids: Query<&NetworkId>,
    mut configs: ResMut<Assets<PlayerActionsConfig>>,
) {
    let Ok(network_id) = network_ids.get(add.entity) else {
        return;
    };
    info!("client connected: {network_id:?} -> {}", add.entity);
    let mut entity = commands.entity(add.entity);
    spawn_player_components(&mut entity, &mut configs, PLAYER_SPAWN, network_id.get());
}

/// Applies an incoming `InputMessage` to the sender's `PlayerInput` component
/// and records the message tick on `LastProcessedInput` so it replicates back to
/// the client for reconciliation.
///
/// The sender's player character is the same entity as the sender's connection, so
/// `FromClient::client_id` gives us the entity directly.
///
/// Also reconciles the `Aiming(SpellKind)` component against the message's
/// `aiming` field. Only writes on transitions (insert / swap / remove) so
/// replicon doesn't re-broadcast every tick of an unchanged aim.
fn apply_input(
    from: On<FromClient<InputMessage>>,
    mut players: Query<(
        &mut PlayerInput,
        &mut LastProcessedInput,
        &mut LookDirection,
        Option<&Aiming>,
    )>,
    mut commands: Commands,
) {
    let Some(entity) = from.client_id.entity() else {
        return;
    };
    let Ok((mut input, mut last_tick, mut look, current_aiming)) = players.get_mut(entity) else {
        return;
    };
    input.desired_motion = from.desired_motion;
    input.jump = from.jump;
    if let Some(cast) = from.cast {
        input.cast_queue.push(cast);
    }
    if from.respawn {
        input.respawn = true;
    }
    // `set_if_neq` keeps replicon from re-replicating these every tick when
    // the value hasn't moved (most notably `LookDirection` for an idle player).
    last_tick.set_if_neq(LastProcessedInput(from.tick));
    look.set_if_neq(LookDirection(from.look_forward));

    // Promote/demote/swap the Aiming component when the desired kind diverges
    // from the current one. The same arm handles "started aiming a different
    // spell" by overwriting — `insert` replaces the existing component.
    let desired = from.aiming.map(Aiming);
    let current = current_aiming.copied();
    if desired != current {
        match desired {
            Some(aiming) => {
                commands.entity(entity).insert(aiming);
            }
            None => {
                commands.entity(entity).remove::<Aiming>();
            }
        }
    }
}
