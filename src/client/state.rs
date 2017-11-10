use std::time::{Duration, Instant};
use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::io::Read;

use jwt::{encode, Header, Algorithm};
use chrono::{self, Utc};

//use error::{PingError, ConnectError, PublishError, PubackError, SubscribeError};
use mqtt3;
use packet;
use MqttOptions;
use SecurityOptions;
use error::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MqttConnectionStatus {
    Handshake,
    Connected,
    Disconnected,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iat: i64,
    exp: i64,
    aud: String,
}

pub struct MqttState {
    opts: MqttOptions,

    // --------  State  ----------
    connection_status: MqttConnectionStatus,
    initial_connect: bool,
    await_pingresp: bool,
    last_flush: Instant,
    last_pkid: mqtt3::PacketIdentifier,

    // For QoS 1. Stores outgoing publishes
    outgoing_pub: VecDeque<mqtt3::Publish>,
    // clean_session=false will remember subscriptions only till lives.
    // Even so, if broker crashes, all its state will be lost (most brokers).
    // client should resubscribe it comes back up again or else the data will
    // be lost
    subscriptions: Vec<::Subscription>,
}

/// Design: `MqttState` methods will just modify the state of the object
///         but doesn't do any network operations. Methods will do
///         appropriate returns so that n/w methods or n/w eventloop can
///         operate directly. This abstracts the functionality better
///         so that it's easy to switch between synchronous code, tokio (or)
///         async/await

impl MqttState {
    pub fn new(opts: MqttOptions) -> Self {
        MqttState {
            opts: opts,
            connection_status: MqttConnectionStatus::Disconnected,
            initial_connect: true,
            await_pingresp: false,
            last_flush: Instant::now(),
            last_pkid: mqtt3::PacketIdentifier(0),
            outgoing_pub: VecDeque::new(),
            subscriptions: Vec::new(),
        }
    }

    pub fn initial_connect(&self) -> bool {
        self.initial_connect
    }

    pub fn handle_outgoing_connect(&mut self) -> mqtt3::Connect {
        let keep_alive = if let Some(keep_alive) = self.opts.keep_alive {
            keep_alive
        } else {
            // rumqtt sets keep alive time to 3 minutes if user sets it to none.
            // (get consensus)
            180
        };

        self.opts.keep_alive = Some(keep_alive);
        self.connection_status = MqttConnectionStatus::Handshake;

        let (username, password) = match self.opts.security {
            SecurityOptions::UsernamePassword((ref username, ref password)) => (Some(username.to_owned()), Some(password.to_owned())),
            SecurityOptions::GcloudIotCore((_, ref key, expiry)) => (Some("unused".to_owned()), Some(gen_iotcore_password(key, expiry))),
            _ => (None, None),
        };

        packet::gen_connect_packet(self.opts.client_id.clone(), keep_alive, self.opts.clean_session, username, password)
    }

    pub fn handle_incoming_connack(&mut self, connack: mqtt3::Connack) -> Result<()> {
        let response = connack.code;
        if response != mqtt3::ConnectReturnCode::Accepted {
            self.connection_status = MqttConnectionStatus::Disconnected;
            Err(format!("Connack error {:?}", response))?
        } else {
            self.connection_status = MqttConnectionStatus::Connected;
            self.initial_connect = false;
            if self.opts.clean_session {
                self.clear_session_info();
            }

            Ok(())
        }
    }

    pub fn handle_reconnection(&mut self) -> Option<VecDeque<mqtt3::Publish>> {
        if self.opts.clean_session {
            None
        } else {
            Some(self.outgoing_pub.clone())
        }
    }

    /// Sets next packet id if pkid is None (fresh publish) and adds it to the
    /// outgoing publish queue
    pub fn handle_outgoing_publish(&mut self, mut publish: mqtt3::Publish) -> Result<mqtt3::Publish> {
        let publish = match publish.qos {
            mqtt3::QoS::AtMostOnce => publish,
            mqtt3::QoS::AtLeastOnce => {
                // add pkid if None
                let publish = if publish.pid == None {
                    let pkid = self.next_pkid();
                    publish.pid = Some(pkid);
                    publish
                } else {
                    publish
                };

                self.outgoing_pub.push_back(publish.clone());
                publish
            }
            _ => unimplemented!()
        };

        if self.connection_status == MqttConnectionStatus::Connected {
            self.reset_last_control_at();
            Ok(publish)
        } else {
            Err(ErrorKind::InvalidState.into())
        }

    }

