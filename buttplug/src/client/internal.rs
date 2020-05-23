// Buttplug Rust Source Code File - See https://buttplug.io for more info.
//
// Copyright 2016-2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

//! Implementation of internal Buttplug Client event loop.

use super::{
  connectors::{ButtplugClientConnector, ButtplugClientConnectorStateShared},
  device::ButtplugClientDevice,
  ButtplugClientEvent, ButtplugClientMessageFuturePair, ButtplugClientResult,
};
use crate::{
  core::messages::{ButtplugClientOutMessage, DeviceList, DeviceMessageInfo},
  util::future::ButtplugFutureStateShared,
};
use async_std::{
  prelude::{FutureExt, StreamExt},
  sync::{channel, Receiver, Sender},
};
use std::collections::HashMap;

/// Enum used for communication from the client to the event loop.
pub enum ButtplugClientMessage {
  /// Client request to disconnect, via already sent connector instance.
  Disconnect(ButtplugClientConnectorStateShared),
  /// Given a DeviceList message, update the inner loop values and create
  /// events for additions.
  HandleDeviceList(DeviceList),
  /// Return new ButtplugClientDevice instances for all known and currently
  /// connected devices.
  RequestDeviceList(ButtplugFutureStateShared<Vec<ButtplugClientDevice>>),
  /// Client request to send a message via the connector.
  ///
  /// Bundled future should have reply set and waker called when this is
  /// finished.
  Message(ButtplugClientMessageFuturePair),
}

/// Enum for messages going to a [ButtplugClientDevice] instance.
pub enum ButtplugClientDeviceEvent {
  /// Device has disconnected from server.
  DeviceDisconnect,
  /// Client has disconnected from server.
  ClientDisconnect,
  /// Message was received from server for that specific device.
  Message(ButtplugClientOutMessage),
}

/// Set of possible responses from the different inputs to the client inner
/// loop.
enum StreamReturn {
  /// Response from the [ButtplugServer].
  ConnectorMessage(ButtplugClientOutMessage),
  /// Incoming message from the [ButtplugClient].
  ClientMessage(ButtplugClientMessage),
  /// Incoming message from a [ButtplugClientDevice].
  DeviceMessage(ButtplugClientMessageFuturePair),
  /// Disconnection from the [ButtplugServer].
  Disconnect,
}

/// Event loop for running [ButtplugClient] connections.
///
/// Acts as a hub for communication between the connector and [ButtplugClient]
/// instances.
///
/// # Why an event loop?
///
/// Due to the async nature of Buttplug, we many channels routed to many
/// different tasks. However, all of those tasks will refer to the same event
/// loop. This allows us to coordinate and centralize our information while
/// keeping the API async.
struct ButtplugClientEventLoop {
  /// List of currently connected devices.
  devices: HashMap<u32, DeviceMessageInfo>,
  /// Sender to pass to new [ButtplugClientDevice] instances.
  device_message_sender: Sender<ButtplugClientMessageFuturePair>,
  /// Receiver for incoming [ButtplugClientDevice] messages.
  device_message_receiver: Receiver<ButtplugClientMessageFuturePair>,
  // TODO this should be a broadcaster
  /// Event sender for specific devices.
  ///
  /// We can have many instances of the same [ButtplugClientDevice]. This map
  /// allows us to send messages to all device instances that refer to the same
  /// device index on the server.
  device_event_senders: HashMap<u32, Vec<Sender<ButtplugClientDeviceEvent>>>,
  /// Sends events to the [ButtplugClient] instance.
  event_sender: Sender<ButtplugClientEvent>,
  /// Receives incoming messages from client instances.
  client_receiver: Receiver<ButtplugClientMessage>,
  /// Connector the event loop will use to communicate with the [ButtplugServer]
  connector: Box<dyn ButtplugClientConnector>,
  /// Receiver for messages send from the [ButtplugServer] via the connector.
  connector_receiver: Receiver<ButtplugClientOutMessage>,
}

