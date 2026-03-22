use alloc::vec::Vec;
use bevy_app::prelude::*;
use bevy_ecs::{
    entity::{EntityHashMap, hash_set::EntityHashSet},
    prelude::*,
};
use bevy_state::prelude::*;

use bevy_replicon::prelude::*;
use bevy_replicon::shared::backend::connected_client::NetworkId;
use lightyear_connection::client::Connected;
use lightyear_connection::client_of::ClientOf;
use lightyear_connection::server::{Started, Stopped};
use lightyear_core::id::RemoteId;
use lightyear_messages::MessageManager;
use lightyear_transport::channel::receivers::ChannelReceive;
use lightyear_transport::plugin::TransportSystems;
use lightyear_transport::prelude::Transport;

use crate::channels::RepliconChannelMap;
use lightyear_messages::plugin::MessageSystems;
use tracing::trace;

/// Adds the replicon server-side backend bridge for lightyear.
///
/// Handles:
/// - `ServerState` transitions (Running when server starts or client connects)
/// - `ConnectedClient` insertion for replicon visibility
/// - Sending `ServerMessages` (replication) and receiving `ClientMessages` (acks) via transport
/// - Syncing replicon's entity map to lightyear's `MessageManager` entity mapper
pub struct RepliconServerPlugin;

impl Plugin for RepliconServerPlugin {
    fn build(&self, app: &mut App) {
        // When Connected is added to a link entity, add replicon's ConnectedClient + NetworkId
        app.add_observer(on_client_connected);

        // State management
        app.add_systems(
            PreUpdate,
            sync_server_state.before(ServerSystems::ReceivePackets),
        );

        // Packet bridge: replicon <-> lightyear transport
        app.add_systems(
            PreUpdate,
            receive_server_packets.in_set(ServerSystems::ReceivePackets),
        );
        app.add_systems(
            PostUpdate,
            send_server_packets.in_set(ServerSystems::SendPackets),
        );

        // Entity map bridge: replicon's ServerEntityMap -> lightyear's MessageManager entity_mapper
        app.add_systems(
            PreUpdate,
            sync_entity_map
                .after(ClientSystems::Receive)
                .after(ServerSystems::Receive),
        );

        app.configure_sets(
            PreUpdate,
            ServerSystems::ReceivePackets
                .after(TransportSystems::Receive)
                // Replicon bridge must read its channels before lightyear's MessagePlugin::recv
                // drains ALL transport receivers (including replicon channels)
                .before(MessageSystems::Receive),
        );
        app.configure_sets(
            PostUpdate,
            ServerSystems::SendPackets.before(TransportSystems::Send),
        );
    }
}

/// When `Connected` is added to a link entity, insert replicon's
/// `ConnectedClient` and `NetworkId` so replicon's visibility system sees it.
///
/// This fires on both CLIENT and SERVER apps:
/// - SERVER: when a remote client connects (client_of entity)
/// - CLIENT: when the client connects to the server (client entity)
fn on_client_connected(
    _trigger: On<Add, Connected>,
    query: Query<(Entity, &RemoteId), Added<Connected>>,
    mut commands: Commands,
) {
    for (entity, remote_id) in query.iter() {
        commands.entity(entity).insert((
            ConnectedClient {
                max_size: lightyear_transport::packet::packet_builder::MAX_PACKET_SIZE,
            },
            NetworkId::new(remote_id.to_bits()),
        ));
    }
}

/// Sync replicon's `ServerState` with lightyear lifecycle.
///
/// Sets `Running` when `Started` is present (server app).
///
/// For CLIENT → SERVER replication (`Replicate::to_server()`), ServerState is set to Running
/// from the Replicate on_insert hook instead, so the CLIENT app's replicon server only runs
/// when there are entities to replicate. This prevents the CLIENT from sending empty mutations
/// (from `track_mutate_messages`) that would confuse the SERVER's replicon client in multi-client setups.
fn sync_server_state(
    started: Query<(), With<Started>>,
    stopped: Query<(), With<Stopped>>,
    state: Res<State<ServerState>>,
    mut next_state: ResMut<NextState<ServerState>>,
) {
    if !started.is_empty() && *state.get() != ServerState::Running {
        next_state.set(ServerState::Running);
    }
    if started.is_empty() && !stopped.is_empty() && *state.get() != ServerState::Stopped {
        next_state.set(ServerState::Stopped);
    }
}

