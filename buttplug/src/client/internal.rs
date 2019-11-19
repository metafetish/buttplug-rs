// Buttplug Rust Source Code File - See https://buttplug.io for more info.
//
// Copyright 2016-2019 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

//! Implementation of internal Buttplug Client event loop.

use super::{
    connectors::{
        ButtplugClientConnectionStateShared, ButtplugClientConnector, ButtplugClientConnectorError,
    },
    device::ButtplugClientDevice,
    ButtplugClientResult, ButtplugClientEvent,
};
use crate::core::{
    messages::{ButtplugMessageUnion, DeviceList, DeviceMessageInfo},
};
use async_std::{
    future::Future,
    prelude::{FutureExt, StreamExt},
    sync::{channel, Receiver, Sender},
    task::{Context, Poll, Waker},
};
use core::pin::Pin;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Struct used for waiting on replies from the server.
///
/// When a ButtplugMessage is sent to the server, it may take an indeterminate
/// amount of time to get a reply. This struct holds the reply, as well as a
/// [Waker] for the related future. Once the reply_msg is filled, the waker will
/// be called to finish the future polling.
#[derive(Debug, Clone)]
pub struct ButtplugClientFutureState<T> {
    reply_msg: Option<T>,
    waker: Option<Waker>,
}

// For some reason, deriving default above doesn't work, but doing an explicit
// derive here does work.
impl<T> Default for ButtplugClientFutureState<T> {
    fn default() -> Self {
        ButtplugClientFutureState::<T> {
            reply_msg: None,
            waker: None,
        }
    }
}

impl<T> ButtplugClientFutureState<T> {
    /// Sets the reply message in a message state struct, firing the waker.
    ///
    /// When a reply is received from (or in the in-process case, generated by)
    /// a server, this function takes the message, updates the state struct, and
    /// calls [Waker::wake] so that the corresponding future can finish.
    ///
    /// # Parameters
    ///
    /// - `msg`: Message to set as reply, which will be returned by the
    /// corresponding future.
    pub fn set_reply(&mut self, reply: T) {
        if self.reply_msg.is_some() {
            // TODO Can we stop multiple calls to set_reply_msg at compile time?
            panic!("set_reply_msg called multiple times on the same future.");
        }

        self.reply_msg = Some(reply);

        if self.waker.is_some() {
            self.waker.take().unwrap().wake();
        }
    }
}

/// Shared [ButtplugClientConnectionStatus] type.
///
/// [ButtplugClientConnectionStatus] is made to be shared across futures, and we'll
/// never know if those futures are single or multithreaded. Only needs to
/// unlock for calls to [ButtplugClientConnectionStatus::set_reply_msg].
pub type ButtplugClientFutureStateShared<T> = Arc<Mutex<ButtplugClientFutureState<T>>>;

/// [Future] implementation for [ButtplugMessageUnion] types send to the server.
///
/// A [Future] implementation that we can always expect to return a
/// [ButtplugMessageUnion]. Used to deal with getting server replies after
/// sending [ButtplugMessageUnion] types via the client API.
#[derive(Debug)]
pub struct ButtplugClientFuture<T> {
    /// State that holds the waker for the future, and the [ButtplugMessageUnion] reply (once set).
    ///
    /// ## Notes
    ///
    /// This needs to be an [Arc]<[Mutex]<T>> in order to make it mutable under
    /// pinning when dealing with being a future. There is a chance we could do
    /// this as a [Pin::get_unchecked_mut] borrow, which would be way faster, but
    /// that's dicey and hasn't been proven as needed for speed yet.
    waker_state: ButtplugClientFutureStateShared<T>,
}

impl<T> Default for ButtplugClientFuture<T> {
    fn default() -> Self {
        ButtplugClientFuture::<T> {
            waker_state: ButtplugClientFutureStateShared::<T>::default(),
        }
    }
}

impl<T> ButtplugClientFuture<T> {
    /// Returns a clone of the state, used for moving the state across contexts
    /// (tasks/threads/etc...).
    pub fn get_state_clone(&self) -> ButtplugClientFutureStateShared<T> {
        self.waker_state.clone()
    }

    // TODO Should we implement drop on this, so it'll yell if its dropping and
    // the waker didn't fire? otherwise it seems like we could have quiet
    // deadlocks.
}

impl<T> Future for ButtplugClientFuture<T> {
    type Output = T;