impl ButtplugClientEventLoop {
  /// Creates a new [ButtplugClientEventLoop].
  ///
  /// Given the [ButtplugClientConnector] object, as well as the channels used
  /// for communicating with the client, creates an event loop structure and
  /// returns it.
  pub fn new(
    mut connector: impl ButtplugClientConnector + 'static,
    event_sender: Sender<ButtplugClientEvent>,
    client_receiver: Receiver<ButtplugClientMessage>,
  ) -> Self {
    let (device_message_sender, device_message_receiver) =
      channel::<ButtplugClientMessageFuturePair>(256);
    Self {
      devices: HashMap::new(),
      device_event_senders: HashMap::new(),
      device_message_sender,
      device_message_receiver,
      event_sender,
      client_receiver,
      connector_receiver: connector.get_event_receiver(),
      connector: Box::new(connector),
    }
  }

  /// Creates a [ButtplugClientDevice] from [DeviceMessageInfo].
  ///
  /// Given a [DeviceMessageInfo] from a [DeviceAdded] or [DeviceList] message,
  /// creates a ButtplugClientDevice and adds it the internal device map, then
  /// returns the instance.
  fn create_client_device(&mut self, info: &DeviceMessageInfo) -> ButtplugClientDevice {
    let (event_sender, event_receiver) = channel(256);
    // If we don't have an entry in the map for the channel, add it. Otherwise,
    // push it on the vector.
    //
    // TODO USE A GOD DAMN BROADCASTER THIS IS SILLY
    self
      .device_event_senders
      .entry(info.device_index)
      .or_insert_with(|| vec![])
      .push(event_sender);
    ButtplugClientDevice::from((info, self.device_message_sender.clone(), event_receiver))
  }

  /// Parse device messages from the connector.
  ///
  /// Since the event loop maintains the state of all devices reported from the
  /// server, it will catch [DeviceAdded]/[DeviceList]/[DeviceRemoved] messages
  /// and update its map accordingly. After that, it will pass the information
  /// on as a [ButtplugClientEvent] to the [ButtplugClient].
  async fn parse_connector_message(&mut self, msg: ButtplugClientOutMessage) {
    info!("Sending message to clients.");
    match &msg {
      ButtplugClientOutMessage::DeviceAdded(dev) => {
        let info = DeviceMessageInfo::from(dev);
        let device = self.create_client_device(&info);
        self.devices.insert(dev.device_index, info);
        self
          .event_sender
          .send(ButtplugClientEvent::DeviceAdded(device))
          .await;
      }
      ButtplugClientOutMessage::DeviceList(dev) => {
        for d in &dev.devices {
          let device = self.create_client_device(&d);
          self.devices.insert(d.device_index, d.clone());
          self
            .event_sender
            .send(ButtplugClientEvent::DeviceAdded(device))
            .await;
        }
      }
      ButtplugClientOutMessage::DeviceRemoved(dev) => {
        let info = self.devices.remove(&dev.device_index);
        self.device_event_senders.remove(&dev.device_index);
        self
          .event_sender
          .send(ButtplugClientEvent::DeviceRemoved(info.unwrap()))
          .await;
      }
      _ => panic!("Got connector message type we don't know how to handle!"),
    }
  }

  /// Send a message from the [ButtplugClient] to the [ButtplugClientConnector].
  async fn send_message(&mut self, msg_fut: ButtplugClientMessageFuturePair) {
    let reply = self.connector.send(msg_fut.msg).await;
    msg_fut.waker.set_reply(reply);
  }