/// Receive packets from transports and populate `ServerMessages` (ack data from peers).
///
/// Reads from client_channels (MutationAcks) on each transport and puts into `ServerMessages`.
fn receive_server_packets(
    channel_map: Res<RepliconChannelMap>,
    mut server_messages: ResMut<ServerMessages>,
    mut transports: Query<(Entity, &mut Transport), With<ClientOf>>,
) {
    for (entity, mut transport) in transports.iter_mut() {
        for (idx, &(_, channel_id)) in channel_map.client_channels.iter().enumerate() {
            if let Some(receiver) = transport.receivers.get_mut(&channel_id) {
                while let Some((_, message, _)) = receiver.receiver.read_message() {
                    server_messages.insert_received(entity, idx, message);
                }
            }
        }
    }
}

/// Send `ServerMessages` (replication data) via transport to peers.
///
/// Drains `ServerMessages` and sends on server_channels (Updates, Mutations).
fn send_server_packets(
    channel_map: Res<RepliconChannelMap>,
    mut server_messages: ResMut<ServerMessages>,
    mut transports: Query<&mut Transport>,
) {
    for (client, channel_idx, message) in server_messages.drain_sent() {
        let (channel_kind, _) = channel_map.server_channels[channel_idx];
        trace!(
            "send_server_packets: sending {} bytes on channel_idx={} to {:?}",
            message.len(),
            channel_idx,
            client
        );
        if let Ok(mut transport) = transports.get_mut(client) {
            transport.send_mut_erased(channel_kind, message, 1.0).ok();
        } else {
            trace!("send_server_packets: no transport for client {:?}", client);
        }
    }
}

/// Sync receive-side remote entity ids into lightyear's `MessageManager.entity_mapper`.
///
/// Dedicated servers no longer have a singleton `ServerEntityMap` when receiving
/// client-authored replication. Instead, each received remote entity carries its
/// source link (`ReplicatedFrom`) and original remote entity id (`RemoteEntity`).
/// We rebuild the per-link lightyear mapping from those components.
fn sync_entity_map(
    remotes: Query<(Entity, &ReplicatedFrom, &RemoteEntity), With<Remote>>,
    mut managers: Query<&mut MessageManager, With<ClientOf>>,
    mut synced_entities: Local<EntityHashMap<EntityHashSet>>,
) {
    let mut current = EntityHashMap::<EntityHashSet>::default();
    let mut mappings = EntityHashMap::<Vec<(Entity, Entity)>>::default();

    for (local_entity, replicated_from, remote_entity) in &remotes {
        current
            .entry(replicated_from.0)
            .or_default()
            .insert(remote_entity.0);
        mappings
            .entry(replicated_from.0)
            .or_default()
            .push((remote_entity.0, local_entity));
    }

    for (source, entries) in &mappings {
        let Ok(mut manager) = managers.get_mut(*source) else {
            continue;
        };

        for (remote_entity, local_entity) in entries {
            manager.entity_mapper.insert(*remote_entity, *local_entity);
        }

        if let Some(previous) = synced_entities.get(source)
            && let Some(current_entities) = current.get(source)
        {
            for remote_entity in previous.iter() {
                if !current_entities.contains(remote_entity) {
                    manager.entity_mapper.remove_by_remote(*remote_entity);
                }
            }
        }
    }

    for (source, previous) in synced_entities.iter() {
        if current.contains_key(source) {
            continue;
        }

        let Ok(mut manager) = managers.get_mut(*source) else {
            continue;
        };

        for remote_entity in previous.iter() {
            manager.entity_mapper.remove_by_remote(*remote_entity);
        }
    }

    *synced_entities = current;
}