    /// Returns when the [ButtplugMessageUnion] reply has been set in the
    /// [ButtplugClientConnectionStatusShared].
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut waker_state = self.waker_state.lock().unwrap();
        if waker_state.reply_msg.is_some() {
            let msg = waker_state.reply_msg.take().unwrap();
            Poll::Ready(msg)
        } else {
            debug!("Waker set.");
            waker_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

pub type ButtplugClientMessageState = ButtplugClientFutureState<ButtplugMessageUnion>;
pub type ButtplugClientMessageStateShared = ButtplugClientFutureStateShared<ButtplugMessageUnion>;
pub type ButtplugClientMessageFuture = ButtplugClientFuture<ButtplugMessageUnion>;

pub type ButtplugClientMessageFuturePair = (ButtplugMessageUnion, ButtplugClientMessageStateShared);

/// Enum used for communication from the client to the event loop.
pub enum ButtplugClientMessage {
    /// Client request to connect, via the included connector instance.
    ///
    /// Once connection is finished, use the bundled future to resolve.
    Connect(
        Box<dyn ButtplugClientConnector>,
        ButtplugClientConnectionStateShared,
    ),
    /// Client request to disconnect, via already sent connector instance.
    Disconnect(ButtplugClientConnectionStateShared),
    /// Given a DeviceList message, update the inner loop values and create
    /// events for additions.
    HandleDeviceList(DeviceList),
    /// Return new ButtplugClientDevice instances for all known and currently
    /// connected devices.
    RequestDeviceList(ButtplugClientFutureStateShared<Vec<ButtplugClientDevice>>),
    /// Client request to send a message via the connector.
    ///
    /// Bundled future should have reply set and waker called when this is
    /// finished.
    Message(ButtplugClientMessageFuturePair),
}

pub enum ButtplugClientDeviceEvent {
    DeviceDisconnect,
    ClientDisconnect,
    Message(ButtplugMessageUnion),
}

enum StreamReturn {
    ConnectorMessage(ButtplugMessageUnion),
    ClientMessage(ButtplugClientMessage),
    DeviceMessage(ButtplugClientMessageFuturePair),
    Disconnect,
}

struct ButtplugClientEventLoop {
    devices: HashMap<u32, DeviceMessageInfo>,
    device_message_sender: Sender<ButtplugClientMessageFuturePair>,
    device_message_receiver: Receiver<ButtplugClientMessageFuturePair>,
    device_event_senders: HashMap<u32, Vec<Sender<ButtplugClientDeviceEvent>>>,
    event_sender: Sender<ButtplugClientEvent>,
    client_receiver: Receiver<ButtplugClientMessage>,
    connector: Box<dyn ButtplugClientConnector>,
    connector_receiver: Receiver<ButtplugMessageUnion>,
}

impl ButtplugClientEventLoop {
    pub async fn wait_for_connector(
        event_sender: Sender<ButtplugClientEvent>,
        mut client_receiver: Receiver<ButtplugClientMessage>,
    ) -> Result<Self, ButtplugClientConnectorError> {
        match client_receiver.next().await {
            None => {
                debug!("Client disconnected.");
                Err(ButtplugClientConnectorError::new("Client was dropped during connect."))
            }
            Some(msg) => match msg {
                ButtplugClientMessage::Connect(mut connector, state) => {
                    match connector.connect().await {
                        Err(err) => {
                            error!("Cannot connect to server: {}", err.message);
                            let mut waker_state = state.lock().unwrap();
                            let reply = Err(ButtplugClientConnectorError::new(&format!(
                                "Cannot connect to server: {}",
                                err.message
                            )));
                            waker_state.set_reply(reply);
                            Err(ButtplugClientConnectorError::new("Client couldn't connect to server."))
                        }
                        Ok(_) => {
                            info!("Connected!");
                            let mut waker_state = state.lock().unwrap();
                            waker_state.set_reply(Ok(()));
                            let (device_message_sender, device_message_receiver) = channel(256);
                            Ok(ButtplugClientEventLoop {
                                devices: HashMap::new(),
                                device_event_senders: HashMap::new(),
                                device_message_sender,
                                device_message_receiver,
                                event_sender,
                                client_receiver,
                                connector_receiver: connector.get_event_receiver(),
                                connector,
                            })
                        }
                    }
                }
                _ => {
                    error!("Received non-connector message before connector message.");
                    Err(ButtplugClientConnectorError::new("Event Loop did not receive Connect message first."))
                }
            },
        }
    }

    fn create_client_device(&mut self, info: &DeviceMessageInfo) -> ButtplugClientDevice {
        let (event_sender, event_receiver) = channel(256);
        self.device_event_senders
            .entry(info.device_index)
            .or_insert_with(|| vec![])
            .push(event_sender);
        ButtplugClientDevice::from((info, self.device_message_sender.clone(), event_receiver))
    }

    async fn parse_connector_message(&mut self, msg: ButtplugMessageUnion) {
        info!("Sending message to clients.");
        match &msg {
            ButtplugMessageUnion::DeviceAdded(dev) => {
                let info = DeviceMessageInfo::from(dev);
                let device = self.create_client_device(&info);
                self.devices.insert(dev.device_index, info);
                self.event_sender
                    .send(ButtplugClientEvent::DeviceAdded(device))
                    .await;
            }
            ButtplugMessageUnion::DeviceList(dev) => {
                for d in &dev.devices {
                    let device = self.create_client_device(&d);
                    self.devices.insert(d.device_index, d.clone());
                    self.event_sender
                        .send(ButtplugClientEvent::DeviceAdded(device))
                        .await;
                }
            }
            ButtplugMessageUnion::DeviceRemoved(dev) => {
                let info = self.devices.remove(&dev.device_index);
                self.device_event_senders.remove(&dev.device_index);
                self.event_sender
                    .send(ButtplugClientEvent::DeviceRemoved(info.unwrap()))
                    .await;
            }
            _ => panic!("Got connector message type we don't know how to handle!"),
        }
    }

    async fn parse_client_message(&mut self, msg: ButtplugClientMessage) -> bool {
        debug!("Parsing a client message.");
        match msg {
            ButtplugClientMessage::Message(msg_fut) => {
                debug!("Sending message through connector.");
                self.connector.send(&msg_fut.0, &msg_fut.1).await;
                true
            }
            ButtplugClientMessage::Disconnect(state) => {
                info!("Client requested disconnect");
                let mut waker_state = state.lock().unwrap();
                waker_state.set_reply(self.connector.disconnect().await);
                false
            }
            ButtplugClientMessage::RequestDeviceList(fut) => {
                info!("Building device list!");
                let mut r = vec![];
                // TODO There's probably a better way to do this.
                let devices = self.devices.clone();
                for d in devices.values() {
                    let dev = self.create_client_device(d);
                    r.push(dev);
                }
                info!("Returning device list of {} items!", r.len());
                let mut waker_state = fut.lock().unwrap();
                waker_state.set_reply(r);
                info!("Finised setting waker!");
                true
            }
            ButtplugClientMessage::HandleDeviceList(device_list) => {
                info!("Handling device list!");
                for d in &device_list.devices {
                    let device = self.create_client_device(&d);
                    self.devices.insert(d.device_index, d.clone());
                    self.event_sender
                        .send(ButtplugClientEvent::DeviceAdded(device))
                        .await;
                }
                true
            }
            // TODO Do something other than panic if someone does
            // something like trying to connect twice..
            _ => panic!("Client message not handled!"),
        }
    }

    pub async fn run(&mut self) {
        // Once connected, wait for messages from either the client or the
        // connector, and send them the direction they're supposed to go.
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
                    self.connector.send(&msg_fut.0, &msg_fut.1).await;
                }
                StreamReturn::Disconnect => {
                    info!("Disconnected!");
                    break;
                }
            }
        }
    }
}