  /// Parses message types from the client, returning false when disconnect
  /// happens.
  ///
  /// Takes different messages from the client and handles them:
  ///
  /// - For outbound messages to the server, sends them to the connector/server.
  /// - For disconnections, requests connector disconnect
  /// - For RequestDeviceList, builds a reply out of its own 
  async fn parse_client_message(&mut self, msg: ButtplugClientMessage) -> bool {
    trace!("Parsing a client message.");
    match msg {
      ButtplugClientMessage::Message(msg_fut) => {
        debug!("Sending message through connector.");
        self.send_message(msg_fut).await;
        true
      }
      ButtplugClientMessage::Disconnect(state) => {
        debug!("Client requested disconnect");
        state.set_reply(self.connector.disconnect().await);
        false
      }
      ButtplugClientMessage::RequestDeviceList(fut) => {
        debug!("Building device list!");
        let mut device_return = vec![];
        // TODO There has to be a way to do this without the clone()
        for device in self.devices.clone().values() {
          let client_device = self.create_client_device(device);
          device_return.push(client_device);
        }
        debug!("Returning device list of {} items!", device_return.len());
        fut.set_reply(device_return);
        true
      }
      ButtplugClientMessage::HandleDeviceList(device_list) => {
        debug!("Handling device list!");
        for d in &device_list.devices {
          let device = self.create_client_device(&d);
          self.devices.insert(d.device_index, d.clone());
          self
            .event_sender
            .send(ButtplugClientEvent::DeviceAdded(device))
            .await;
        }
        true
      }
    }
  }

  /// Runs the event loop, returning once either the client or connector drops.
  pub async fn run(&mut self) {
    // Once connected, wait for messages from either the client, the generated
    // client devices, or the connector, and send them the direction they're
    // supposed to go.
    let mut client_receiver = self.client_receiver.clone();
    let mut connector_receiver = self.connector_receiver.clone();
    let mut device_receiver = self.device_message_receiver.clone();
    loop {
      let client_future = async {
        match client_receiver.next().await {
          None => {
            debug!("Client disconnected.");
            StreamReturn::Disconnect
          }
          Some(msg) => StreamReturn::ClientMessage(msg),
        }
      };
      let event_future = async {
        match connector_receiver.next().await {
          None => {
            debug!("Connector disconnected.");
            StreamReturn::Disconnect
          }
          Some(msg) => StreamReturn::ConnectorMessage(msg),
        }
      };
      let device_future = async {
        match device_receiver.next().await {
          None => {
            // Since we hold a reference to the sender so we can
            // redistribute it when creating devices, we'll never
            // actually do this.
            panic!("We should never get here.");
          }
          Some(msg) => StreamReturn::DeviceMessage(msg),
        }
      };

      let stream_fut = event_future.race(client_future).race(device_future);
      match stream_fut.await {
        StreamReturn::ConnectorMessage(msg) => self.parse_connector_message(msg).await,
        StreamReturn::ClientMessage(msg) => {
          if !self.parse_client_message(msg).await {
            break;
          }
        }
        StreamReturn::DeviceMessage(msg_fut) => {
          // TODO Check whether we actually are still connected to
          // this device.
          self.send_message(msg_fut).await;
        }
        StreamReturn::Disconnect => {
          info!("Disconnected!");
          break;
        }
      }
    }
  }
}

/// The internal event loop for [super::ButtplugClient] connection and
/// communication
///
/// Created whenever a new [super::ButtplugClient] is created, the internal loop
/// handles connection and communication with the server through the connector,
/// and creation of events received from the server.
///
/// The event_loop does a few different things during its lifetime.
///
/// - The first thing it will do is wait for a Connect message from a client.
///   This message contains a [ButtplugClientConnector] that will be used to
///   connect and communicate with a [crate::server::ButtplugServer].
///
/// - After a connection is established, it will listen for events from the
///   connector, or messages from the client, until either server/client
///   disconnects.
///
/// - Finally, on disconnect, it will tear down, and cannot be used again. All
///   clients and devices associated with the loop will be invalidated, and a
///   new [super::ButtplugClient] must be created.
pub async fn client_event_loop(
  connector: impl ButtplugClientConnector + 'static,
  event_sender: Sender<ButtplugClientEvent>,
  client_receiver: Receiver<ButtplugClientMessage>,
) -> ButtplugClientResult {
  info!("Starting client event loop.");
  ButtplugClientEventLoop::new(connector, event_sender, client_receiver)
    .run()
    .await;
  info!("Exiting client event loop");
  Ok(())
}