    pub fn handle_incoming_puback(&mut self, pkid: mqtt3::PacketIdentifier) -> Result<mqtt3::Publish> {
        if let Some(index) = self.outgoing_pub.iter().position(|x| x.pid == Some(pkid)) {
            Ok(self.outgoing_pub.remove(index).unwrap())
        } else {
            error!("Unsolicited PUBLISH packet: {:?}", pkid);
            Err(ErrorKind::InvalidState.into())
        }
    }

    // return a tuple. tuple.0 is supposed to be send to user through 'notify_tx' while tuple.1
    // should be sent back on network as ack
    pub fn handle_incoming_publish(&mut self, publish: mqtt3::Publish) -> (Option<mqtt3::Publish>, Option<mqtt3::Packet>) {
        let pkid = publish.pid;
        let qos = publish.qos;

        match qos {
            mqtt3::QoS::AtMostOnce => (Some(publish), None),
            mqtt3::QoS::AtLeastOnce => (Some(publish), Some(mqtt3::Packet::Puback(pkid.unwrap()))),
            mqtt3::QoS::ExactlyOnce => unimplemented!()
        }
    }

    // reset the last control packet received time
    pub fn reset_last_control_at(&mut self) {
        self.last_flush = Instant::now();
    }

    // check if pinging is required based on last flush time
    pub fn is_ping_required(&self) -> bool {
        if let Some(keep_alive) = self.opts.keep_alive  {
            let keep_alive = Duration::new(f32::ceil(0.9 * f32::from(keep_alive)) as u64, 0);
            self.last_flush.elapsed() > keep_alive
        } else {
            false
        }
    }

    // check when the last control packet/pingreq packet
    // is received and return the status which tells if
    // keep alive time has exceeded
    // NOTE: status will be checked for zero keepalive times also
    pub fn handle_outgoing_ping(&mut self) -> Result<()> {
        let keep_alive = self.opts.keep_alive.expect("No keep alive");

        let elapsed = self.last_flush.elapsed();
        if elapsed >= Duration::new(u64::from(keep_alive + 1), 0) {
            return Err(ErrorKind::InvalidState.into());
        }
        // @ Prevents half open connections. Tcp writes will buffer up
        // with out throwing any error (till a timeout) when internet
        // is down. Eventhough broker closes the socket after timeout,
        // EOF will be known only after reconnection.
        // We need to unbind the socket if there in no pingresp before next ping
        // (What about case when pings aren't sent because of constant publishes
        // ?. A. Tcp write buffer gets filled up and write will be blocked for 10
        // secs and then error out because of timeout.)
        if self.await_pingresp {
            return Err(ErrorKind::InvalidState.into());
        }

        if self.connection_status == MqttConnectionStatus::Connected {
            self.last_flush = Instant::now();
            self.await_pingresp = true;
            Ok(())
        } else {
            error!("State = {:?}. Shouldn't ping in this state", self.connection_status);
            Err(ErrorKind::InvalidState.into())
        }
    }

    pub fn handle_incoming_pingresp(&mut self) {
        self.await_pingresp = false;
    }

    pub fn handle_outgoing_subscribe(&mut self, topics: Vec<mqtt3::SubscribeTopic>) -> Result<mqtt3::Subscribe> {
        let pkid = self.next_pkid();

        if self.connection_status == MqttConnectionStatus::Connected {
            self.last_flush = Instant::now();
            self.await_pingresp = true;

            Ok(mqtt3::Subscribe {
                pid: pkid,
                topics: topics,
            })
        } else {
            error!("State = {:?}. Shouldn't subscribe in this state", self.connection_status);
            Err(ErrorKind::InvalidState.into())
        }
    }


    // pub fn handle_incoming_suback(&mut self, ack: Suback) -> Result<()> {
    //     if ack.return_codes.iter().any(|v| *v == SubscribeReturnCodes::Failure) {
    //         Err(SubackError::Rejected)
    //     } else {
    //         Ok(())
    //     }
    // }

    pub fn handle_disconnect(&mut self) {
        self.await_pingresp = false;
        self.connection_status = MqttConnectionStatus::Disconnected;

        // remove all the state
        if self.opts.clean_session {
            self.clear_session_info();
        }
    }

    fn clear_session_info(&mut self) {
        self.outgoing_pub.clear();
    }

    // http://stackoverflow.com/questions/11115364/mqtt-messageid-practical-implementation
    fn next_pkid(&mut self) -> mqtt3::PacketIdentifier {
        let mqtt3::PacketIdentifier(mut pkid) = self.last_pkid;
        if pkid == 65_535 {
            pkid = 0;
        }
        self.last_pkid = mqtt3::PacketIdentifier(pkid + 1);
        self.last_pkid
    }
}