/// The internal event loop for [ButtplugClient] connection and
/// communication
///
/// Created whenever a new [ButtplugClient] is created, the internal loop
/// handles connection and communication with the server, and creation of events
/// received from the server. As [ButtplugClient] is clonable, multiple
/// ButtplugClient instances can exist that all communicate with the same
/// [client_event_loop] created future.
///
/// Also, if multiple [ButtplugClient] instances are created via new(), multiple
/// [client_event_loop] futures can run in parallel. This allows applications
///
/// The event_loop does a few different things during its lifetime.
///
/// - The first thing it will do is wait for a Connect message from a
/// client. This message contains a [ButtplugClientConnector] that will be
/// used to connect and communicate with a [ButtplugServer].
///
/// - After a connection is established, it will listen for events from the
/// connector, or messages from the client, until either server/client
/// disconnects.
///
/// - Finally, on disconnect, it will tear down, and cannot be used again.
/// All clients and devices associated with the loop will be invalidated,
/// and a new [ButtplugClient] (and corresponding
/// [ButtplugClientInternalLoop]) must be created.
///
/// # Parameters
///
/// - `event_sender`: Used when sending server updates to clients.
/// - `client_receiver`: Used when receiving commands from clients to
/// send to server.
pub async fn client_event_loop(
    event_sender: Sender<ButtplugClientEvent>,
    client_receiver: Receiver<ButtplugClientMessage>,
) -> ButtplugClientResult {
    info!("Starting client event loop.");
    ButtplugClientEventLoop::wait_for_connector(event_sender, client_receiver)
        .await?
        .run()
        .await;
    info!("Exiting client event loop");
    Ok(())
}
