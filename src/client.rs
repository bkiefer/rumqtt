use std::time::{Duration, Instant};
use time;

use std::net::{SocketAddr, ToSocketAddrs, Shutdown};
use std::collections::VecDeque;
use std::io::Write;
use std::str;
use std::net::TcpStream;
use mio::*;
use mqtt::{Encodable, Decodable, QualityOfService, TopicFilter};
use mqtt::packet::*;
use mqtt::control::variable_header::{ConnectReturnCode, PacketIdentifier};
use mqtt::topic_name::TopicName;
use std::sync::Arc;
use std::thread;
use tls::{NetworkStream, TlsStream};
use std::sync::mpsc;
use jobsteal;

use error::{Error, Result};
use message::Message;
use clientoptions::MqttOptions;
use publisher::Publisher;
use subscriber::{Subscriber, SendableFn};

const MIO_PING_TIMER: u64 = 123;
const MIO_QUEUE_TIMER: u64 = 321;

// static mut N: i32 = 0;
// unsafe {
//     N += 1;
//     println!("N: {}", N);
// }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttState {
    Handshake,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttStatus {
    Success,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PubNotify {
    QoS0,
    QoS0Reconnect,
    QoS1,
    QoS1QueueDown,
    QoS1Reconnect,
    QoS2,
    QoS2QueueDown,
    QoS2Reconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MioNotification {
    Pub(PubNotify),
    Sub,
    Disconnect,
    Shutdown,
    Incoming,
    Reconnect,
}

enum HandlePacket {
    ConnAck,
    Publish(Box<Message>),
    PubAck,
    PubRec,
    // PubRel,
    PubComp,
    SubAck,
    UnSubAck,
    PingResp,
    Disconnect,
    Invalid,
}


/// Handles commands from Publisher and Subscriber. Saves MQTT
/// state and takes care of retransmissions.
pub struct MqttClient {
    pub addr: SocketAddr,
    pub opts: MqttOptions,
    pub stream: NetworkStream,

    // state
    pub state: MqttState,
    pub last_flush: Instant,
    pub last_pkid: PacketIdentifier,
    pub await_ping: bool,
    pub initial_connect: bool,
    // no. of pending messages in qos0 pub channel
    pub pub0_channel_pending: u32,
    // no. of pending messages in qos1 pub channel
    pub pub1_channel_pending: u32,
    // no. of pending messages in qos2 pub channel
    pub pub2_channel_pending: u32,
    // stop reading from all the publish channels when disconnected
    pub disconnect_block: bool,
    // stop reading from pub1 channel when qos1 queue is full(didn't get acks)
    pub should_qos1_block: bool,
    // stop reading from pub2 channel when qos2 queue is full
    pub should_qos2_block: bool,
    // no. of successful reconnections
    pub no_of_reconnections: u32,

    // Channels
    pub pub0_rx: Option<mpsc::Receiver<Message>>,
    pub pub1_rx: Option<mpsc::Receiver<Message>>,
    pub pub2_rx: Option<mpsc::Receiver<Message>>,
    pub sub_rx: Option<mpsc::Receiver<Vec<(TopicFilter, QualityOfService)>>>,
    pub incoming_rx: Option<mpsc::Receiver<VariablePacket>>,
    pub mionotify_tx: Option<Sender<MioNotification>>,
    pub connsync_tx: Option<mpsc::SyncSender<MqttStatus>>,
    pub streamupdate_tx: Option<mpsc::SyncSender<NetworkStream>>,

    /// Queues. Note: 'record' is qos2 term for 'publish'
    /// For QoS 1. Stores outgoing publishes
    pub outgoing_pub: VecDeque<(i64, Box<Message>)>,
    /// For QoS 2. Store for incoming publishes to record.
    pub incoming_rec: VecDeque<Box<Message>>, //
    /// For QoS 2. Store for outgoing publishes.
    pub outgoing_rec: VecDeque<(i64, Box<Message>)>,
    /// For Qos2. Store for outgoing `pubrel` packets.
    pub outgoing_rel: VecDeque<(i64, PacketIdentifier)>,

    // clean_session=false will remember subscriptions only till lives.
    // If broker crashes, all its state will be lost (most brokers).
    // client wouldn't want to loose messages after it comes back up again
    pub subscriptions: VecDeque<Vec<(TopicFilter, QualityOfService)>>,

    /// On message callback
    pub callback: Option<Arc<SendableFn>>,
    pub pool: Option<jobsteal::Pool>,
}

// TODO: Use Mio::Handler, Unit test for state machine
impl Handler for MqttClient {
    type Timeout = u64;
    type Message = MioNotification;

    fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timer: Self::Timeout) {
        // TODO: Move timer handling logic to seperate methods
        match timer {
            MIO_PING_TIMER => {
                debug!("client state --> {:?}, await_ping --> {}", self.state, self.await_ping);

                match self.state {
                    MqttState::Connected => {
                        if !self.await_ping {
                            let _ = self.ping();
                        } else {
                            error!("awaiting for previous ping resp");
                        }

                        if let Some(keep_alive) = self.opts.keep_alive {
                            event_loop.timeout_ms(MIO_PING_TIMER, keep_alive as u64 * 900).unwrap();
                        }
                    }

                    MqttState::Disconnected | MqttState::Handshake => {
                        error!("I won't ping. Client is in disconnected/handshake state")
                    }
                }
            }

            MIO_QUEUE_TIMER => {
                match self.state {
                    MqttState::Connected => {
                        debug!("^^^ QUEUE RESEND");
                        self._try_retransmit();
                    }
                    MqttState::Disconnected | MqttState::Handshake => {
                        debug!("I won't republish. Client is in disconnected/handshake state")
                    }
                }
                event_loop.timeout_ms(MIO_QUEUE_TIMER, self.opts.queue_timeout as u64 * 1000)
                    .unwrap();

            }

            _ => panic!("Invalid timer id"),

        }
    }

    // TODO: Make smaller methods
    fn notify(&mut self, event_loop: &mut EventLoop<Self>, notification_type: MioNotification) {
        // No Mqtt state distiction here. Should receive messages from channel in all
        // the states
        match notification_type {
            MioNotification::Pub(p) => {
                match p {
                    // FIX THIS: All the QoS0 publishes before listener thread notifies
                    // event loop (this) that connection is lost (where disconnect_block is set)
                    // are lost
                    PubNotify::QoS0 |
                    PubNotify::QoS0Reconnect => {
                        // Increment only if notificication is from publisher
                        if p == PubNotify::QoS0 {
                            self.pub0_channel_pending += 1;
                        }

                        debug!("Channel pending @@@@@ {}", self.pub0_channel_pending);
                        // Receive from publish qos0 channel only when connected.
                        if !self.disconnect_block {
                            loop {
                                if self.pub0_channel_pending == 0 {
                                    debug!("Finished everything in channel");
                                    break;
                                }
                                let message = {
                                    let pub0_rx = self.pub0_rx.as_ref().unwrap();
                                    pub0_rx.recv().expect("Pub0 Rx Recv Error")
                                };
                                let _ = self._publish(message);
                                self.pub0_channel_pending -= 1;
                            }
                        }
                    }
                    PubNotify::QoS1 |
                    PubNotify::QoS1QueueDown |
                    PubNotify::QoS1Reconnect => {
                        // Increment only if notificication is from publisher
                        if p == PubNotify::QoS1 {
                            self.pub1_channel_pending += 1;
                        }

                        debug!("Channel pending @@@@@ {}", self.pub1_channel_pending);
                        // Receive from publish qos1 channel only when outgoing pub queue
                        // length is < max and in connected state
                        if !self.should_qos1_block && !self.disconnect_block {
                            loop {
                                if self.pub1_channel_pending == 0 {
                                    debug!("Finished everything in channel");
                                    break;
                                }
                                let mut message = {
                                    let pub1_rx = self.pub1_rx.as_ref().unwrap();
                                    pub1_rx.recv().expect("Pub1 Rx Recv Error")
                                };
                                // Add next packet id to message and publish
                                let PacketIdentifier(pkid) = self._next_pkid();
                                message.set_pkid(pkid);
                                let _ = self._publish(message);
                                self.pub1_channel_pending -= 1;
                            }
                        }
                    }

                    PubNotify::QoS2 |
                    PubNotify::QoS2QueueDown |
                    PubNotify::QoS2Reconnect => {
                        // Increment only if notificication is from publisher
                        if p == PubNotify::QoS2 {
                            self.pub2_channel_pending += 1;
                        }

                        debug!("QoS2 Channel pending @@@@@ {}", self.pub2_channel_pending);
                        // Receive from publish qos2 channel only when outgoing pub queue
                        // length is < max and in connected state
                        if !self.should_qos2_block && !self.disconnect_block {
                            loop {
                                // Before
                                if self.pub2_channel_pending == 0 {
                                    debug!("Finished everything in channel");
                                    break;
                                }
                                let mut message = {
                                    // Careful, this is a blocking call. Might
                                    // be easier to find queue len bugs with this.
                                    let pub2_rx = self.pub2_rx.as_ref().unwrap();
                                    pub2_rx.recv().expect("Pub2 Rx Recv Error")
                                };
                                // Add next packet id to message and publish
                                let PacketIdentifier(pkid) = self._next_pkid();
                                message.set_pkid(pkid);
                                let _ = self._publish(message);
                                self.pub2_channel_pending -= 1;
                            }
                        }
                    }
                }
            }
            MioNotification::Sub => {
                let topics = {
                    let sub_rx = self.sub_rx.as_ref().unwrap();
                    sub_rx.recv().expect("Sub Rx Recv Error")
                };
                self.subscriptions.push_back(topics.clone());
                let _ = self._subscribe(topics);
            }
            MioNotification::Disconnect => {
                debug!("{:?}", self.state);
                match self.state {
                    MqttState::Connected => {
                        let _ = self._disconnect();
                    }
                    _ => debug!("Mqtt connection not established"),
                }
            }
            MioNotification::Incoming => {
                let packet = {
                    let incoming_rx = self.incoming_rx.as_ref().unwrap();
                    incoming_rx.recv().expect("Incoming Rx Recv Error")
                };
                self.STATE_handle_packet(&packet, event_loop);
            }
            MioNotification::Reconnect => {
                debug!("{:?}", self.state);

                self.state = MqttState::Disconnected;
                self.disconnect_block = true;
                loop {
                    match self._try_reconnect() {
                        Ok(_) => break,
                        Err(_) => continue,
                    }
                }

                // Handles the case where initial tcp connect is successful and mqtt connect
                // packets are sent (_try_reconnect) but broker closed the connection without
                // sending CONNACK. Broker might be expecting TLS or username & password
                if self.initial_connect {
                    let connsync_tx = self.connsync_tx.as_ref().unwrap();
                    connsync_tx.send(MqttStatus::Failed).expect("ConnSync Tx Send Error");
                    event_loop.shutdown();
                } else {
                    let streamupdate_tx = self.streamupdate_tx.as_ref().unwrap();
                    let stream = self.stream.try_clone().expect("Stream Clone Error");
                    streamupdate_tx.send(stream).expect("StreamUpdate Tx Send Error");
                }
            }
            MioNotification::Shutdown => {
                let _ = self.stream.shutdown(Shutdown::Both);
            }
        }
    }
}

impl MqttClient {
    fn lookup_ipv4<A: ToSocketAddrs>(addr: A) -> SocketAddr {
        let addrs = addr.to_socket_addrs().expect("Conversion Failed");
        for addr in addrs {
            if let SocketAddr::V4(_) = addr {
                return addr;
            }
        }
        unreachable!("Cannot lookup address");
    }

    pub fn new(opts: MqttOptions) -> Self {
        let addr = opts.addr.clone();
        let addr = Self::lookup_ipv4(addr.as_str());
        // TODO: Move state initialization to MqttClient constructor
        MqttClient {
            addr: addr,
            stream: NetworkStream::None,

            // State
            last_flush: Instant::now(),
            last_pkid: PacketIdentifier(0),
            await_ping: false,
            state: MqttState::Disconnected,
            initial_connect: true,
            opts: opts,
            pub0_channel_pending: 0,
            pub1_channel_pending: 0,
            pub2_channel_pending: 0,
            disconnect_block: false,
            should_qos1_block: false,
            should_qos2_block: false,
            no_of_reconnections: 0,

            // Channels
            pub0_rx: None,
            pub1_rx: None,
            pub2_rx: None,
            sub_rx: None,
            incoming_rx: None,
            mionotify_tx: None,
            connsync_tx: None,
            streamupdate_tx: None,

            // Queues
            incoming_rec: VecDeque::new(),
            outgoing_pub: VecDeque::new(),
            outgoing_rec: VecDeque::new(),
            outgoing_rel: VecDeque::new(),

            // Subscriptions
            subscriptions: VecDeque::new(),

            // callback
            callback: None,
            pool: None,
        }
    }
    // Note: Setting callback before subscriber & publisher
    // are created ensures that message callbacks are registered
    // before subscription & you don't need to pass callbacks through
    // channels (simplifies code)
    pub fn message_callback<F>(mut self, callback: F) -> Self
        where F: Fn(Message) + Send + Sync + 'static
    {
        // Build a pool with 4 threads, including this one.
        let pool = jobsteal::make_pool(4).expect("couldn't create thread pool");
        self.pool = Some(pool);
        self.callback = Some(Arc::new(Box::new(callback)));
        self
    }

    /// Connects to the broker and starts an event loop in a new thread.
    /// Returns `Subscriber` and `Publisher` and handles reqests from them.
    /// Also handles network events, reconnections and retransmissions.
    pub fn start(mut self) -> Result<(Publisher, Subscriber)> {
        let mut event_loop = EventLoop::new().unwrap();
        let mionotify_tx = event_loop.channel();
        self.mionotify_tx = Some(mionotify_tx.clone());

        let (pub0_tx, pub0_rx) = mpsc::sync_channel::<Message>(self.opts.pub_q_len as usize);
        self.pub0_rx = Some(pub0_rx);
        let (pub1_tx, pub1_rx) = mpsc::sync_channel::<Message>(self.opts.pub_q_len as usize);
        self.pub1_rx = Some(pub1_rx);
        let (pub2_tx, pub2_rx) = mpsc::sync_channel::<Message>(self.opts.pub_q_len as usize);
        self.pub2_rx = Some(pub2_rx);

        let (sub_tx, sub_rx) = mpsc::sync_channel::<Vec<(TopicFilter, QualityOfService)>>(self.opts.sub_q_len as usize);
        self.sub_rx = Some(sub_rx);

        let (incoming_tx, incoming_rx) = mpsc::sync_channel::<VariablePacket>(1);
        self.incoming_rx = Some(incoming_rx);

        // synchronizes tcp connection. why ?
        // start() call should fail if there a problem creating initial tcp
        // connection & mqtt connection. Since connections are happening inside thread,
        // this method should be informed to return error instead of
        // (publisher, subscriber) in case connection fails.
        let (connsync_tx, connsync_rx) = mpsc::sync_channel::<MqttStatus>(1);
        self.connsync_tx = Some(connsync_tx);

        let (streamupdate_tx, streamupdate_rx) = mpsc::sync_channel::<NetworkStream>(1);
        self.streamupdate_tx = Some(streamupdate_tx.clone());

        // @ Create 'publisher' and 'subscriber'
        // @ These are the handles using which user interacts with rumqtt.
        let publisher = Publisher {
            pub0_tx: pub0_tx,
            pub1_tx: pub1_tx,
            pub2_tx: pub2_tx,
            mionotify_tx: mionotify_tx.clone(),
            retain: false,
        };

        let subscriber = Subscriber {
            subscribe_tx: sub_tx,
            mionotify_tx: mionotify_tx.clone(),
        };

        // Initial Mqtt connection
        try!(self._try_reconnect());

        let reader_stream = self.stream.try_clone().expect("Couldn't clone the stream");
        try!(streamupdate_tx.send(reader_stream));

        // This is the thread that handles mio event loop.
        // Mio event loop is intentionally made to use just notify and timers.
        // Tcp Streams are std blocking streams.This helps avoiding the state
        // machine hell.
        // All the network writes also happen in this thread
        // TODO: Handle thread death
        thread::spawn(move || {
            event_loop.run(&mut self).expect("Couldn't Run EventLoop");
        });

        // This thread handles network reads (coz they are blocking) and
        // and sends them to event loop thread to handle mqtt state.
        thread::spawn(move || {
            'update_stream: loop {
                let mut stream = streamupdate_rx.recv().expect("Stream update channel error");
                loop {
                    let packet = match VariablePacket::decode(&mut stream) {
                        // @ Decoded packet successfully.
                        Ok(pk) => pk,
                        Err(err) => {
                            // Socket error are readily available here as soon as
                            // disconnection happens. So it might be right for this
                            // thread to ask for reconnection rather than reconnecting
                            // during write failures
                            // UPDATE: Lot of publishes are being written by the time this notified
                            // the eventloop thread. Setting disconnect_block = true during write failure
                            error!("Error in receiving packet {:?}", err);
                            mionotify_tx.send(MioNotification::Reconnect).expect("Unable to Notify");
                            continue 'update_stream;
                        }
                        // Err(err) => {
                        //     // maybe size=0 while reading indicating socket
                        //     // close at broker end
                        //     panic!("Error in receiving packet {:?}", err);
                        //     // self._unbind();
                        //     // TODO: Return actual error
                        //     //Err(Error::Read)
                        // }
                    };
                    incoming_tx.send(packet).expect("Unable to send incoming message");
                    mionotify_tx.send(MioNotification::Incoming).expect("Unable to Notify");
                }
            }
        });

        let conn = connsync_rx.recv().expect("Connection sync recv error");
        match conn {
            MqttStatus::Success => Ok((publisher, subscriber)),
            MqttStatus::Failed => Err(Error::ConnectionAbort),
        }
    }

    /// Return a count of (successful) mqtt connections that happened from the
    /// start.
    /// Just to know how many times the client reconnected (coz of bad
    /// networks, broker crashes etc)
    pub fn get_reconnection_count(&self) -> u32 { self.no_of_reconnections }

    #[allow(non_snake_case)]
    fn STATE_handle_packet(&mut self, packet: &VariablePacket, event_loop: &mut EventLoop<Self>) {
        let handle = self.handle_packet(packet);
        if let Ok(p) = handle {
            match p {
                // Mqtt connection established, release (publisher, subscriber) & start the timers
                HandlePacket::ConnAck => {
                    self.state = MqttState::Connected;
                    self.no_of_reconnections += 1;
                    if self.initial_connect {
                        let connsync_tx = self.connsync_tx.as_ref().unwrap();
                        connsync_tx.send(MqttStatus::Success).expect("ConnSync Tx Send Error");
                        self.initial_connect = false;
                    } else {
                        // Resubscribe after a reconnection.
                        for s in self.subscriptions.clone() {
                            let _ = self._subscribe(s);
                        }
                        // Retransmit QoS1,2 queues after reconnection. Clears the queue by the time
                        // QoS*Reconnect notifications are sent to read pending messages in the channel
                        self._force_retransmit();
                        // Publisher won't stop even when disconnected until channel is full.
                        // This notifies notify() to publish channel pending messages after
                        // reconnect.
                        let mionotify_tx = self.mionotify_tx.as_ref().unwrap();
                        self.disconnect_block = false;
                        mionotify_tx.send(MioNotification::Pub(PubNotify::QoS0Reconnect)).expect("MioNotify Tx Send Error");
                        mionotify_tx.send(MioNotification::Pub(PubNotify::QoS1Reconnect)).expect("MioNotify Tx Send Error");
                        mionotify_tx.send(MioNotification::Pub(PubNotify::QoS2Reconnect)).expect("MioNotify Tx Send Error");
                    }

                    // TODO: Bug?? Multiple timers after restart? Doesn't seem so based on pings
                    event_loop.timeout_ms(MIO_QUEUE_TIMER, self.opts.queue_timeout as u64 * 1000)
                        .unwrap();
                    if let Some(keep_alive) = self.opts.keep_alive {
                        event_loop.timeout_ms(MIO_PING_TIMER, keep_alive as u64 * 900).unwrap();
                    }

                }
                HandlePacket::Publish(m) => {
                    if let Some(ref message_callback) = self.callback {
                        let message_callback = message_callback.clone();

                        // Have a thread pool to handle message callbacks. Take the threadpool as a
                        // parameter
                        let pool = self.pool.as_mut().unwrap();
                        pool.submit(move || message_callback(*m));
                        // thread::spawn(move || message_callback(*m));
                    }
                }
                // Sending a dummy notification saying tha queue size has reduced
                HandlePacket::PubAck => {
                    // Don't notify everytime q len is < max. This will always be true initially
                    // leading to dup notify.
                    // Send only for notify() to recover if channel is blocked.
                    // Blocking = true is set during publish if pub q len is more than desired.
                    if self.outgoing_pub.len() < self.opts.pub_q_len as usize && self.should_qos1_block {
                        let mionotify_tx = self.mionotify_tx.as_ref().unwrap();
                        self.should_qos1_block = false;
                        mionotify_tx.send(MioNotification::Pub(PubNotify::QoS1QueueDown)).expect("MioNotify Tx Send Error");
                    }
                }
                // TODO: Better read from channel again after PubComp instead of PubRec
                HandlePacket::PubRec => {
                    if self.outgoing_rec.len() < self.opts.pub_q_len as usize && self.should_qos2_block {
                        let mionotify_tx = self.mionotify_tx.as_ref().unwrap();
                        self.should_qos2_block = false;
                        mionotify_tx.send(MioNotification::Pub(PubNotify::QoS2QueueDown)).expect("MioNotify Tx Send Error");
                    }
                }
                _ => info!("packet handler says that he doesn't care"),
            }
        } else if let Err(err) = handle {
            error!("Error handling the packet {:?}", err);
        }
    }

    fn handle_packet(&mut self, packet: &VariablePacket) -> Result<HandlePacket> {
        match self.state {
            MqttState::Handshake => {
                match *packet {
                    VariablePacket::ConnackPacket(ref connack) => {
                        let conn_ret_code = connack.connect_return_code();
                        if conn_ret_code != ConnectReturnCode::ConnectionAccepted {
                            error!("Failed to connect, err {:?}", conn_ret_code);
                            Err(Error::ConnectionRefused(conn_ret_code))
                        } else {
                            Ok(HandlePacket::ConnAck)
                        }
                    }
                    _ => {
                        error!("received invalid packet in handshake state --> {:?}", packet);
                        Ok(HandlePacket::Invalid)
                    }
                }
            }

            MqttState::Connected => {
                match *packet {
                    VariablePacket::SubackPacket(..) => {
                        // if ack.packet_identifier() != 10
                        // TODO: Maintain a subscribe queue and retry if
                        // subscribes are not successful
                        Ok(HandlePacket::SubAck)
                    }

                    VariablePacket::PingrespPacket(..) => {
                        self.await_ping = false;
                        Ok(HandlePacket::PingResp)
                    }

                    // @ Receives disconnect packet
                    VariablePacket::DisconnectPacket(..) => {
                        // TODO
                        Ok(HandlePacket::Disconnect)
                    }

                    // @ Receives puback packet and verifies it with sub packet id
                    VariablePacket::PubackPacket(ref puback) => {
                        // debug!("*** puback --> {:?}\n @@@ queue --> {:#?}",
                        //        puback,
                        //        self.outgoing_pub);
                        let pkid = puback.packet_identifier();
                        match self.outgoing_pub
                            .iter()
                            .position(|ref x| x.1.get_pkid() == Some(pkid)) {
                            Some(i) => {
                                self.outgoing_pub.remove(i);
                            }
                            None => {
                                error!("Oopssss..unsolicited ack --> {:?}", puback);
                            }
                        };
                        debug!("Pub Q Len After Ack @@@ {:?}", self.outgoing_pub.len());
                        Ok(HandlePacket::PubAck)
                    }

                    // @ Receives publish packet
                    VariablePacket::PublishPacket(ref publ) => {
                        // unsafe {
                        //     N += 1;
                        //     println!("N: {}", N);
                        // }
                        let message = try!(Message::from_pub(publ));
                        self._handle_message(message)
                    }

                    // @ Qos2 message published by client is recorded by broker
                    // @ Remove message from 'outgoing_rec' queue and add pkid to 'outgoing_rel'
                    // @ Send 'pubrel' to broker
                    VariablePacket::PubrecPacket(ref pubrec) => {
                        let pkid = pubrec.packet_identifier();
                        match self.outgoing_rec
                            .iter()
                            .position(|ref x| x.1.get_pkid() == Some(pkid)) {
                            Some(i) => {
                                self.outgoing_rec.remove(i);
                            }
                            None => {
                                error!("Oopssss..unsolicited record --> {:?}", pubrec);
                            }
                        };

                        try!(self._pubrel(pkid));
                        self.outgoing_rel.push_back((time::get_time().sec, PacketIdentifier(pkid)));
                        Ok(HandlePacket::PubRec)
                    }

                    // @ Broker knows that client has the message
                    // @ release the message stored in 'recorded' queue
                    // @ send 'pubcomp' to sender indicating that message is released
                    // @ if 'pubcomp' packet is lost, broker will send pubrel again
                    // @ for the released message, for which we send dummy 'pubcomp' again
                    VariablePacket::PubrelPacket(ref pubrel) => {
                        let pkid = pubrel.packet_identifier();
                        let message = match self.incoming_rec
                            .iter()
                            .position(|ref x| x.get_pkid() == Some(pkid)) {
                            Some(i) => {
                                if let Some(message) = self.incoming_rec.remove(i) {
                                    Some(message)
                                } else {
                                    None
                                }
                            }
                            None => {
                                error!("Oopssss..unsolicited release. Message might have already \
                                        been released --> {:?}",
                                       pubrel);
                                None
                            }
                        };
                        try!(self._pubcomp(pkid));

                        if let Some(message) = message {
                            Ok(HandlePacket::Publish(message))
                        } else {
                            Ok(HandlePacket::Invalid)
                        }
                    }

                    // @ Remove this pkid from 'outgoing_rel' queue
                    VariablePacket::PubcompPacket(ref pubcomp) => {
                        let pkid = pubcomp.packet_identifier();
                        match self.outgoing_rel
                            .iter()
                            .position(|ref x| x.1 == PacketIdentifier(pkid)) {
                            Some(pos) => self.outgoing_rel.remove(pos),
                            None => {
                                error!("Oopssss..unsolicited complete --> {:?}", pubcomp);
                                None
                            }
                        };
                        Ok(HandlePacket::PubComp)
                    }

                    VariablePacket::UnsubackPacket(..) => Ok(HandlePacket::UnSubAck),

                    _ => Ok(HandlePacket::Invalid), //TODO: Replace this with panic later
                }
            }

            MqttState::Disconnected => {
                error!("Invalid (Disconnected) state while handling packets");
                Ok(HandlePacket::Invalid)
            }
        }
    }

    // TODO: Rename to handle incoming publish
    fn _handle_message(&mut self, message: Box<Message>) -> Result<HandlePacket> {
        debug!("       Publish {:?} {:?} < {:?} bytes",
               message.qos,
               message.topic.to_string(),
               message.payload.len());
        match message.qos {
            QoSWithPacketIdentifier::Level0 => Ok(HandlePacket::Publish(message)),
            QoSWithPacketIdentifier::Level1(pkid) => {
                try!(self._puback(pkid));
                Ok(HandlePacket::Publish(message))
            }

            // @ store the message in 'recorded' queue and send 'pubrec' to broker
            // @ if 'pubrec' is lost, broker will resend the message.
            // @ so only pushback is pkid is new. and resend pubcomp.
            // @ TODO: Analyze broker crash cases for all queues.
            QoSWithPacketIdentifier::Level2(pkid) => {
                match self.incoming_rec
                    .iter()
                    .position(|ref x| x.get_pkid() == Some(pkid)) {
                    Some(i) => {
                        self.incoming_rec[i] = message.clone();
                    }
                    None => {
                        self.incoming_rec.push_back(message.clone());
                    }
                };

                try!(self._pubrec(pkid));
                Ok(HandlePacket::PubRec)
            }
        }
    }

    fn _connect(&mut self) -> Result<()> {
        let connect = try!(self._generate_connect_packet());
        try!(self._write_packet(connect));
        self._flush()
    }

    pub fn _disconnect(&mut self) -> Result<()> {
        let disconnect = try!(self._generate_disconnect_packet());
        try!(self._write_packet(disconnect));
        self._flush()
    }


    fn _try_reconnect(&mut self) -> Result<()> {
        match self.opts.reconnect {
            // TODO: Implement
            None => panic!("To be implemented"),
            Some(dur) => {
                if !self.initial_connect {
                    error!("  Will try Reconnect in {} seconds", dur);
                    thread::sleep(Duration::new(dur as u64, 0));
                }
                let stream = try!(TcpStream::connect(&self.addr));
                let stream = match self.opts.tls {
                    Some(ref tls) => {
                        let host = self.opts.addr.split(':');
                        let host: Vec<&str> = host.collect();
                        println!("@@@@ {:?}", host);

                        let tls_stream = try!(TlsStream::new_session(stream, host[0], tls));
                        NetworkStream::Tls(tls_stream)
                    }
                    None => NetworkStream::Tcp(stream),
                };
                self.stream = stream;
                try!(self._connect());
                                        println!("######");

                // TODO: Change states properly in one location
                self.state = MqttState::Handshake;
                Ok(())
            }
        }
    }

    fn _try_retransmit(&mut self) {
        match self.state {
            MqttState::Connected => {
                let timeout = self.opts.queue_timeout as i64;

                // Republish QoS 1 outgoing publishes
                while let Some(index) = self.outgoing_pub
                    .iter()
                    .position(|ref x| time::get_time().sec - x.0 > timeout) {
                    let message = self.outgoing_pub.remove(index).expect("No such entry");
                    let _ = self._publish(*message.1);
                }

                // Republish QoS 2 outgoing records
                while let Some(index) = self.outgoing_rec
                    .iter()
                    .position(|ref x| time::get_time().sec - x.0 > timeout) {
                    let message = self.outgoing_rec.remove(index).expect("No such entry");
                    let _ = self._publish(*message.1);
                }

                let outgoing_rel = self.outgoing_rel.clone(); //TODO: Remove the clone
                // Resend QoS 2 outgoing release
                for e in outgoing_rel.iter().filter(|ref x| time::get_time().sec - x.0 > timeout) {
                    let PacketIdentifier(pkid) = e.1;
                    let _ = self._pubrel(pkid);
                }
            }

            MqttState::Disconnected | MqttState::Handshake => error!("I won't republish. Client isn't in connected state"),
        }
    }

    // Spec says that client (for QoS > 0, clean session) should retransmit all the
    // unacked messages after reconnection. Instead of waiting for retransmit
    // timeout
    // to kickin, this methods retransmits everthing in the queue immediately
    // NOTE: outgoing_rels are handled in _try_retransmit. Sending duplicate pubrels
    // isn't a problem (I guess ?). Broker will just resend pubcomps
    fn _force_retransmit(&mut self) {
        if self.opts.clean_session {
            // We are anyway going to clear the queues if they aren't empty
            self.should_qos1_block = false;
            self.should_qos2_block = false;
            match self.state {
                MqttState::Connected => {
                    for index in 0..self.outgoing_pub.len() {
                        let message = self.outgoing_pub.remove(index).expect("No such entry");
                        let _ = self._publish(*message.1);
                    }

                    for index in 0..self.outgoing_rec.len() {
                        let message = self.outgoing_rec.remove(index).expect("No such entry");
                        let _ = self._publish(*message.1);
                    }
                }
                MqttState::Disconnected | MqttState::Handshake => {
                    error!("I won't force republish. Client isn't in connected state")
                }
            }
        }
    }

    fn ping(&mut self) -> Result<()> {
        let ping = try!(self._generate_pingreq_packet());
        self.await_ping = true;
        try!(self._write_packet(ping));
        self._flush()
    }

    fn _unbind(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        self.await_ping = false;
        self.state = MqttState::Disconnected;
        info!("  Disconnected {:?}", self.opts.client_id);
    }

    fn _subscribe(&mut self, topics: Vec<(TopicFilter, QualityOfService)>) -> Result<()> {
        let subscribe_packet = try!(self._generate_subscribe_packet(topics));
        try!(self._write_packet(subscribe_packet));
        self._flush()
    }

    fn _publish(&mut self, message: Message) -> Result<()> {

        let qos = message.qos;
        let message_box = message.transform(Some(qos));
        let topic = message.topic;
        let payload = &*message.payload;
        let retain = message.retain;

        let publish_packet = try!(self._generate_publish_packet(topic, qos, retain, payload.clone()));

        match message.qos {
            QoSWithPacketIdentifier::Level0 => (),
            QoSWithPacketIdentifier::Level1(_) => {
                self.outgoing_pub.push_back((time::get_time().sec, message_box.clone()));
                if self.outgoing_pub.len() >= self.opts.pub_q_len as usize {
                    self.should_qos1_block = true;
                }
            }
            QoSWithPacketIdentifier::Level2(_) => {
                self.outgoing_rec.push_back((time::get_time().sec, message_box.clone()));
                if self.outgoing_rec.len() >= self.opts.pub_q_len as usize {
                    self.should_qos2_block = true;
                }
            }
        }
        // debug!("       Publish {:?} {:?} > {} bytes", message.qos,
        // topic.clone().to_string(), message.payload.len());

        // TODO: print error for failure here
        try!(self._write_packet(publish_packet));
        self._flush()
    }

    fn _puback(&mut self, pkid: u16) -> Result<()> {
        let puback_packet = try!(self._generate_puback_packet(pkid));
        try!(self._write_packet(puback_packet));
        self._flush()
    }

    fn _pubrec(&mut self, pkid: u16) -> Result<()> {
        let pubrec_packet = try!(self._generate_pubrec_packet(pkid));
        try!(self._write_packet(pubrec_packet));
        self._flush()
    }

    fn _pubrel(&mut self, pkid: u16) -> Result<()> {
        let pubrel_packet = try!(self._generate_pubrel_packet(pkid));
        try!(self._write_packet(pubrel_packet));
        self._flush()
    }

    fn _pubcomp(&mut self, pkid: u16) -> Result<()> {
        let puback_packet = try!(self._generate_pubcomp_packet(pkid));
        try!(self._write_packet(puback_packet));
        self._flush()
    }

    fn _flush(&mut self) -> Result<()> {
        try!(self.stream.flush());
        self.last_flush = Instant::now();
        Ok(())
    }

    #[inline]
    fn _write_packet(&mut self, packet: Vec<u8>) -> Result<()> {
        // debug!("@@@ WRITING PACKET\n{:?}", packet);
        match self.stream.write_all(&packet) {
            Ok(v) => v,
            Err(e) => {
                error!("{:?}", e);
                // disconnect block in case of socket errors. verify
                // self.disconnect_block = true
                return Err(e.into());
            }
        };
        Ok(())
    }

    fn _generate_connect_packet(&self) -> Result<Vec<u8>> {
        let mut connect_packet = ConnectPacket::new("MQTT".to_owned(), self.opts.client_id.clone().unwrap());

        connect_packet.set_clean_session(self.opts.clean_session);

        if let Some(keep_alive) = self.opts.keep_alive {
            connect_packet.set_keep_alive(keep_alive);
        }

        // Converting (String, String) -> (TopicName, String)
        let will = match self.opts.will {
            Some(ref will) => Some((try!(TopicName::new(will.0.clone())), will.1.clone())),
            None => None,
        };

        if will.is_some() {
            connect_packet.set_will(will);
            connect_packet.set_will_qos(self.opts.will_qos as u8);
            connect_packet.set_will_retain(self.opts.will_retain);
        }

        // mqtt-protocol APIs are directly handling None cases.
        connect_packet.set_user_name(self.opts.username.clone());
        connect_packet.set_password(self.opts.password.clone());

        let mut buf = Vec::new();

        try!(connect_packet.encode(&mut buf));
        Ok(buf)
    }

    fn _generate_disconnect_packet(&self) -> Result<Vec<u8>> {
        let disconnect_packet = DisconnectPacket::new();
        let mut buf = Vec::new();

        try!(disconnect_packet.encode(&mut buf));
        Ok(buf)
    }

    fn _generate_pingreq_packet(&self) -> Result<Vec<u8>> {
        let pingreq_packet = PingreqPacket::new();
        let mut buf = Vec::new();

        try!(pingreq_packet.encode(&mut buf));
        Ok(buf)
    }

    fn _generate_subscribe_packet(&self, topics: Vec<(TopicFilter, QualityOfService)>) -> Result<Vec<u8>> {
        let subscribe_packet = SubscribePacket::new(11, topics);
        let mut buf = Vec::new();

        try!(subscribe_packet.encode(&mut buf));
        Ok(buf)
    }

    // TODO: dup flag
    fn _generate_publish_packet(&self,
                                topic: TopicName,
                                qos: QoSWithPacketIdentifier,
                                retain: bool,
                                payload: Vec<u8>)
                                -> Result<Vec<u8>> {
        let mut publish_packet = PublishPacket::new(topic, qos, payload);
        let mut buf = Vec::new();
        publish_packet.set_retain(retain);
        // publish_packet.set_dup(dup);
        try!(publish_packet.encode(&mut buf));
        Ok(buf)
    }

    fn _generate_puback_packet(&self, pkid: u16) -> Result<Vec<u8>> {
        let puback_packet = PubackPacket::new(pkid);
        let mut buf = Vec::new();

        try!(puback_packet.encode(&mut buf));
        Ok(buf)
    }

    fn _generate_pubrec_packet(&self, pkid: u16) -> Result<Vec<u8>> {
        let pubrec_packet = PubrecPacket::new(pkid);
        let mut buf = Vec::new();

        try!(pubrec_packet.encode(&mut buf));
        Ok(buf)
    }

    fn _generate_pubrel_packet(&self, pkid: u16) -> Result<Vec<u8>> {
        let pubrel_packet = PubrelPacket::new(pkid);
        let mut buf = Vec::new();

        try!(pubrel_packet.encode(&mut buf));
        Ok(buf)
    }

    fn _generate_pubcomp_packet(&self, pkid: u16) -> Result<Vec<u8>> {
        let pubcomp_packet = PubcompPacket::new(pkid);
        let mut buf = Vec::new();

        try!(pubcomp_packet.encode(&mut buf));
        Ok(buf)
    }

    // http://stackoverflow.
    // com/questions/11115364/mqtt-messageid-practical-implementation
    #[inline]
    fn _next_pkid(&mut self) -> PacketIdentifier {
        let PacketIdentifier(pkid) = self.last_pkid;
        self.last_pkid = PacketIdentifier(pkid + 1);
        self.last_pkid
    }
}

// Why RuMqtt:
// GOALS
// -----
// 1. Synchronous mqtt connects: No need of callback to check if mqtt
// connection is
// successful or not. You'll know of of errors (if any) synchronously
// 2. Synchronous subscribes (TODO): Same as above
// 3. Queued publishes: publishes won't throw errors by default. A queue (with
// user defined
// length) will be buffered when the n/w is down. If n/w is down for some time
// and queue
// becomes full, publishes are blocked
// 4. No locks. Fast and efficient because of Rust and Mio
// 5. Callback only for subscibed incoming message. Callbacks are executed
// using threadpool
//