// Generates a new password for mqtt client authentication
pub fn gen_iotcore_password<P>(key: P, expiry: i64) -> String
where P: AsRef<Path> {
    let time = Utc::now();
    let jwt_header = Header::new(Algorithm::RS256);
    let iat = time.timestamp();
    let exp = time.checked_add_signed(chrono::Duration::minutes(expiry)).unwrap().timestamp();
    let claims = Claims {
        iat: iat,
        exp: exp,
        aud: "crested-return-122311".to_string(),
    };

    let mut key_file = File::open(key).expect("Unable to open private keyfile for gcloud iot core auth");
    let mut key = vec![];
    key_file.read_to_end(&mut key).expect("Unable to read private key file for gcloud iot core auth till end");
    encode(&jwt_header, &claims, &key).expect("encode error")
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::{MqttState, MqttConnectionStatus};
    use mqtt3::*;
    use mqttopts::MqttOptions;
    use error::*;

    #[test]
    fn next_pkid_roll() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        let mut pkt_id = PacketIdentifier(0);
        for _ in 0..65536 {
            pkt_id = mqtt.next_pkid();
        }
        assert_eq!(PacketIdentifier(1), pkt_id);
    }

    #[test]
    fn outgoing_publish_handle_should_set_pkid_correctly_and_add_publish_to_queue_correctly() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.connection_status = MqttConnectionStatus::Connected;

        // QoS0 Publish
        let publish = Publish {
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let publish_out = mqtt.handle_outgoing_publish(publish);
        // pkid shouldn't be added
        assert_eq!(publish_out.unwrap().pid, None);
        // publish shouldn't added to queue
        assert_eq!(mqtt.outgoing_pub.len(), 0);
        

        // QoS1 Publish
        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let publish_out = mqtt.handle_outgoing_publish(publish.clone());
        // pkid shouldn't be added
        assert_eq!(publish_out.unwrap().pid, Some(PacketIdentifier(1)));
        // publish shouldn't added to queue
        assert_eq!(mqtt.outgoing_pub.len(), 1);

        let publish_out = mqtt.handle_outgoing_publish(publish.clone());
        // pkid shouldn't be added
        assert_eq!(publish_out.unwrap().pid, Some(PacketIdentifier(2)));
        // publish shouldn't added to queue
        assert_eq!(mqtt.outgoing_pub.len(), 2);
    }

    #[test]
    fn outgoing_publish_handle_should_throw_error_in_invalid_state() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));

        let publish = Publish {
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let publish_out = mqtt.handle_outgoing_publish(publish);
        let err = publish_out.unwrap_err();
        match err {
            Error(ErrorKind::InvalidState, _) => {}
            _ => panic!()
        }
    }

    #[test]
    #[ignore]
    fn outgoing_publish_handle_should_throw_error_when_packetsize_exceeds_max() {
        /*
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));

        let publish = Publish {
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![0; 101 * 1024]),
        };

        let publish_out = mqtt.handle_outgoing_publish(publish);
        assert_eq!(publish_out, Err(PublishError::PacketSizeLimitExceeded));
        */
    }
