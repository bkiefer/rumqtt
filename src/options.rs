use std::time::Duration;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ReconnectOptions {
    Never,
    AfterFirstSuccess(Duration),
    Always(Duration),
}

#[derive(Clone)]
pub enum SecurityOptions {
    None,
    /// username, password
    UsernamePassword((String, String)),
    /// ca, client cert, client key
    Tls((String, String, String)),
    /// roots.pem, private_key.der to sign jwt, expiry in seconds
    GcloudIotCore((String, String, i64)),
}


// TODO: Add getters & make fields private

#[derive(Clone)]
pub struct MqttOptions {
    /// broker address that you want to connect to
    pub broker_addr: String,
    /// keep alive time to send pingreq to broker when the connection is idle
    pub keep_alive: Option<u16>,
    /// clean (or) persistent session
    pub clean_session: bool,
    /// client identifier
    pub client_id: String,
    /// time left for server to send a connection acknowlegment
    pub mqtt_connection_timeout: Duration,
    /// reconnection options
    pub reconnect: ReconnectOptions,
    /// security options
    pub security: SecurityOptions,
    /// maximum packet size
    pub max_packet_size: usize,
    /// mqtt will
    pub last_will: Option<::mqtt3::LastWill>,
}

impl MqttOptions {
    pub fn new<S1: Into<String>, S2: Into<String>>(id: S1, addr: S2) -> MqttOptions {
        // TODO: Validate client id. Shouldn't be empty or start with spaces
        // TODO: Validate if addr is proper address type
        MqttOptions {
            broker_addr: addr.into(),
            keep_alive: Some(10),
            clean_session: true,
            client_id: id.into(),
            mqtt_connection_timeout: Duration::from_secs(5),
            reconnect: ReconnectOptions::AfterFirstSuccess(Duration::from_secs(10)),
            security: SecurityOptions::None,
            max_packet_size: 100 * 1024,
            last_will: None,
        }
    }

    /// Set number of seconds after which client should ping the broker
    /// if there is no other data exchange
    pub fn set_keep_alive(mut self, secs: u16) -> Self {
        if secs < 5 {
            panic!("Keep alives should be greater than 5 secs");
        }

        self.keep_alive = Some(secs);
        self
    }

    /// Set packet size limit (in Kilo Bytes)
    pub fn set_max_packet_size(mut self, sz: usize) -> Self {
        self.max_packet_size = sz * 1024;
        self
    }

    /// `clean_session = true` removes all the state from queues & instructs the broker
    /// to clean all the client state when client disconnects.
    ///
    /// When set `false`, broker will hold the client state and performs pending
    /// operations on the client when reconnection with same `client_id`
    /// happens. Local queue state is also held to retransmit packets after reconnection.
    ///
    /// So **make sure that you manually set `client_id` when `clean_session` is false**
    pub fn set_clean_session(mut self, clean_session: bool) -> Self {
        self.clean_session = clean_session;
        self
    }

    /// Time interval after which client should retry for new
    /// connection if there are any disconnections. By default, no retry will happen
    pub fn set_reconnect_opts(mut self, opts: ReconnectOptions) -> Self {
        self.reconnect = opts;
        self
    }

    /// Set security option
    /// Supports username-password auth, tls client cert auth, gcloud iotcore jwt auth
    pub fn set_security_opts(mut self, opts: SecurityOptions) -> Self {
        self.security = opts;
        self
    }

    /// Set MQTT last will
    /// This message will be emit by the broker on disconnect.
    pub fn set_last_will(mut self, will: Option<::mqtt3::LastWill>) -> Self {
        self.last_will = will;
        self
    }
}