/*
    #[test]
    fn incoming_puback_should_remove_correct_publish_from_queue() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        // QoS1 Publish
        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let publish_out = mqtt.handle_outgoing_publish(publish.clone());
        let publish_out = mqtt.handle_outgoing_publish(publish.clone());
        let publish_out = mqtt.handle_outgoing_publish(publish);

        let publish = mqtt.handle_incoming_puback(PacketIdentifier(1)).unwrap();
        assert_eq!(publish.pid, Some(PacketIdentifier(1)));
        assert_eq!(mqtt.outgoing_pub.len(), 2);

        let publish = mqtt.handle_incoming_puback(PacketIdentifier(2)).unwrap();
        assert_eq!(publish.pid, Some(PacketIdentifier(2)));
        assert_eq!(mqtt.outgoing_pub.len(), 1);

        let publish = mqtt.handle_incoming_puback(PacketIdentifier(3)).unwrap();
        assert_eq!(publish.pid, Some(PacketIdentifier(3)));
        assert_eq!(mqtt.outgoing_pub.len(), 0);
    }

    #[test]
    fn outgoing_ping_handle_should_throw_errors_during_invalid_state() {
        // 1. test for invalid state
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.opts.keep_alive = Some(5);
        thread::sleep(Duration::new(5, 0));
        assert_eq!(Err(ErrorKind::InvalidState.into()), mqtt.handle_outgoing_ping());
    }

    #[test]
    fn outgoing_ping_handle_should_throw_errors_for_no_pingresp() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.opts.keep_alive = Some(5);
        mqtt.connection_status = MqttConnectionStatus::Connected;
        thread::sleep(Duration::new(5, 0));
        // should ping
        assert_eq!(Ok(()), mqtt.handle_outgoing_ping());
        thread::sleep(Duration::new(5, 0));
        // should throw error because we didn't get pingresp for previous ping
        assert_eq!(Err(ErrorKind::InvalidState), mqtt.handle_outgoing_ping());
    }

    #[test]
    fn outgoing_ping_handle_should_throw_error_if_ping_time_exceeded() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.opts.keep_alive = Some(5);
        mqtt.connection_status = MqttConnectionStatus::Connected;
        thread::sleep(Duration::new(7, 0));
        // should ping
        assert_eq!(Err(ErrorKind::InvalidState.into()), mqtt.handle_outgoing_ping());
    }

    #[test]
    fn outgoing_ping_handle_should_succeed_if_pingresp_is_received() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.opts.keep_alive = Some(5);
        mqtt.connection_status = MqttConnectionStatus::Connected;
        thread::sleep(Duration::new(5, 0));
        // should ping
        assert_eq!(Ok(()), mqtt.handle_outgoing_ping());
        mqtt.handle_incoming_pingresp();
        thread::sleep(Duration::new(5, 0));
        // should ping
        assert_eq!(Ok(()), mqtt.handle_outgoing_ping());
    }
*/
    #[test]
    fn disconnect_handle_should_reset_everything_in_clean_session() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.await_pingresp = true;
        // QoS1 Publish
        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish);

        mqtt.handle_disconnect();
        assert_eq!(mqtt.outgoing_pub.len(), 0);
        assert_eq!(mqtt.connection_status, MqttConnectionStatus::Disconnected);
        assert_eq!(mqtt.await_pingresp, false);
    }

    #[test]
    fn disconnect_handle_should_reset_everything_except_queues_in_persistent_session() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.await_pingresp = true;
        mqtt.opts.clean_session = false;
        // QoS1 Publish
        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish);

        mqtt.handle_disconnect();
        assert_eq!(mqtt.outgoing_pub.len(), 3);
        assert_eq!(mqtt.connection_status, MqttConnectionStatus::Disconnected);
        assert_eq!(mqtt.await_pingresp, false);
    }

    #[test]
    fn connection_status_is_valid_while_handling_connect_and_connack_packets() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));

        assert_eq!(mqtt.connection_status, MqttConnectionStatus::Disconnected);
        mqtt.handle_outgoing_connect();
        assert_eq!(mqtt.connection_status, MqttConnectionStatus::Handshake);

        let connack = Connack {
            session_present: false,
            code: ConnectReturnCode::Accepted
        };

        mqtt.handle_incoming_connack(connack);
        assert_eq!(mqtt.connection_status, MqttConnectionStatus::Connected);

        let connack = Connack {
            session_present: false,
            code: ConnectReturnCode::BadUsernamePassword
        };

        mqtt.handle_incoming_connack(connack);
        assert_eq!(mqtt.connection_status, MqttConnectionStatus::Disconnected);
    }

    #[test]
    fn connack_handle_should_not_return_list_of_incomplete_messages_to_be_sent_in_clean_session() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));

        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish);

        let connack = Connack {
            session_present: false,
            code: ConnectReturnCode::Accepted
        };

        mqtt.handle_incoming_connack(connack).unwrap();
        assert_eq!(None, mqtt.handle_reconnection());
    }

    #[test]
    fn connack_handle_should_return_list_of_incomplete_messages_to_be_sent_in_persistent_session() {
        let mut mqtt = MqttState::new(MqttOptions::new("test-id", "127.0.0.1:1883"));
        mqtt.opts.clean_session = false;

        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            pid: None,
            topic_name: "hello/world".to_owned(),
            payload: Arc::new(vec![1, 2, 3]),
        };

        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish.clone());
        let _ = mqtt.handle_outgoing_publish(publish);

        let connack = Connack {
            session_present: false,
            code: ConnectReturnCode::Accepted
        };

        if let Ok(_) = mqtt.handle_incoming_connack(connack) {
            if let Some(v) = mqtt.handle_reconnection() {
                assert_eq!(v.len(), 3);
            } else {
                panic!("Should return publishes to be retransmitted");
            }
        }
    }
}
