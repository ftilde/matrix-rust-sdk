// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(all(feature = "encryption", not(target_arch = "wasm32")))]
use std::path::PathBuf;
use std::{
    collections::BTreeMap,
    fmt::{self, Debug},
    future::Future,
    io::Read,
    path::Path,
    pin::Pin,
    result::Result as StdResult,
    sync::Arc,
};
#[cfg(feature = "sso_login")]
use std::{
    collections::HashMap,
    io::{Error as IoError, ErrorKind as IoErrorKind},
    ops::Range,
};
#[cfg(feature = "encryption")]
use std::{
    collections::HashSet,
    io::{Cursor, Write},
};

use dashmap::DashMap;
use futures::FutureExt;
use futures_timer::Delay as sleep;
use http::HeaderValue;
#[cfg(feature = "sso_login")]
use http::Response;
#[cfg(all(feature = "encryption", not(target_arch = "wasm32")))]
use matrix_sdk_base::crypto::{decrypt_key_export, encrypt_key_export, olm::InboundGroupSession};
#[cfg(feature = "encryption")]
use matrix_sdk_base::crypto::{
    store::CryptoStoreError, AttachmentDecryptor, OutgoingRequests, RoomMessageRequest,
    ToDeviceRequest,
};
#[cfg(feature = "encryption")]
use matrix_sdk_base::{crypto::CrossSigningStatus, deserialized_responses::RoomEvent};
use matrix_sdk_base::{
    deserialized_responses::{JoinedRoom, LeftRoom, SyncResponse},
    media::{MediaEventContent, MediaFormat, MediaRequest, MediaThumbnailSize, MediaType},
    BaseClient, BaseClientConfig, Session, Store,
};
use mime::{self, Mime};
#[cfg(feature = "sso_login")]
use rand::{thread_rng, Rng};
use reqwest::header::InvalidHeaderValue;
#[cfg(feature = "encryption")]
use ruma::events::{AnyMessageEvent, AnyRoomEvent, AnySyncMessageEvent, EventType};
use ruma::{
    api::{client::r0::push::get_notifications::Notification, SendAccessToken},
    events::AnyMessageEventContent,
    MxcUri,
};
use serde::de::DeserializeOwned;
#[cfg(feature = "sso_login")]
use tokio::{net::TcpListener, sync::oneshot};
#[cfg(feature = "sso_login")]
use tokio_stream::wrappers::TcpListenerStream;
#[cfg(feature = "encryption")]
use tracing::{debug, trace};
use tracing::{error, info, instrument, warn};
use url::Url;
#[cfg(feature = "sso_login")]
use warp::Filter;
#[cfg(all(feature = "encryption", not(target_arch = "wasm32")))]
use zeroize::Zeroizing;

/// Enum controlling if a loop running callbacks should continue or abort.
///
/// This is mainly used in the [`sync_with_callback`] method, the return value
/// of the provided callback controls if the sync loop should be exited.
///
/// [`sync_with_callback`]: #method.sync_with_callback
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopCtrl {
    /// Continue running the loop.
    Continue,
    /// Break out of the loop.
    Break,
}

use matrix_sdk_common::{
    instant::{Duration, Instant},
    locks::{Mutex, RwLock},
    uuid::Uuid,
};
#[cfg(feature = "encryption")]
use ruma::{
    api::client::r0::{
        keys::{get_keys, upload_keys, upload_signing_keys::Request as UploadSigningKeysRequest},
        to_device::send_event_to_device::{
            Request as RumaToDeviceRequest, Response as ToDeviceResponse,
        },
    },
    DeviceId,
};
use ruma::{
    api::{
        client::{
            r0::{
                account::{register, whoami},
                device::{delete_devices, get_devices},
                directory::{get_public_rooms, get_public_rooms_filtered},
                filter::{create_filter::Request as FilterUploadRequest, FilterDefinition},
                media::{create_content, get_content, get_content_thumbnail},
                membership::{join_room_by_id, join_room_by_id_or_alias},
                message::send_message_event,
                profile::{get_avatar_url, get_display_name, set_avatar_url, set_display_name},
                room::create_room,
                session::{get_login_types, login, sso_login},
                sync::sync_events,
                uiaa::{AuthData, UserIdentifier},
            },
            unversioned::{discover_homeserver, get_supported_versions},
        },
        error::FromHttpResponseError,
        OutgoingRequest,
    },
    assign,
    presence::PresenceState,
    DeviceIdBox, RoomId, RoomIdOrAliasId, ServerName, UInt, UserId,
};

#[cfg(feature = "encryption")]
use crate::{
    error::RoomKeyImportError,
    identities::{Device, UserDevices},
    verification::{QrVerification, SasVerification, Verification, VerificationRequest},
};
use crate::{
    error::{HttpError, HttpResult},
    event_handler::{EventHandler, EventHandlerData, EventHandlerResult, EventKind, SyncEvent},
    http_client::{client_with_config, HttpClient, HttpSend},
    room, Error, Result,
};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);
/// A conservative upload speed of 1Mbps
const DEFAULT_UPLOAD_SPEED: u64 = 125_000;
/// 5 min minimal upload request timeout, used to clamp the request timeout.
const MIN_UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 5);
/// The range of ports the SSO server will try to bind to randomly
#[cfg(feature = "sso_login")]
const SSO_SERVER_BIND_RANGE: Range<u16> = 20000..30000;
/// The number of times the SSO server will try to bind to a random port
#[cfg(feature = "sso_login")]
const SSO_SERVER_BIND_TRIES: u8 = 10;

type EventHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
type EventHandlerFn = Box<dyn Fn(EventHandlerData<'_>) -> EventHandlerFut + Send + Sync>;
type EventHandlerMap = BTreeMap<(EventKind, &'static str), Vec<EventHandlerFn>>;

type NotificationHandlerFut = EventHandlerFut;
type NotificationHandlerFn =
    Box<dyn Fn(Notification, room::Room, Client) -> NotificationHandlerFut + Send + Sync>;

/// An async/await enabled Matrix client.
///
/// All of the state is held in an `Arc` so the `Client` can be cloned freely.
#[derive(Clone)]
pub struct Client {
    /// The URL of the homeserver to connect to.
    homeserver: Arc<RwLock<Url>>,
    /// The underlying HTTP client.
    http_client: HttpClient,
    /// User session data.
    pub(crate) base_client: BaseClient,
    /// Locks making sure we only have one group session sharing request in
    /// flight per room.
    #[cfg(feature = "encryption")]
    pub(crate) group_session_locks: Arc<DashMap<RoomId, Arc<Mutex<()>>>>,
    #[cfg(feature = "encryption")]
    /// Lock making sure we're only doing one key claim request at a time.
    key_claim_lock: Arc<Mutex<()>>,
    pub(crate) members_request_locks: Arc<DashMap<RoomId, Arc<Mutex<()>>>>,
    pub(crate) typing_notice_times: Arc<DashMap<RoomId, Instant>>,
    /// Event handlers. See `register_event_handler`.
    pub(crate) event_handlers: Arc<RwLock<EventHandlerMap>>,
    /// Notification handlers. See `register_notification_handler`.
    notification_handlers: Arc<RwLock<Vec<NotificationHandlerFn>>>,
    /// Whether the client should operate in application service style mode.
    /// This is low-level functionality. For an high-level API check the
    /// `matrix_sdk_appservice` crate.
    appservice_mode: bool,
    /// An event that can be listened on to wait for a successful sync. The
    /// event will only be fired if a sync loop is running. Can be used for
    /// synchronization, e.g. if we send out a request to create a room, we can
    /// wait for the sync to get the data to fetch a room object from the state
    /// store.
    sync_beat: Arc<event_listener::Event>,
}

#[cfg(not(tarpaulin_include))]
impl Debug for Client {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> StdResult<(), fmt::Error> {
        write!(fmt, "Client")
    }
}

/// Configuration for the creation of the `Client`.
///
/// When setting the `StateStore` it is up to the user to open/connect
/// the storage backend before client creation.
///
/// # Example
///
/// ```
/// # use matrix_sdk::ClientConfig;
/// // To pass all the request through mitmproxy set the proxy and disable SSL
/// // verification
/// let client_config = ClientConfig::new()
///     .proxy("http://localhost:8080")
///     .unwrap()
///     .disable_ssl_verification();
/// ```
#[derive(Default)]
pub struct ClientConfig {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) proxy: Option<reqwest::Proxy>,
    pub(crate) user_agent: Option<HeaderValue>,
    pub(crate) disable_ssl_verification: bool,
    pub(crate) base_config: BaseClientConfig,
    pub(crate) request_config: RequestConfig,
    pub(crate) client: Option<Arc<dyn HttpSend>>,
    pub(crate) appservice_mode: bool,
}

#[cfg(not(tarpaulin_include))]
impl Debug for ClientConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut res = fmt.debug_struct("ClientConfig");

        #[cfg(not(target_arch = "wasm32"))]
        let res = res.field("proxy", &self.proxy);

        res.field("user_agent", &self.user_agent)
            .field("disable_ssl_verification", &self.disable_ssl_verification)
            .field("request_config", &self.request_config)
            .finish()
    }
}

impl ClientConfig {
    /// Create a new default `ClientConfig`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the proxy through which all the HTTP requests should go.
    ///
    /// Note, only HTTP proxies are supported.
    ///
    /// # Arguments
    ///
    /// * `proxy` - The HTTP URL of the proxy.
    ///
    /// # Example
    ///
    /// ```
    /// use matrix_sdk::ClientConfig;
    ///
    /// let client_config = ClientConfig::new()
    ///     .proxy("http://localhost:8080")
    ///     .unwrap();
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub fn proxy(mut self, proxy: &str) -> Result<Self> {
        self.proxy = Some(reqwest::Proxy::all(proxy)?);
        Ok(self)
    }

    /// Disable SSL verification for the HTTP requests.
    pub fn disable_ssl_verification(mut self) -> Self {
        self.disable_ssl_verification = true;
        self
    }

    /// Set a custom HTTP user agent for the client.
    pub fn user_agent(mut self, user_agent: &str) -> StdResult<Self, InvalidHeaderValue> {
        self.user_agent = Some(HeaderValue::from_str(user_agent)?);
        Ok(self)
    }

    ///// Set a custom implementation of a `StateStore`.
    /////
    ///// The state store should be opened before being set.
    //pub fn state_store(mut self, store: Box<dyn StateStore>) -> Self {
    //    self.base_config = self.base_config.state_store(store);
    //    self
    //}

    /// Set the path for storage.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the stores should save data in. It is the
    /// callers responsibility to make sure that the path exists.
    ///
    /// In the default configuration the client will open default
    /// implementations for the crypto store and the state store. It will use
    /// the given path to open the stores. If no path is provided no store will
    /// be opened
    pub fn store_path(mut self, path: impl AsRef<Path>) -> Self {
        self.base_config = self.base_config.store_path(path);
        self
    }

    /// Set the passphrase to encrypt the crypto store.
    ///
    /// # Argument
    ///
    /// * `passphrase` - The passphrase that will be used to encrypt the data in
    /// the cryptostore.
    ///
    /// This is only used if no custom cryptostore is set.
    pub fn passphrase(mut self, passphrase: String) -> Self {
        self.base_config = self.base_config.passphrase(passphrase);
        self
    }

    /// Set the default timeout, fail and retry behavior for all HTTP requests.
    pub fn request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = request_config;
        self
    }

    /// Get the [`RequestConfig`]
    pub fn get_request_config(&self) -> &RequestConfig {
        &self.request_config
    }

    /// Specify a client to handle sending requests and receiving responses.
    ///
    /// Any type that implements the `HttpSend` trait can be used to
    /// send/receive `http` types.
    pub fn client(mut self, client: Arc<dyn HttpSend>) -> Self {
        self.client = Some(client);
        self
    }

    /// Puts the client into application service mode
    ///
    /// This is low-level functionality. For an high-level API check the
    /// `matrix_sdk_appservice` crate.
    #[cfg(feature = "appservice")]
    pub fn appservice_mode(mut self) -> Self {
        self.appservice_mode = true;
        self
    }
}

#[derive(Debug, Clone)]
/// Settings for a sync call.
pub struct SyncSettings<'a> {
    pub(crate) filter: Option<sync_events::Filter<'a>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) token: Option<String>,
    pub(crate) full_state: bool,
}

impl<'a> Default for SyncSettings<'a> {
    fn default() -> Self {
        Self {
            filter: Default::default(),
            timeout: Some(DEFAULT_SYNC_TIMEOUT),
            token: Default::default(),
            full_state: Default::default(),
        }
    }
}

impl<'a> SyncSettings<'a> {
    /// Create new default sync settings.
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the sync token.
    ///
    /// # Arguments
    ///
    /// * `token` - The sync token that should be used for the sync call.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Set the maximum time the server can wait, in milliseconds, before
    /// responding to the sync request.
    ///
    /// # Arguments
    ///
    /// * `timeout` - The time the server is allowed to wait.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set the sync filter.
    /// It can be either the filter ID, or the definition for the filter.
    ///
    /// # Arguments
    ///
    /// * `filter` - The filter configuration that should be used for the sync
    ///   call.
    pub fn filter(mut self, filter: sync_events::Filter<'a>) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Should the server return the full state from the start of the timeline.
    ///
    /// This does nothing if no sync token is set.
    ///
    /// # Arguments
    /// * `full_state` - A boolean deciding if the server should return the full
    ///   state or not.
    pub fn full_state(mut self, full_state: bool) -> Self {
        self.full_state = full_state;
        self
    }
}

/// Configuration for requests the `Client` makes.
///
/// This sets how often and for how long a request should be repeated. As well
/// as how long a successful request is allowed to take.
///
/// By default requests are retried indefinitely and use no timeout.
///
/// # Example
///
/// ```
/// # use matrix_sdk::RequestConfig;
/// # use std::time::Duration;
/// // This sets makes requests fail after a single send request and sets the timeout to 30s
/// let request_config = RequestConfig::new()
///     .disable_retry()
///     .timeout(Duration::from_secs(30));
/// ```
#[derive(Copy, Clone)]
pub struct RequestConfig {
    pub(crate) timeout: Duration,
    pub(crate) retry_limit: Option<u64>,
    pub(crate) retry_timeout: Option<Duration>,
    pub(crate) force_auth: bool,
    pub(crate) assert_identity: bool,
}

#[cfg(not(tarpaulin_include))]
impl Debug for RequestConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut res = fmt.debug_struct("RequestConfig");

        res.field("timeout", &self.timeout)
            .field("retry_limit", &self.retry_limit)
            .field("retry_timeout", &self.retry_timeout)
            .finish()
    }
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_REQUEST_TIMEOUT,
            retry_limit: Default::default(),
            retry_timeout: Default::default(),
            force_auth: false,
            assert_identity: false,
        }
    }
}

impl RequestConfig {
    /// Create a new default `RequestConfig`.
    pub fn new() -> Self {
        Default::default()
    }

    /// This is a convince method to disable the retries of a request. Setting
    /// the `retry_limit` to `0` has the same effect.
    pub fn disable_retry(mut self) -> Self {
        self.retry_limit = Some(0);
        self
    }

    /// The number of times a request should be retried. The default is no limit
    pub fn retry_limit(mut self, retry_limit: u64) -> Self {
        self.retry_limit = Some(retry_limit);
        self
    }

    /// Set the timeout duration for all HTTP requests.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set a timeout for how long a request should be retried. The default is
    /// no timeout, meaning requests are retried forever.
    pub fn retry_timeout(mut self, retry_timeout: Duration) -> Self {
        self.retry_timeout = Some(retry_timeout);
        self
    }

    /// Force sending authorization even if the endpoint does not require it.
    /// Default is only sending authorization if it is required.
    pub(crate) fn force_auth(mut self) -> Self {
        self.force_auth = true;
        self
    }

    /// All outgoing http requests will have a GET query key-value appended with
    /// `user_id` being the key and the `user_id` from the `Session` being
    /// the value. Will error if there's no `Session`. This is called
    /// [identity assertion] in the Matrix Application Service Spec
    ///
    /// [identity assertion]: https://spec.matrix.org/unstable/application-service-api/#identity-assertion
    #[cfg(feature = "appservice")]
    #[cfg_attr(feature = "docs", doc(cfg(appservice)))]
    pub fn assert_identity(mut self) -> Self {
        self.assert_identity = true;
        self
    }
}

impl Client {
    /// Creates a new client for making HTTP requests to the given homeserver.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    pub fn new(homeserver_url: Url) -> Result<Self> {
        let config = ClientConfig::new();
        Client::new_with_config(homeserver_url, config)
    }

    /// Create a new client with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The homeserver that the client should connect to.
    ///
    /// * `config` - Configuration for the client.
    pub fn new_with_config(homeserver_url: Url, config: ClientConfig) -> Result<Self> {
        let homeserver = Arc::new(RwLock::new(homeserver_url));

        let client = if let Some(client) = config.client {
            client
        } else {
            Arc::new(client_with_config(&config)?)
        };

        let base_client = BaseClient::new_with_config(config.base_config)?;
        let session = base_client.session().clone();

        let http_client =
            HttpClient::new(client, homeserver.clone(), session, config.request_config);

        Ok(Self {
            homeserver,
            http_client,
            base_client,
            #[cfg(feature = "encryption")]
            group_session_locks: Default::default(),
            #[cfg(feature = "encryption")]
            key_claim_lock: Default::default(),
            members_request_locks: Default::default(),
            typing_notice_times: Default::default(),
            event_handlers: Default::default(),
            notification_handlers: Default::default(),
            appservice_mode: config.appservice_mode,
            sync_beat: event_listener::Event::new().into(),
        })
    }

    /// Creates a new client for making HTTP requests to the homeserver of the
    /// given user. Follows homeserver discovery directions described
    /// [here](https://spec.matrix.org/unstable/client-server-api/#well-known-uri).
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the user whose homeserver the client should
    ///   connect to.
    ///
    /// # Example
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use futures::executor::block_on;
    /// let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # block_on(async {
    /// let client = Client::new_from_user_id(alice.clone()).await.unwrap();
    /// client.login(alice.localpart(), "password", None, None).await.unwrap();
    /// # });
    /// ```
    pub async fn new_from_user_id(user_id: UserId) -> Result<Self> {
        let config = ClientConfig::new();
        Client::new_from_user_id_with_config(user_id, config).await
    }

    /// Creates a new client for making HTTP requests to the homeserver of the
    /// given user and configuration. Follows homeserver discovery directions
    /// described [here](https://spec.matrix.org/unstable/client-server-api/#well-known-uri).
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the user whose homeserver the client should
    ///   connect to.
    ///
    /// * `config` - Configuration for the client.
    pub async fn new_from_user_id_with_config(
        user_id: UserId,
        config: ClientConfig,
    ) -> Result<Self> {
        let homeserver = Client::homeserver_from_user_id(user_id)?;
        let mut client = Client::new_with_config(homeserver, config)?;

        let well_known = client.discover_homeserver().await?;
        let well_known = Url::parse(well_known.homeserver.base_url.as_ref())?;
        client.set_homeserver(well_known).await;
        client.get_supported_versions().await?;
        Ok(client)
    }

    fn homeserver_from_user_id(user_id: UserId) -> Result<Url> {
        let homeserver = format!("https://{}", user_id.server_name());
        #[allow(unused_mut)]
        let mut result = Url::parse(homeserver.as_str())?;
        // Mockito only knows how to test http endpoints:
        // https://github.com/lipanski/mockito/issues/127
        #[cfg(test)]
        let _ = result.set_scheme("http");
        Ok(result)
    }

    async fn discover_homeserver(&self) -> HttpResult<discover_homeserver::Response> {
        self.send(discover_homeserver::Request::new(), Some(RequestConfig::new().disable_retry()))
            .await
    }

    /// Change the homeserver URL used by this client.
    ///
    /// # Arguments
    ///
    /// * `homeserver_url` - The new URL to use.
    pub async fn set_homeserver(&mut self, homeserver_url: Url) {
        let mut homeserver = self.homeserver.write().await;
        *homeserver = homeserver_url;
    }

    async fn get_supported_versions(&self) -> HttpResult<get_supported_versions::Response> {
        self.send(
            get_supported_versions::Request::new(),
            Some(RequestConfig::new().disable_retry()),
        )
        .await
    }

    /// Process a [transaction] received from the homeserver
    ///
    /// # Arguments
    ///
    /// * `incoming_transaction` - The incoming transaction received from the
    ///   homeserver.
    ///
    /// [transaction]: https://matrix.org/docs/spec/application_service/r0.1.2#put-matrix-app-v1-transactions-txnid
    #[cfg(feature = "appservice")]
    #[cfg_attr(feature = "docs", doc(cfg(appservice)))]
    pub async fn receive_transaction(
        &self,
        incoming_transaction: ruma::api::appservice::event::push_events::v1::IncomingRequest,
    ) -> Result<()> {
        let txn_id = incoming_transaction.txn_id.clone();
        let response = incoming_transaction.try_into_sync_response(txn_id)?;
        self.process_sync(response).await?;

        Ok(())
    }

    /// Is the client logged in.
    pub async fn logged_in(&self) -> bool {
        self.base_client.logged_in().await
    }

    /// The Homeserver of the client.
    pub async fn homeserver(&self) -> Url {
        self.homeserver.read().await.clone()
    }

    /// Get the user id of the current owner of the client.
    pub async fn user_id(&self) -> Option<UserId> {
        let session = self.base_client.session().read().await;
        session.as_ref().cloned().map(|s| s.user_id)
    }

    /// Get the device id that identifies the current session.
    pub async fn device_id(&self) -> Option<DeviceIdBox> {
        let session = self.base_client.session().read().await;
        session.as_ref().map(|s| s.device_id.clone())
    }

    /// Get the public ed25519 key of our own device. This is usually what is
    /// called the fingerprint of the device.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn ed25519_key(&self) -> Option<String> {
        self.base_client.olm_machine().await.map(|o| o.identity_keys().ed25519().to_owned())
    }

    /// Get all the tracked users we know about
    ///
    /// Tracked users are users for which we keep the device list of E2EE
    /// capable devices up to date.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn tracked_users(&self) -> HashSet<UserId> {
        self.base_client.olm_machine().await.map(|o| o.tracked_users()).unwrap_or_default()
    }

    /// Fetches the display name of the owner of the client.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// let user = "example";
    /// let client = Client::new(homeserver).unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// if let Some(name) = client.display_name().await.unwrap() {
    ///     println!("Logged in as user '{}' with display name '{}'", user, name);
    /// }
    /// # })
    /// ```
    pub async fn display_name(&self) -> Result<Option<String>> {
        let user_id = self.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = get_display_name::Request::new(&user_id);
        let response = self.send(request, None).await?;
        Ok(response.displayname)
    }

    /// Sets the display name of the owner of the client.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// let user = "example";
    /// let client = Client::new(homeserver).unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// client.set_display_name(Some("Alice")).await.expect("Failed setting display name");
    /// # })
    /// ```
    pub async fn set_display_name(&self, name: Option<&str>) -> Result<()> {
        let user_id = self.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = set_display_name::Request::new(&user_id, name);
        self.send(request, None).await?;
        Ok(())
    }

    /// Gets the mxc avatar url of the owner of the client, if set.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// if let Some(url) = client.avatar_url().await.unwrap() {
    ///     println!("Your avatar's mxc url is {}", url);
    /// }
    /// # })
    /// ```
    pub async fn avatar_url(&self) -> Result<Option<MxcUri>> {
        let user_id = self.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = get_avatar_url::Request::new(&user_id);

        #[cfg(not(feature = "require_auth_for_profile_requests"))]
        let config = None;

        #[cfg(feature = "require_auth_for_profile_requests")]
        let config = Some(RequestConfig::new().force_auth());

        let response = self.send(request, config).await?;
        Ok(response.avatar_url)
    }

    /// Gets the avatar of the owner of the client, if set.
    ///
    /// Returns the avatar.
    /// If a thumbnail is requested no guarantee on the size of the image is
    /// given.
    ///
    /// # Arguments
    ///
    /// * `format` - The desired format of the avatar.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::media::MediaFormat;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// if let Some(avatar) = client.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        if let Some(url) = self.avatar_url().await? {
            let request = MediaRequest { media_type: MediaType::Uri(url), format };
            Ok(Some(self.get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }

    /// Get a reference to the store.
    pub fn store(&self) -> &Store {
        self.base_client.store()
    }

    /// Sets the mxc avatar url of the client's owner. The avatar gets unset if
    /// `url` is `None`.
    pub async fn set_avatar_url(&self, url: Option<&MxcUri>) -> Result<()> {
        let user_id = self.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = set_avatar_url::Request::new(&user_id, url);
        self.send(request, None).await?;
        Ok(())
    }

    /// Upload and set the owning client's avatar.
    ///
    /// The will upload the data produced by the reader to the homeserver's
    /// content repository, and set the user's avatar to the mxc url for the
    /// uploaded file.
    ///
    /// This is a convenience method for calling [`upload()`](#method.upload),
    /// followed by [`set_avatar_url()`](#method.set_avatar_url).
    ///
    /// # Example
    /// ```no_run
    /// # use std::{path::Path, fs::File, io::Read};
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// let path = Path::new("/home/example/selfie.jpg");
    /// let mut image = File::open(&path).unwrap();
    ///
    /// client.upload_avatar(&mime::IMAGE_JPEG, &mut image).await.expect("Can't set avatar");
    /// # })
    /// ```
    pub async fn upload_avatar<R: Read>(&self, content_type: &Mime, reader: &mut R) -> Result<()> {
        let upload_response = self.upload(content_type, reader).await?;
        self.set_avatar_url(Some(&upload_response.content_uri)).await?;
        Ok(())
    }

    /// Register a handler for a specific event type.
    ///
    /// The handler is a function or closure with one or more arguments. The
    /// first argument is the event itself. All additional arguments are
    /// "context" arguments: They have to implement [`EventHandlerContext`].
    /// This trait is named that way because most of the types implementing it
    /// give additional context about an event: The room it was in, its raw form
    /// and other similar things. As an exception to this,
    /// [`Client`] also implements the `EventHandlerContext` trait
    /// so you don't have to clone your client into the event handler manually.
    ///
    /// Some context arguments are not universally applicable. A context
    /// argument that isn't available for the given event type will result in
    /// the event handler being skipped and an error being logged. The following
    /// context argument types are only available for a subset of event types:
    ///
    /// * [`Room`][room::Room] is only available for room-specific events, i.e.
    ///   not for events like global account data events or presence events
    ///
    /// [`EventHandlerContext`]: crate::event_handler::EventHandlerContext
    ///
    /// # Examples
    ///
    /// ```
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// use matrix_sdk::{
    ///     room::Room,
    ///     ruma::{
    ///         events::{
    ///             macros::EventContent,
    ///             push_rules::PushRulesEvent,
    ///             room::{message::MessageEventContent, topic::TopicEventContent},
    ///             SyncMessageEvent, SyncStateEvent,
    ///         },
    ///         Int, MilliSecondsSinceUnixEpoch,
    ///     },
    ///     Client,
    /// };
    /// use serde::{Deserialize, Serialize};
    ///
    /// # let _ = async {
    /// client
    ///     .register_event_handler(
    ///         |ev: SyncMessageEvent<MessageEventContent>,
    ///          room: Room,
    ///          client: Client| async move {
    ///             // Common usage: Room event plus room and client.
    ///         },
    ///     )
    ///     .await
    ///     .register_event_handler(
    ///         |ev: SyncStateEvent<TopicEventContent>| async move {
    ///             // Also possible: Omit any or all arguments after the first.
    ///         }
    ///     ).await;
    ///
    /// // Custom events work exactly the same way, you just need to declare
    /// // the content struct and use the EventContent derive macro on it.
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.shiny_new_2fa.token", kind = Message)]
    /// struct TokenEventContent {
    ///     token: String,
    ///     #[serde(rename = "exp")]
    ///     expires_at: MilliSecondsSinceUnixEpoch,
    /// }
    ///
    /// client.register_event_handler(
    ///     |ev: SyncMessageEvent<TokenEventContent>, room: Room| async move {
    ///         todo!("Display the token");
    ///     },
    /// ).await;
    ///
    /// // Adding your custom data to the handler can be done as well
    /// let data = "MyCustomIdentifier".to_string();
    ///
    /// client.register_event_handler({
    ///     let data = data.clone();
    ///     move |ev: SyncMessageEvent<MessageEventContent> | {
    ///         let data = data.clone();
    ///         async move {
    ///             println!("Calling the handler with identifier {}", data);
    ///         }
    ///     }
    /// }).await;
    /// # };
    /// ```
    pub async fn register_event_handler<Ev, Ctx, H>(&self, handler: H) -> &Self
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
        <H::Future as Future>::Output: EventHandlerResult,
    {
        let event_type = H::ID.1;
        self.event_handlers.write().await.entry(H::ID).or_default().push(Box::new(move |data| {
            let maybe_fut = serde_json::from_str(data.raw.get())
                .map(|ev| handler.clone().handle_event(ev, data));

            async move {
                match maybe_fut {
                    Ok(Some(fut)) => {
                        fut.await.print_error(event_type);
                    }
                    Ok(None) => {
                        error!("Event handler for {} has an invalid context argument", event_type);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize `{}` event, skipping event handler.\n\
                                 Deserialization error: {}",
                            event_type, e,
                        );
                    }
                }
            }
            .boxed()
        }));

        self
    }

    /// Register a handler for a notification.
    ///
    /// Similar to `.register_event_handler`, but only allows functions or
    /// closures with exactly the three arguments `Notification`, `room::Room`,
    /// `Client` for now.
    pub async fn register_notification_handler<H, Fut>(&self, handler: H) -> &Self
    where
        H: Fn(Notification, room::Room, Client) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.notification_handlers.write().await.push(Box::new(
            move |notification, room, client| (handler)(notification, room, client).boxed(),
        ));

        self
    }

    /// Get all the rooms the client knows about.
    ///
    /// This will return the list of joined, invited, and left rooms.
    pub fn rooms(&self) -> Vec<room::Room> {
        self.store()
            .get_rooms()
            .into_iter()
            .map(|room| room::Common::new(self.clone(), room).into())
            .collect()
    }

    /// Returns the joined rooms this client knows about.
    pub fn joined_rooms(&self) -> Vec<room::Joined> {
        self.store()
            .get_rooms()
            .into_iter()
            .filter_map(|room| room::Joined::new(self.clone(), room))
            .collect()
    }

    /// Returns the invited rooms this client knows about.
    pub fn invited_rooms(&self) -> Vec<room::Invited> {
        self.store()
            .get_rooms()
            .into_iter()
            .filter_map(|room| room::Invited::new(self.clone(), room))
            .collect()
    }

    /// Returns the left rooms this client knows about.
    pub fn left_rooms(&self) -> Vec<room::Left> {
        self.store()
            .get_rooms()
            .into_iter()
            .filter_map(|room| room::Left::new(self.clone(), room))
            .collect()
    }

    /// Get a room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_room(&self, room_id: &RoomId) -> Option<room::Room> {
        self.store().get_room(room_id).map(|room| room::Common::new(self.clone(), room).into())
    }

    /// Get a joined room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_joined_room(&self, room_id: &RoomId) -> Option<room::Joined> {
        self.store().get_room(room_id).and_then(|room| room::Joined::new(self.clone(), room))
    }

    /// Get an invited room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_invited_room(&self, room_id: &RoomId) -> Option<room::Invited> {
        self.store().get_room(room_id).and_then(|room| room::Invited::new(self.clone(), room))
    }

    /// Get a left room with the given room id.
    ///
    /// # Arguments
    ///
    /// `room_id` - The unique id of the room that should be fetched.
    pub fn get_left_room(&self, room_id: &RoomId) -> Option<room::Left> {
        self.store().get_room(room_id).and_then(|room| room::Left::new(self.clone(), room))
    }

    /// Tries to decrypt a `AnyRoomEvent`. Returns unencrypted room event when
    /// decryption fails.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub(crate) async fn decrypt_room_event(&self, event: &AnyRoomEvent) -> RoomEvent {
        if let Some(machine) = self.base_client.olm_machine().await {
            if let AnyRoomEvent::Message(event) = event {
                if let AnyMessageEvent::RoomEncrypted(_) = event {
                    let room_id = event.room_id();
                    // Turn the AnyMessageEvent into a AnySyncMessageEvent
                    let event = event.clone().into();

                    if let AnySyncMessageEvent::RoomEncrypted(e) = event {
                        if let Ok(decrypted) = machine.decrypt_room_event(&e, room_id).await {
                            let event = decrypted
                                .event
                                .deserialize()
                                .unwrap()
                                .into_full_event(room_id.clone())
                                .into();
                            let encryption_info = decrypted.encryption_info;

                            // Return decrytped room event
                            return RoomEvent { event, encryption_info };
                        }
                    }
                }
            }
        }

        // Fallback to unencrypted room event
        RoomEvent { event: event.into(), encryption_info: None }
    }

    /// Gets the homeserver’s supported login types.
    ///
    /// This should be the first step when trying to login so you can call the
    /// appropriate method for the next step.
    pub async fn get_login_types(&self) -> HttpResult<get_login_types::Response> {
        let request = get_login_types::Request::new();
        self.send(request, None).await
    }

    /// Get the URL to use to login via Single Sign-On.
    ///
    /// Returns a URL that should be opened in a web browser to let the user
    /// login.
    ///
    /// After a successful login, the loginToken received at the redirect URL
    /// should be used to login with [`login_with_token`].
    ///
    /// # Arguments
    ///
    /// * `redirect_url` - The URL that will receive a `loginToken` after a
    ///   successful SSO login.
    ///
    /// [`login_with_token`]: #method.login_with_token
    pub async fn get_sso_login_url(&self, redirect_url: &str) -> Result<String> {
        let homeserver = self.homeserver().await;
        let request = sso_login::Request::new(redirect_url)
            .try_into_http_request::<Vec<u8>>(homeserver.as_str(), SendAccessToken::None);
        match request {
            Ok(req) => Ok(req.uri().to_string()),
            Err(err) => Err(Error::from(HttpError::from(err))),
        }
    }

    /// Login to the server.
    ///
    /// This can be used for the first login as well as for subsequent logins,
    /// note that if the device id isn't provided a new device will be created.
    ///
    /// If this isn't the first login a device id should be provided to restore
    /// the correct stores.
    ///
    /// Alternatively the [`restore_login`] method can be used to restore a
    /// logged in client without the password.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that should be logged in to the homeserver.
    ///
    /// * `password` - The password of the user.
    ///
    /// * `device_id` - A unique id that will be associated with this session.
    ///   If not given the homeserver will create one. Can be an existing
    ///   device_id from a previous login call. Note that this should be done
    ///   only if the client also holds the encryption keys for this device.
    ///
    /// # Example
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{assign, DeviceId};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// let client = Client::new(homeserver).unwrap();
    /// let user = "example";
    /// let response = client
    ///     .login(user, "wordpass", None, Some("My bot")).await
    ///     .unwrap();
    ///
    /// println!("Logged in as {}, got device_id {} and access_token {}",
    ///          user, response.device_id, response.access_token);
    /// # })
    /// ```
    ///
    /// [`restore_login`]: #method.restore_login
    #[instrument(skip(password))]
    pub async fn login(
        &self,
        user: &str,
        password: &str,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
    ) -> Result<login::Response> {
        info!("Logging in to {} as {:?}", self.homeserver().await, user);

        let login_info =
            login::LoginInfo::Password { identifier: UserIdentifier::MatrixId(user), password };
        let request = assign!(login::Request::new(login_info), {
            device_id: device_id.map(|d| d.into()),
            initial_device_display_name,
        });

        let response = self.send(request, None).await?;
        self.base_client.receive_login_response(&response).await?;

        Ok(response)
    }

    /// Login to the server via Single Sign-On.
    ///
    /// This takes care of the whole SSO flow:
    ///   * Spawn a local http server
    ///   * Provide a callback to open the SSO login URL in a web browser
    ///   * Wait for the local http server to get the loginToken
    ///   * Call [`login_with_token`]
    ///
    /// If cancellation is needed the method should be wrapped in a cancellable
    /// task. **Note** that users with root access to the system have the
    /// ability to snoop in on the data/token that is passed to the local
    /// HTTP server that will be spawned.
    ///
    /// If you need more control over the SSO login process, you should use
    /// [`get_sso_login_url`] and [`login_with_token`] directly.
    ///
    /// This should only be used for the first login.
    ///
    /// The [`restore_login`] method should be used to restore a
    /// logged in client after the first login.
    ///
    /// A device id should be provided to restore the correct stores, if the
    /// device id isn't provided a new device will be created.
    ///
    /// # Arguments
    ///
    /// * `use_sso_login_url` - A callback that will receive the SSO Login URL.
    ///   It should usually be used to open the SSO URL in a browser and must
    ///   return `Ok(())` if the URL was successfully opened. If it returns
    ///   `Err`, the error will be forwarded.
    ///
    /// * `server_url` - The local URL the server is going to try to bind to, e.g. `http://localhost:3030`.
    ///   If `None`, the server will try to open a random port on localhost.
    ///
    /// * `server_response` - The text that will be shown on the webpage at the
    ///   end of the login process. This can be an HTML page. If `None`, a
    ///   default text will be displayed.
    ///
    /// * `device_id` - A unique id that will be associated with this session.
    ///   If not given the homeserver will create one. Can be an existing
    ///   device_id from a previous login call. Note that this should be
    ///   provided only if the client also holds the encryption keys for this
    ///   device.
    ///
    /// * `initial_device_display_name` - A public display name that will be
    ///   associated with the device_id. Only necessary the first time you login
    ///   with this device_id. It can be changed later.
    ///
    /// # Example
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # block_on(async {
    /// let client = Client::new(homeserver).unwrap();
    ///
    /// let response = client
    ///     .login_with_sso(
    ///         |sso_url| async move {
    ///             // Open sso_url
    ///             Ok(())
    ///         },
    ///         None,
    ///         None,
    ///         None,
    ///         Some("My app")
    ///     )
    ///     .await
    ///     .unwrap();
    ///
    /// println!("Logged in as {}, got device_id {} and access_token {}",
    ///          response.user_id, response.device_id, response.access_token);
    /// # })
    /// ```
    ///
    /// [`get_sso_login_url`]: #method.get_sso_login_url
    /// [`login_with_token`]: #method.login_with_token
    /// [`restore_login`]: #method.restore_login
    #[cfg(all(feature = "sso_login", not(target_arch = "wasm32")))]
    #[cfg_attr(feature = "docs", doc(cfg(all(sso_login, not(target_arch = "wasm32")))))]
    pub async fn login_with_sso<C>(
        &self,
        use_sso_login_url: impl Fn(String) -> C,
        server_url: Option<&str>,
        server_response: Option<&str>,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
    ) -> Result<login::Response>
    where
        C: Future<Output = Result<()>>,
    {
        info!("Logging in to {}", self.homeserver().await);
        let (signal_tx, signal_rx) = oneshot::channel();
        let (data_tx, data_rx) = oneshot::channel();
        let data_tx_mutex = Arc::new(std::sync::Mutex::new(Some(data_tx)));

        let mut redirect_url = match server_url {
            Some(s) => match Url::parse(s) {
                Ok(url) => url,
                Err(err) => return Err(IoError::new(IoErrorKind::InvalidData, err).into()),
            },
            None => Url::parse("http://localhost:0/").unwrap(),
        };

        let response = match server_response {
            Some(s) => s.to_string(),
            None => String::from(
                "The Single Sign-On login process is complete. You can close this page now.",
            ),
        };

        let route = warp::get().and(warp::query::<HashMap<String, String>>()).map(
            move |p: HashMap<String, String>| {
                if let Some(data_tx) = data_tx_mutex.lock().unwrap().take() {
                    if let Some(token) = p.get("loginToken") {
                        data_tx.send(Some(token.to_owned())).unwrap();
                    } else {
                        data_tx.send(None).unwrap();
                    }
                }
                Response::builder().body(response.clone())
            },
        );

        let listener = {
            if redirect_url.port().expect("The redirect URL doesn't include a port") == 0 {
                let host = redirect_url.host_str().expect("The redirect URL doesn't have a host");
                let mut n = 0u8;
                let mut port = 0u16;
                let mut res = Err(IoError::new(IoErrorKind::Other, ""));
                let mut rng = thread_rng();

                while res.is_err() && n < SSO_SERVER_BIND_TRIES {
                    port = rng.gen_range(SSO_SERVER_BIND_RANGE);
                    res = TcpListener::bind((host, port)).await;
                    n += 1;
                }
                match res {
                    Ok(s) => {
                        redirect_url
                            .set_port(Some(port))
                            .expect("Could not set new port on redirect URL");
                        s
                    }
                    Err(err) => return Err(err.into()),
                }
            } else {
                match TcpListener::bind(redirect_url.as_str()).await {
                    Ok(s) => s,
                    Err(err) => return Err(err.into()),
                }
            }
        };

        let server = warp::serve(route).serve_incoming_with_graceful_shutdown(
            TcpListenerStream::new(listener),
            async {
                signal_rx.await.ok();
            },
        );

        tokio::spawn(server);

        let sso_url = self.get_sso_login_url(redirect_url.as_str()).await.unwrap();

        match use_sso_login_url(sso_url).await {
            Ok(t) => t,
            Err(err) => return Err(err),
        };

        let token = match data_rx.await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Err(IoError::new(IoErrorKind::Other, "Could not get the loginToken").into())
            }
            Err(err) => return Err(IoError::new(IoErrorKind::Other, format!("{}", err)).into()),
        };

        let _ = signal_tx.send(());

        self.login_with_token(token.as_str(), device_id, initial_device_display_name).await
    }

    /// Login to the server with a token.
    ///
    /// This token is usually received in the SSO flow after following the URL
    /// provided by [`get_sso_login_url`], note that this is not the access
    /// token of a session.
    ///
    /// This should only be used for the first login.
    ///
    /// The [`restore_login`] method should be used to restore a
    /// logged in client after the first login.
    ///
    /// A device id should be provided to restore the correct stores, if the
    /// device id isn't provided a new device will be created.
    ///
    /// # Arguments
    ///
    /// * `token` - A login token.
    ///
    /// * `device_id` - A unique id that will be associated with this session.
    ///   If not given the homeserver will create one. Can be an existing
    ///   device_id from a previous login call. Note that this should be
    ///   provided only if the client also holds the encryption keys for this
    ///   device.
    ///
    /// * `initial_device_display_name` - A public display name that will be
    ///   associated with the device_id. Only necessary the first time you login
    ///   with this device_id. It can be changed later.
    ///
    /// # Example
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{assign, DeviceId};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("https://example.com").unwrap();
    /// # let redirect_url = "http://localhost:1234";
    /// # let login_token = "token";
    /// # block_on(async {
    /// let client = Client::new(homeserver).unwrap();
    /// let sso_url = client.get_sso_login_url(redirect_url);
    ///
    /// // Let the user authenticate at the SSO URL
    /// // Receive the loginToken param at redirect_url
    ///
    /// let response = client
    ///     .login_with_token(login_token, None, Some("My app")).await
    ///     .unwrap();
    ///
    /// println!("Logged in as {}, got device_id {} and access_token {}",
    ///          response.user_id, response.device_id, response.access_token);
    /// # })
    /// ```
    ///
    /// [`get_sso_login_url`]: #method.get_sso_login_url
    /// [`restore_login`]: #method.restore_login
    #[instrument(skip(token))]
    pub async fn login_with_token(
        &self,
        token: &str,
        device_id: Option<&str>,
        initial_device_display_name: Option<&str>,
    ) -> Result<login::Response> {
        info!("Logging in to {}", self.homeserver().await);

        let request = assign!(
            login::Request::new(
                login::LoginInfo::Token { token },
            ), {
                device_id: device_id.map(|d| d.into()),
                initial_device_display_name,
            }
        );

        let response = self.send(request, None).await?;
        self.base_client.receive_login_response(&response).await?;

        Ok(response)
    }

    /// Restore a previously logged in session.
    ///
    /// This can be used to restore the client to a logged in state, loading all
    /// the stored state and encryption keys.
    ///
    /// Alternatively, if the whole session isn't stored the [`login`] method
    /// can be used with a device id.
    ///
    /// # Arguments
    ///
    /// * `session` - A session that the user already has from a
    /// previous login call.
    ///
    /// [`login`]: #method.login
    pub async fn restore_login(&self, session: Session) -> Result<()> {
        Ok(self.base_client.restore_login(session).await?)
    }

    /// Register a user to the server.
    ///
    /// # Arguments
    ///
    /// * `registration` - The easiest way to create this request is using the
    ///   `register::Request`
    /// itself.
    ///
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{
    /// #     api::client::r0::{
    /// #         account::register::{Request as RegistrationRequest, RegistrationKind},
    /// #         uiaa,
    /// #     },
    /// #     assign, DeviceId,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    ///
    /// let request = assign!(RegistrationRequest::new(), {
    ///     username: Some("user"),
    ///     password: Some("password"),
    ///     auth: Some(uiaa::AuthData::FallbackAcknowledgement(
    ///         uiaa::FallbackAcknowledgement::new("foobar"),
    ///     )),
    /// });
    /// let client = Client::new(homeserver).unwrap();
    /// client.register(request).await;
    /// # })
    /// ```
    #[instrument(skip(registration))]
    pub async fn register(
        &self,
        registration: impl Into<register::Request<'_>>,
    ) -> HttpResult<register::Response> {
        info!("Registering to {}", self.homeserver().await);

        let config = if self.appservice_mode {
            Some(self.http_client.request_config.force_auth())
        } else {
            None
        };

        let request = registration.into();
        self.send(request, config).await
    }

    /// Get or upload a sync filter.
    ///
    /// This method will either get a filter ID from the store or upload the
    /// filter definition to the homeserver and return the new filter ID.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The unique name of the filter, this name will be used
    /// locally to store and identify the filter ID returned by the server.
    ///
    /// * `definition` - The filter definition that should be uploaded to the
    /// server if no filter ID can be found in the store.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #    Client, SyncSettings,
    /// #    ruma::api::client::r0::{
    /// #        filter::{
    /// #           FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter,
    /// #        },
    /// #        sync::sync_events::Filter,
    /// #    }
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let mut filter = FilterDefinition::default();
    /// let mut room_filter = RoomFilter::default();
    /// let mut event_filter = RoomEventFilter::default();
    ///
    /// // Let's enable member lazy loading.
    /// event_filter.lazy_load_options = LazyLoadOptions::Enabled {
    ///     include_redundant_members: false,
    /// };
    /// room_filter.state = event_filter;
    /// filter.room = room_filter;
    ///
    /// let filter_id = client
    ///     .get_or_upload_filter("sync", filter)
    ///     .await
    ///     .unwrap();
    ///
    /// let sync_settings = SyncSettings::new()
    ///     .filter(Filter::FilterId(&filter_id));
    ///
    /// let response = client.sync_once(sync_settings).await.unwrap();
    /// # });
    pub async fn get_or_upload_filter(
        &self,
        filter_name: &str,
        definition: FilterDefinition<'_>,
    ) -> Result<String> {
        if let Some(filter) = self.base_client.get_filter(filter_name).await? {
            Ok(filter)
        } else {
            let user_id = self.user_id().await.ok_or(Error::AuthenticationRequired)?;
            let request = FilterUploadRequest::new(&user_id, definition);
            let response = self.send(request, None).await?;

            self.base_client.receive_filter_upload(filter_name, &response).await?;

            Ok(response.filter_id)
        }
    }

    /// Join a room by `RoomId`.
    ///
    /// Returns a `join_room_by_id::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to be joined.
    pub async fn join_room_by_id(&self, room_id: &RoomId) -> HttpResult<join_room_by_id::Response> {
        let request = join_room_by_id::Request::new(room_id);
        self.send(request, None).await
    }

    /// Join a room by `RoomId`.
    ///
    /// Returns a `join_room_by_id_or_alias::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * `alias` - The `RoomId` or `RoomAliasId` of the room to be joined.
    /// An alias looks like `#name:example.com`.
    pub async fn join_room_by_id_or_alias(
        &self,
        alias: &RoomIdOrAliasId,
        server_names: &[Box<ServerName>],
    ) -> HttpResult<join_room_by_id_or_alias::Response> {
        let request = assign!(join_room_by_id_or_alias::Request::new(alias), {
            server_name: server_names,
        });
        self.send(request, None).await
    }

    /// Search the homeserver's directory of public rooms.
    ///
    /// Sends a request to "_matrix/client/r0/publicRooms", returns
    /// a `get_public_rooms::Response`.
    ///
    /// # Arguments
    ///
    /// * `limit` - The number of `PublicRoomsChunk`s in each response.
    ///
    /// * `since` - Pagination token from a previous request.
    ///
    /// * `server` - The name of the server, if `None` the requested server is
    ///   used.
    ///
    /// # Examples
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use std::convert::TryInto;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let limit = Some(10);
    /// # let since = Some("since token");
    /// # let server = Some("servername.com".try_into().unwrap());
    ///
    /// let mut client = Client::new(homeserver).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    ///
    /// client.public_rooms(limit, since, server).await;
    /// # });
    /// ```
    pub async fn public_rooms(
        &self,
        limit: Option<u32>,
        since: Option<&str>,
        server: Option<&ServerName>,
    ) -> HttpResult<get_public_rooms::Response> {
        let limit = limit.map(UInt::from);

        let request = assign!(get_public_rooms::Request::new(), {
            limit,
            since,
            server,
        });
        self.send(request, None).await
    }

    /// Create a room using the `RoomBuilder` and send the request.
    ///
    /// Sends a request to `/_matrix/client/r0/createRoom`, returns a
    /// `create_room::Response`, this is an empty response.
    ///
    /// # Arguments
    ///
    /// * `room` - The easiest way to create this request is using the
    /// `create_room::Request` itself.
    ///
    /// # Examples
    /// ```no_run
    /// use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::api::client::r0::room::{
    /// #     create_room::Request as CreateRoomRequest,
    /// #     Visibility,
    /// # };
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let request = CreateRoomRequest::new();
    /// let client = Client::new(homeserver).unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(client.create_room(request).await.is_ok());
    /// # });
    /// ```
    pub async fn create_room(
        &self,
        room: impl Into<create_room::Request<'_>>,
    ) -> HttpResult<create_room::Response> {
        let request = room.into();
        self.send(request, None).await
    }

    /// Search the homeserver's directory of public rooms with a filter.
    ///
    /// Sends a request to "_matrix/client/r0/publicRooms", returns
    /// a `get_public_rooms_filtered::Response`.
    ///
    /// # Arguments
    ///
    /// * `room_search` - The easiest way to create this request is using the
    /// `get_public_rooms_filtered::Request` itself.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{
    /// #     api::client::r0::directory::get_public_rooms_filtered::Request as PublicRoomsFilterRequest,
    /// #     directory::{Filter, RoomNetwork},
    /// #     assign,
    /// # };
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// let mut client = Client::new(homeserver).unwrap();
    ///
    /// let generic_search_term = Some("matrix-rust-sdk");
    /// let filter = assign!(Filter::new(), { generic_search_term });
    /// let request = assign!(PublicRoomsFilterRequest::new(), { filter });
    ///
    /// client.public_rooms_filtered(request).await;
    /// # })
    /// ```
    pub async fn public_rooms_filtered(
        &self,
        room_search: impl Into<get_public_rooms_filtered::Request<'_>>,
    ) -> HttpResult<get_public_rooms_filtered::Response> {
        let request = room_search.into();
        self.send(request, None).await
    }

    #[cfg(feature = "encryption")]
    pub(crate) async fn room_send_helper(
        &self,
        request: &RoomMessageRequest,
    ) -> Result<send_message_event::Response> {
        let content = request.content.clone();
        let txn_id = request.txn_id;
        let room_id = &request.room_id;

        self.get_joined_room(room_id)
            .expect("Can't send a message to a room that isn't known to the store")
            .send(content, Some(txn_id))
            .await
    }

    /// Upload some media to the server.
    ///
    /// # Arguments
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, fs::File, io::Read};
    /// # use matrix_sdk::{Client, ruma::room_id};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use mime;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// let path = PathBuf::from("/home/example/my-cat.jpg");
    /// let mut image = File::open(path).unwrap();
    ///
    /// let response = client
    ///     .upload(&mime::IMAGE_JPEG, &mut image)
    ///     .await
    ///     .expect("Can't upload my cat.");
    ///
    /// println!("Cat URI: {}", response.content_uri);
    /// # });
    /// ```
    pub async fn upload(
        &self,
        content_type: &Mime,
        reader: &mut impl Read,
    ) -> Result<create_content::Response> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        let timeout = std::cmp::max(
            Duration::from_secs(data.len() as u64 / DEFAULT_UPLOAD_SPEED),
            MIN_UPLOAD_REQUEST_TIMEOUT,
        );

        let request = assign!(create_content::Request::new(&data), {
            content_type: Some(content_type.essence_str()),
        });

        let request_config = self.http_client.request_config.timeout(timeout);
        Ok(self.http_client.upload(request, Some(request_config)).await?)
    }

    /// Send a room message to a room.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the room message if this room is encrypted.
    ///
    /// **Note**: This method will send an unencrypted message if the room
    /// cannot be found in the store, prefer the higher level
    /// [send()](room::Joined::send()) method that can be found for the
    /// [Joined](room::Joined) room struct to avoid this.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room.
    ///
    /// * `content` - The content of the message event.
    ///
    /// * `txn_id` - A unique `Uuid` that can be attached to a `MessageEvent`
    /// held in its unsigned field as `transaction_id`. If not given one is
    /// created for the message.
    ///
    /// # Example
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::ruma::room_id;
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::ruma::events::{
    ///     AnyMessageEventContent,
    ///     room::message::{MessageEventContent, TextMessageEventContent},
    /// };
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// # let room_id = room_id!("!test:localhost");
    /// use matrix_sdk_common::uuid::Uuid;
    ///
    /// let content = AnyMessageEventContent::RoomMessage(
    ///     MessageEventContent::text_plain("Hello world")
    /// );
    ///
    /// let txn_id = Uuid::new_v4();
    /// client.room_send(&room_id, content, Some(txn_id)).await.unwrap();
    /// # })
    /// ```
    pub async fn room_send(
        &self,
        room_id: &RoomId,
        content: impl Into<AnyMessageEventContent>,
        txn_id: Option<Uuid>,
    ) -> Result<send_message_event::Response> {
        if let Some(room) = self.get_joined_room(room_id) {
            room.send(content, txn_id).await
        } else {
            let content = content.into();
            let txn_id = txn_id.unwrap_or_else(Uuid::new_v4).to_string();
            let request = send_message_event::Request::new(room_id, &txn_id, &content);

            Ok(self.send(request, None).await?)
        }
    }

    /// Send an arbitrary request to the server, without updating client state.
    ///
    /// **Warning:** Because this method *does not* update the client state, it
    /// is important to make sure than you account for this yourself, and
    /// use wrapper methods where available.  This method should *only* be
    /// used if a wrapper method for the endpoint you'd like to use is not
    /// available.
    ///
    /// # Arguments
    ///
    /// * `request` - A filled out and valid request for the endpoint to be hit
    ///
    /// * `timeout` - An optional request timeout setting, this overrides the
    /// default request setting if one was set.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # use std::convert::TryFrom;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// use matrix_sdk::ruma::{api::client::r0::profile, user_id};
    ///
    /// // First construct the request you want to make
    /// // See https://docs.rs/ruma-client-api/latest/ruma_client_api/index.html
    /// // for all available Endpoints
    /// let user_id = user_id!("@example:localhost");
    /// let request = profile::get_profile::Request::new(&user_id);
    ///
    /// // Start the request using Client::send()
    /// let response = client.send(request, None).await.unwrap();
    ///
    /// // Check the corresponding Response struct to find out what types are
    /// // returned
    /// # })
    /// ```
    pub async fn send<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
    ) -> HttpResult<Request::IncomingResponse>
    where
        Request: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        Ok(self.http_client.send(request, config).await?)
    }

    #[cfg(feature = "encryption")]
    pub(crate) async fn send_to_device(
        &self,
        request: &ToDeviceRequest,
    ) -> HttpResult<ToDeviceResponse> {
        let txn_id_string = request.txn_id_string();

        let request = RumaToDeviceRequest::new_raw(
            request.event_type.as_str(),
            &txn_id_string,
            request.messages.clone(),
        );

        self.send(request, None).await
    }

    /// Get information of all our own devices.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, SyncSettings};
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # use std::convert::TryFrom;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// let response = client.devices().await.expect("Can't get devices from server");
    ///
    /// for device in response.devices {
    ///     println!(
    ///         "Device: {} {}",
    ///         device.device_id,
    ///         device.display_name.as_deref().unwrap_or("")
    ///     );
    /// }
    /// # });
    /// ```
    pub async fn devices(&self) -> HttpResult<get_devices::Response> {
        let request = get_devices::Request::new();

        self.send(request, None).await
    }

    /// Delete the given devices from the server.
    ///
    /// # Arguments
    ///
    /// * `devices` - The list of devices that should be deleted from the
    /// server.
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    /// request needs to set this to `None` and will always fail with an
    /// `UiaaResponse`. The response will contain information for the
    /// interactive auth and the same request needs to be made but this time
    /// with some `auth_data` provided.
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #    ruma::{
    /// #        api::{
    /// #            client::r0::uiaa,
    /// #            error::{FromHttpResponseError, ServerError},
    /// #        },
    /// #        assign,
    /// #    },
    /// #    Client, Error, SyncSettings,
    /// # };
    /// # use futures::executor::block_on;
    /// # use serde_json::json;
    /// # use url::Url;
    /// # use std::{collections::BTreeMap, convert::TryFrom};
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// let devices = &["DEVICEID".into()];
    ///
    /// if let Err(e) = client.delete_devices(devices, None).await {
    ///     if let Some(info) = e.uiaa_response() {
    ///         let auth_data = uiaa::AuthData::Password(assign!(
    ///             uiaa::Password::new(uiaa::UserIdentifier::MatrixId("example"), "wordpass"),
    ///             { session: info.session.as_deref() }
    ///         ));
    ///
    ///         client
    ///             .delete_devices(devices, Some(auth_data))
    ///             .await
    ///             .expect("Can't delete devices");
    ///     }
    /// }
    /// # });
    pub async fn delete_devices(
        &self,
        devices: &[DeviceIdBox],
        auth_data: Option<AuthData<'_>>,
    ) -> HttpResult<delete_devices::Response> {
        let mut request = delete_devices::Request::new(devices);
        request.auth = auth_data;

        self.send(request, None).await
    }

    /// Synchronize the client's state with the latest state on the server.
    ///
    /// **Note**: You should not use this method to repeatedly sync if
    /// encryption support is enabled, the [`sync`] method will make
    /// additional requests between syncs that are needed for E2E encryption
    /// to work.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call.
    ///
    /// [`sync`]: #method.sync
    #[instrument]
    pub async fn sync_once(&self, sync_settings: SyncSettings<'_>) -> Result<SyncResponse> {
        let request = assign!(sync_events::Request::new(), {
            filter: sync_settings.filter.as_ref(),
            since: sync_settings.token.as_deref(),
            full_state: sync_settings.full_state,
            set_presence: &PresenceState::Online,
            timeout: sync_settings.timeout,
        });

        let request_config = self.http_client.request_config.timeout(
            sync_settings.timeout.unwrap_or_else(|| Duration::from_secs(0))
                + self.http_client.request_config.timeout,
        );

        let response = self.send(request, Some(request_config)).await?;
        self.process_sync(response).await
    }

    async fn process_sync(&self, response: sync_events::Response) -> Result<SyncResponse> {
        let response = self.base_client.receive_sync_response(response).await?;
        let SyncResponse {
            next_batch: _,
            rooms,
            presence,
            account_data,
            to_device: _,
            device_lists: _,
            device_one_time_keys_count: _,
            ambiguity_changes: _,
            notifications,
        } = &response;

        self.handle_sync_events(EventKind::GlobalAccountData, &None, &account_data.events).await?;
        self.handle_sync_events(EventKind::Presence, &None, &presence.events).await?;

        for (room_id, room_info) in &rooms.join {
            let room = self.get_room(room_id);
            if room.is_none() {
                error!("Can't call event handler, room {} not found", room_id);
                continue;
            }

            let JoinedRoom { unread_notifications: _, timeline, state, account_data, ephemeral } =
                room_info;

            self.handle_sync_events(EventKind::EphemeralRoomData, &room, &ephemeral.events).await?;
            self.handle_sync_events(EventKind::RoomAccountData, &room, &account_data.events)
                .await?;
            self.handle_sync_state_events(&room, &state.events).await?;
            self.handle_sync_timeline_events(&room, &timeline.events).await?;
        }

        for (room_id, room_info) in &rooms.leave {
            let room = self.get_room(room_id);
            if room.is_none() {
                error!("Can't call event handler, room {} not found", room_id);
                continue;
            }

            let LeftRoom { timeline, state, account_data } = room_info;

            self.handle_sync_events(EventKind::RoomAccountData, &room, &account_data.events)
                .await?;
            self.handle_sync_state_events(&room, &state.events).await?;
            self.handle_sync_timeline_events(&room, &timeline.events).await?;
        }

        for (room_id, room_info) in &rooms.invite {
            let room = self.get_room(room_id);
            if room.is_none() {
                error!("Can't call event handler, room {} not found", room_id);
                continue;
            }

            // FIXME: Destructure room_info
            self.handle_sync_events(EventKind::InitialState, &room, &room_info.invite_state.events)
                .await?;
        }

        for handler in &*self.notification_handlers.read().await {
            for (room_id, room_notifications) in notifications {
                let room = match self.get_room(room_id) {
                    Some(room) => room,
                    None => {
                        warn!("Can't call notification handler, room {} not found", room_id);
                        continue;
                    }
                };

                for notification in room_notifications {
                    matrix_sdk_common::executor::spawn((handler)(
                        notification.clone(),
                        room.clone(),
                        self.clone(),
                    ));
                }
            }
        }

        Ok(response)
    }

    /// Repeatedly call sync to synchronize the client state with the server.
    ///
    /// This method will never return, if cancellation is needed the method
    /// should be wrapped in a cancelable task or the [`sync_with_callback`]
    /// method can be used.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. Note that those settings
    ///   will be only used for the first sync call.
    ///
    /// [`sync_with_callback`]: #method.sync_with_callback
    pub async fn sync(&self, sync_settings: SyncSettings<'_>) {
        self.sync_with_callback(sync_settings, |_| async { LoopCtrl::Continue }).await
    }

    /// Repeatedly call sync to synchronize the client state with the server.
    ///
    /// # Arguments
    ///
    /// * `sync_settings` - Settings for the sync call. Note that those settings
    ///   will be only used for the first sync call.
    ///
    /// * `callback` - A callback that will be called every time a successful
    ///   response has been fetched from the server. The callback must return a
    ///   boolean which signalizes if the method should stop syncing. If the
    ///   callback returns `LoopCtrl::Continue` the sync will continue, if the
    ///   callback returns `LoopCtrl::Break` the sync will be stopped.
    ///
    /// # Examples
    ///
    /// The following example demonstrates how to sync forever while sending all
    /// the interesting events through a mpsc channel to another thread e.g. a
    /// UI thread.
    ///
    /// ```no_run
    /// # use matrix_sdk::ruma::events::{
    /// #     room::message::{MessageEvent, MessageEventContent, TextMessageEventContent},
    /// # };
    /// # use std::sync::{Arc, RwLock};
    /// # use std::time::Duration;
    /// # use matrix_sdk::{Client, SyncSettings, LoopCtrl};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    ///
    /// use tokio::sync::mpsc::channel;
    ///
    /// let (tx, rx) = channel(100);
    ///
    /// let sync_channel = &tx;
    /// let sync_settings = SyncSettings::new()
    ///     .timeout(Duration::from_secs(30));
    ///
    /// client
    ///     .sync_with_callback(sync_settings, |response| async move {
    ///         let channel = sync_channel;
    ///
    ///         for (room_id, room) in response.rooms.join {
    ///             for event in room.timeline.events {
    ///                 channel.send(event).await.unwrap();
    ///             }
    ///         }
    ///
    ///         LoopCtrl::Continue
    ///     })
    ///     .await;
    /// })
    /// ```
    #[instrument(skip(callback))]
    pub async fn sync_with_callback<C>(
        &self,
        mut sync_settings: SyncSettings<'_>,
        callback: impl Fn(SyncResponse) -> C,
    ) where
        C: Future<Output = LoopCtrl>,
    {
        let mut last_sync_time: Option<Instant> = None;

        if sync_settings.token.is_none() {
            sync_settings.token = self.sync_token().await;
        }

        loop {
            let response = self.sync_once(sync_settings.clone()).await;

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    error!("Received an invalid response: {}", e);
                    sleep::new(Duration::from_secs(1)).await;
                    continue;
                }
            };

            #[cfg(feature = "encryption")]
            {
                // This is needed because sometimes we need to automatically
                // claim some one-time keys to unwedge an existing Olm session.
                if let Err(e) = self.claim_one_time_keys([].iter()).await {
                    warn!("Error while claiming one-time keys {:?}", e);
                }

                // TODO we should probably abort if we get an cryptostore error here
                let outgoing_requests = match self.base_client.outgoing_requests().await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Could not fetch the outgoing requests {:?}", e);
                        vec![]
                    }
                };

                for r in outgoing_requests {
                    match r.request() {
                        OutgoingRequests::KeysQuery(request) => {
                            if let Err(e) =
                                self.keys_query(r.request_id(), request.device_keys.clone()).await
                            {
                                warn!("Error while querying device keys {:?}", e);
                            }
                        }
                        OutgoingRequests::KeysUpload(request) => {
                            if let Err(e) = self.keys_upload(r.request_id(), request).await {
                                warn!("Error while querying device keys {:?}", e);
                            }
                        }
                        OutgoingRequests::ToDeviceRequest(request) => {
                            // TODO remove this unwrap
                            if let Ok(resp) = self.send_to_device(request).await {
                                self.base_client
                                    .mark_request_as_sent(r.request_id(), &resp)
                                    .await
                                    .unwrap();
                            }
                        }
                        OutgoingRequests::SignatureUpload(request) => {
                            // TODO remove this unwrap.
                            if let Ok(resp) = self.send(request.clone(), None).await {
                                self.base_client
                                    .mark_request_as_sent(r.request_id(), &resp)
                                    .await
                                    .unwrap();
                            }
                        }
                        OutgoingRequests::RoomMessage(request) => {
                            if let Ok(resp) = self.room_send_helper(request).await {
                                self.base_client
                                    .mark_request_as_sent(r.request_id(), &resp)
                                    .await
                                    .unwrap();
                            }
                        }
                        OutgoingRequests::KeysClaim(request) => {
                            if let Ok(resp) = self.send(request.clone(), None).await {
                                self.base_client
                                    .mark_request_as_sent(r.request_id(), &resp)
                                    .await
                                    .unwrap();
                            }
                        }
                    }
                }
            }

            if callback(response).await == LoopCtrl::Break {
                return;
            }

            let now = Instant::now();

            // If the last sync happened less than a second ago, sleep for a
            // while to not hammer out requests if the server doesn't respect
            // the sync timeout.
            if let Some(t) = last_sync_time {
                if now - t <= Duration::from_secs(1) {
                    sleep::new(Duration::from_secs(1)).await;
                }
            }

            last_sync_time = Some(now);

            sync_settings.token =
                Some(self.sync_token().await.expect("No sync token found after initial sync"));

            self.sync_beat.notify(usize::MAX);
        }
    }

    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    async fn send_account_data(
        &self,
        content: ruma::events::AnyGlobalAccountDataEventContent,
    ) -> Result<ruma::api::client::r0::config::set_global_account_data::Response> {
        let own_user =
            self.user_id().await.ok_or_else(|| Error::from(HttpError::AuthenticationRequired))?;
        let data = serde_json::value::to_raw_value(&content)?;

        let request = ruma::api::client::r0::config::set_global_account_data::Request::new(
            &data,
            ruma::events::EventContent::event_type(&content),
            &own_user,
        );

        Ok(self.send(request, None).await?)
    }

    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub(crate) async fn create_dm_room(&self, user_id: UserId) -> Result<Option<room::Joined>> {
        use ruma::{
            api::client::r0::room::create_room::RoomPreset,
            events::AnyGlobalAccountDataEventContent,
        };

        const SYNC_WAIT_TIME: Duration = Duration::from_secs(3);

        // First we create the DM room, where we invite the user and tell the
        // invitee that the room should be a DM.
        let invite = &[user_id.clone()];

        let request = assign!(
            ruma::api::client::r0::room::create_room::Request::new(),
            {
                invite,
                is_direct: true,
                preset: Some(RoomPreset::TrustedPrivateChat),
            }
        );

        let response = self.send(request, None).await?;

        // Now we need to mark the room as a DM for ourselves, we fetch the
        // existing `m.direct` event and append the room to the list of DMs we
        // have with this user.
        let mut content = self
            .store()
            .get_account_data_event(EventType::Direct)
            .await?
            .map(|e| e.deserialize())
            .transpose()?
            .and_then(|e| {
                if let AnyGlobalAccountDataEventContent::Direct(c) = e.content() {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| ruma::events::direct::DirectEventContent(BTreeMap::new()));

        content.entry(user_id.to_owned()).or_default().push(response.room_id.to_owned());

        // TODO We should probably save the fact that we need to send this out
        // because otherwise we might end up in a state where we have a DM that
        // isn't marked as one.
        self.send_account_data(AnyGlobalAccountDataEventContent::Direct(content)).await?;

        // If the room is already in our store, fetch it, otherwise wait for a
        // sync to be done which should put the room into our store.
        if let Some(room) = self.get_joined_room(&response.room_id) {
            Ok(Some(room))
        } else {
            self.sync_beat.listen().wait_timeout(SYNC_WAIT_TIME);
            Ok(self.get_joined_room(&response.room_id))
        }
    }

    /// Claim one-time keys creating new Olm sessions.
    ///
    /// # Arguments
    ///
    /// * `users` - The list of user/device pairs that we should claim keys for.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    #[instrument(skip(users))]
    pub(crate) async fn claim_one_time_keys(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        let _lock = self.key_claim_lock.lock().await;

        if let Some((request_id, request)) = self.base_client.get_missing_sessions(users).await? {
            let response = self.send(request, None).await?;
            self.base_client.mark_request_as_sent(&request_id, &response).await?;
        }

        Ok(())
    }

    /// Upload the E2E encryption keys.
    ///
    /// This uploads the long lived device keys as well as the required amount
    /// of one-time keys.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in, or if no encryption keys need to
    /// be uploaded.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    #[instrument]
    async fn keys_upload(
        &self,
        request_id: &Uuid,
        request: &upload_keys::Request,
    ) -> Result<upload_keys::Response> {
        debug!(
            "Uploading encryption keys device keys: {}, one-time-keys: {}",
            request.device_keys.is_some(),
            request.one_time_keys.as_ref().map_or(0, |k| k.len())
        );

        let response = self.send(request.clone(), None).await?;
        self.base_client.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    /// Get the current, if any, sync token of the client.
    /// This will be None if the client didn't sync at least once.
    pub async fn sync_token(&self) -> Option<String> {
        self.base_client.sync_token().await
    }

    /// Query the server for users device keys.
    ///
    /// # Panics
    ///
    /// Panics if no key query needs to be done.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    #[instrument]
    async fn keys_query(
        &self,
        request_id: &Uuid,
        device_keys: BTreeMap<UserId, Vec<DeviceIdBox>>,
    ) -> Result<get_keys::Response> {
        let request = assign!(get_keys::Request::new(), { device_keys });

        let response = self.send(request, None).await?;
        self.base_client.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    /// Get a verification object with the given flow id.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_verification(&self, user_id: &UserId, flow_id: &str) -> Option<Verification> {
        let olm = self.base_client.olm_machine().await?;
        olm.get_verification(user_id, flow_id).map(|v| match v {
            matrix_sdk_base::crypto::Verification::SasV1(s) => {
                SasVerification { inner: s, client: self.clone() }.into()
            }
            matrix_sdk_base::crypto::Verification::QrV1(qr) => {
                QrVerification { inner: qr, client: self.clone() }.into()
            }
        })
    }

    /// Get a `VerificationRequest` object for the given user with the given
    /// flow id.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_verification_request(
        &self,
        user_id: &UserId,
        flow_id: impl AsRef<str>,
    ) -> Option<VerificationRequest> {
        let olm = self.base_client.olm_machine().await?;

        olm.get_verification_request(user_id, flow_id)
            .map(|r| VerificationRequest { inner: r, client: self.clone() })
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let device = client.get_device(&alice, "DEVICEID".into())
    ///     .await
    ///     .unwrap()
    ///     .unwrap();
    ///
    /// println!("{:?}", device.verified());
    ///
    /// let verification = device.request_verification().await.unwrap();
    /// # });
    /// ```
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> StdResult<Option<Device>, CryptoStoreError> {
        let device = self.base_client.get_device(user_id, device_id).await?;

        Ok(device.map(|d| Device { inner: d, client: self.clone() }))
    }

    /// Get a E2EE identity of an user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the identity belongs to.
    ///
    /// Returns a `UserIdentity` if one is found and the crypto store
    /// didn't throw an error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let user = client.get_user_identity(&alice).await?;
    ///
    /// if let Some(user) = user {
    ///     println!("{:?}", user.verified());
    ///
    ///     let verification = user.request_verification().await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> StdResult<Option<crate::identities::UserIdentity>, CryptoStoreError> {
        use crate::identities::UserIdentity;

        if let Some(olm) = self.base_client.olm_machine().await {
            let identity = olm.get_identity(user_id).await?;

            Ok(identity.map(|i| match i {
                matrix_sdk_base::crypto::UserIdentities::Own(i) => {
                    UserIdentity::new_own(self.clone(), i)
                }
                matrix_sdk_base::crypto::UserIdentities::Other(i) => {
                    UserIdentity::new(self.clone(), i, self.get_dm_room(user_id))
                }
            }))
        } else {
            Ok(None)
        }
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we have
    /// stored locally.
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn cross_signing_status(&self) -> Option<CrossSigningStatus> {
        if let Some(machine) = self.base_client.olm_machine().await {
            Some(machine.cross_signing_status().await)
        } else {
            None
        }
    }

    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    fn get_dm_room(&self, user_id: &UserId) -> Option<room::Joined> {
        let rooms = self.joined_rooms();
        let room_pairs: Vec<_> =
            rooms.iter().map(|r| (r.room_id().to_owned(), r.direct_target())).collect();
        trace!(rooms =? room_pairs, "Finding direct room");

        let room = rooms.into_iter().find(|r| r.direct_target().as_ref() == Some(user_id));

        trace!(room =? room, "Found room");
        room
    }

    /// Create and upload a new cross signing identity.
    ///
    /// # Arguments
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    /// request needs to set this to `None` and will always fail with an
    /// `UiaaResponse`. The response will contain information for the
    /// interactive auth and the same request needs to be made but this time
    /// with some `auth_data` provided.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::{convert::TryFrom, collections::BTreeMap};
    /// # use matrix_sdk::{
    /// #     ruma::{api::client::r0::uiaa, assign, UserId},
    /// #     Client,
    /// # };
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use serde_json::json;
    /// # let user_id = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// if let Err(e) = client.bootstrap_cross_signing(None).await {
    ///     if let Some(response) = e.uiaa_response() {
    ///         let auth_data = uiaa::AuthData::Password(assign!(
    ///             uiaa::Password::new(uiaa::UserIdentifier::MatrixId("example"), "wordpass"),
    ///             { session: response.session.as_deref() }
    ///         ));
    ///
    ///         client
    ///             .bootstrap_cross_signing(Some(auth_data))
    ///             .await
    ///             .expect("Couldn't bootstrap cross signing")
    ///     } else {
    ///         panic!("Error durign cross signing bootstrap {:#?}", e);
    ///     }
    /// }
    /// # })
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn bootstrap_cross_signing(&self, auth_data: Option<AuthData<'_>>) -> Result<()> {
        let olm = self.base_client.olm_machine().await.ok_or(Error::AuthenticationRequired)?;

        let (request, signature_request) = olm.bootstrap_cross_signing(false).await?;

        let request = assign!(UploadSigningKeysRequest::new(), {
            auth: auth_data,
            master_key: request.master_key,
            self_signing_key: request.self_signing_key,
            user_signing_key: request.user_signing_key,
        });

        self.send(request, None).await?;
        self.send(signature_request, None).await?;

        Ok(())
    }

    /// Get a map holding all the devices of an user.
    ///
    /// This will always return an empty map if the client hasn't been logged
    /// in.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # let client = Client::new(homeserver).unwrap();
    /// # block_on(async {
    /// let devices = client.get_user_devices(&alice).await.unwrap();
    ///
    /// for device in devices.devices() {
    ///     println!("{:?}", device);
    /// }
    /// # });
    /// ```
    #[cfg(feature = "encryption")]
    #[cfg_attr(feature = "docs", doc(cfg(encryption)))]
    pub async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StdResult<UserDevices, CryptoStoreError> {
        let devices = self.base_client.get_user_devices(user_id).await?;

        Ok(UserDevices { inner: devices, client: self.clone() })
    }

    /// Export E2EE keys that match the given predicate encrypting them with the
    /// given passphrase.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will be saved.
    ///
    /// * `passphrase` - The passphrase that will be used to encrypt the
    ///   exported
    /// room keys.
    ///
    /// * `predicate` - A closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true` the `InboundGroupSessoin` will be included in the export,
    /// if the closure returns `false` it will not be included.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// This method will panic if it can't get enough randomness from the OS to
    /// encrypt the exported keys securely.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// // Export all room keys.
    /// client
    ///     .export_keys(path, "secret-passphrase", |_| true)
    ///     .await
    ///     .expect("Can't export keys.");
    ///
    /// // Export only the room keys for a certain room.
    /// let path = PathBuf::from("/home/example/e2e-room-keys.txt");
    /// let room_id = room_id!("!test:localhost");
    ///
    /// client
    ///     .export_keys(path, "secret-passphrase", |s| s.room_id() == &room_id)
    ///     .await
    ///     .expect("Can't export keys.");
    /// # });
    /// ```
    #[cfg(all(feature = "encryption", not(target_arch = "wasm32")))]
    #[cfg_attr(feature = "docs", doc(cfg(all(encryption, not(target_arch = "wasm32")))))]
    pub async fn export_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
        predicate: impl FnMut(&InboundGroupSession) -> bool,
    ) -> Result<()> {
        let olm = self.base_client.olm_machine().await.ok_or(Error::AuthenticationRequired)?;

        let keys = olm.export_keys(predicate).await?;
        let passphrase = Zeroizing::new(passphrase.to_owned());

        let encrypt = move || -> Result<()> {
            let export: String = encrypt_key_export(&keys, &passphrase, 500_000)?;
            let mut file = std::fs::File::create(path)?;
            file.write_all(&export.into_bytes())?;
            Ok(())
        };

        let task = tokio::task::spawn_blocking(encrypt);
        task.await.expect("Task join error")
    }

    /// Import E2EE keys from the given file path.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will can be found.
    ///
    /// * `passphrase` - The passphrase that should be used to decrypt the
    /// exported room keys.
    ///
    /// Returns a tuple of numbers that represent the number of sessions that
    /// were imported and the total number of sessions that were found in the
    /// key export.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let mut client = Client::new(homeserver).unwrap();
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// client
    ///     .import_keys(path, "secret-passphrase")
    ///     .await
    ///     .expect("Can't import keys");
    /// # });
    /// ```
    #[cfg(all(feature = "encryption", not(target_arch = "wasm32")))]
    #[cfg_attr(feature = "docs", doc(cfg(all(encryption, not(target_arch = "wasm32")))))]
    pub async fn import_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
    ) -> StdResult<(usize, usize), RoomKeyImportError> {
        let olm = self.base_client.olm_machine().await.ok_or(RoomKeyImportError::StoreClosed)?;
        let passphrase = Zeroizing::new(passphrase.to_owned());

        let decrypt = move || {
            let file = std::fs::File::open(path)?;
            decrypt_key_export(file, &passphrase)
        };

        let task = tokio::task::spawn_blocking(decrypt);
        let import = task.await.expect("Task join error")?;

        Ok(olm.import_keys(import, |_, _| {}).await?)
    }

    /// Get a media file's content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    ///
    /// * `use_cache` - If we should use the media cache for this request.
    pub async fn get_media_content(
        &self,
        request: &MediaRequest,
        use_cache: bool,
    ) -> Result<Vec<u8>> {
        let content = if use_cache {
            self.base_client.store().get_media_content(request).await?
        } else {
            None
        };

        if let Some(content) = content {
            Ok(content)
        } else {
            let content: Vec<u8> = match &request.media_type {
                MediaType::Encrypted(file) => {
                    let content: Vec<u8> =
                        self.send(get_content::Request::from_url(&file.url)?, None).await?.file;

                    #[cfg(feature = "encryption")]
                    let content = {
                        let mut cursor = Cursor::new(content);
                        let mut reader =
                            AttachmentDecryptor::new(&mut cursor, file.as_ref().clone().into())?;

                        let mut decrypted = Vec::new();
                        reader.read_to_end(&mut decrypted)?;

                        decrypted
                    };

                    content
                }
                MediaType::Uri(uri) => {
                    if let MediaFormat::Thumbnail(size) = &request.format {
                        self.send(
                            get_content_thumbnail::Request::from_url(uri, size.width, size.height)?,
                            None,
                        )
                        .await?
                        .file
                    } else {
                        self.send(get_content::Request::from_url(uri)?, None).await?.file
                    }
                }
            };

            if use_cache {
                self.base_client.store().add_media_content(request, content.clone()).await?;
            }

            Ok(content)
        }
    }

    /// Remove a media file's content from the store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    pub async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        Ok(self.base_client.store().remove_media_content(request).await?)
    }

    /// Delete all the media content corresponding to the given
    /// uri from the store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the files.
    pub async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        Ok(self.base_client.store().remove_media_content_for_uri(uri).await?)
    }

    /// Get the file of the given media event content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// Returns `Ok(None)` if the event content has no file.
    ///
    /// This is a convenience method that calls the
    /// [`get_media_content`](#method.get_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `use_cache` - If we should use the media cache for this file.
    pub async fn get_file(
        &self,
        event_content: impl MediaEventContent,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(media_type) = event_content.file() {
            Ok(Some(
                self.get_media_content(
                    &MediaRequest { media_type, format: MediaFormat::File },
                    use_cache,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Remove the file of the given media event content from the cache.
    ///
    /// This is a convenience method that calls the
    /// [`remove_media_content`](#method.remove_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    pub async fn remove_file(&self, event_content: impl MediaEventContent) -> Result<()> {
        if let Some(media_type) = event_content.file() {
            self.remove_media_content(&MediaRequest { media_type, format: MediaFormat::File })
                .await?
        }

        Ok(())
    }

    /// Get a thumbnail of the given media event content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// Returns `Ok(None)` if the event content has no thumbnail.
    ///
    /// This is a convenience method that calls the
    /// [`get_media_content`](#method.get_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `size` - The _desired_ size of the thumbnail. The actual thumbnail may
    ///   not match the size specified.
    ///
    /// * `use_cache` - If we should use the media cache for this thumbnail.
    pub async fn get_thumbnail(
        &self,
        event_content: impl MediaEventContent,
        size: MediaThumbnailSize,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(media_type) = event_content.thumbnail() {
            Ok(Some(
                self.get_media_content(
                    &MediaRequest { media_type, format: MediaFormat::Thumbnail(size) },
                    use_cache,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Remove the thumbnail of the given media event content from the cache.
    ///
    /// This is a convenience method that calls the
    /// [`remove_media_content`](#method.remove_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `size` - The _desired_ size of the thumbnail. Must match the size
    ///   requested with [`get_thumbnail`](#method.get_thumbnail).
    pub async fn remove_thumbnail(
        &self,
        event_content: impl MediaEventContent,
        size: MediaThumbnailSize,
    ) -> Result<()> {
        if let Some(media_type) = event_content.file() {
            self.remove_media_content(&MediaRequest {
                media_type,
                format: MediaFormat::Thumbnail(size),
            })
            .await?
        }

        Ok(())
    }

    /// Gets information about the owner of a given access token.
    pub async fn whoami(&self) -> HttpResult<whoami::Response> {
        let request = whoami::Request::new();
        self.send(request, None).await
    }

    #[cfg(feature = "encryption")]
    pub(crate) async fn send_verification_request(
        &self,
        request: matrix_sdk_base::crypto::OutgoingVerificationRequest,
    ) -> Result<()> {
        match request {
            matrix_sdk_base::crypto::OutgoingVerificationRequest::ToDevice(t) => {
                self.send_to_device(&t).await?;
            }
            matrix_sdk_base::crypto::OutgoingVerificationRequest::InRoom(r) => {
                self.room_send_helper(&r).await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::{
        collections::BTreeMap,
        convert::{TryFrom, TryInto},
        future,
        io::Cursor,
        str::FromStr,
        sync::Arc,
        time::Duration,
    };

    use matrix_sdk_base::media::{MediaFormat, MediaRequest, MediaThumbnailSize, MediaType};
    use matrix_sdk_test::{test_json, EventBuilder, EventsJson};
    use mockito::{mock, Matcher};
    use ruma::{
        api::{
            client::{
                self as client_api,
                r0::{
                    account::register::{RegistrationKind, Request as RegistrationRequest},
                    directory::{
                        get_public_rooms,
                        get_public_rooms_filtered::{self, Request as PublicRoomsFilterRequest},
                    },
                    media::get_content_thumbnail::Method,
                    membership::Invite3pidInit,
                    session::get_login_types::LoginType,
                    uiaa::{self, UiaaResponse},
                },
            },
            error::{FromHttpResponseError, ServerError},
        },
        assign,
        directory::Filter,
        event_id,
        events::{
            room::{
                member::MemberEventContent,
                message::{ImageMessageEventContent, MessageEventContent},
                ImageInfo,
            },
            AnyMessageEventContent, AnySyncStateEvent, EventType, SyncStateEvent,
        },
        mxc_uri, room_id, thirdparty, uint, user_id, UserId,
    };
    use serde_json::json;

    use super::{Client, Session, SyncSettings, Url};
    use crate::{room, ClientConfig, HttpError, RequestConfig, RoomMember};

    async fn logged_in_client() -> Client {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost"),
            device_id: "DEVICEID".into(),
        };
        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();

        client
    }

    #[tokio::test]
    async fn set_homeserver() {
        let homeserver = Url::from_str("http://example.com/").unwrap();

        let mut client = Client::new(homeserver).unwrap();

        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        client.set_homeserver(homeserver.clone()).await;

        assert_eq!(client.homeserver().await, homeserver);
    }

    #[tokio::test]
    async fn successful_discovery() {
        let server_url = mockito::server_url();
        let domain = server_url.strip_prefix("http://").unwrap();
        let alice = UserId::try_from("@alice:".to_string() + domain).unwrap();

        let _m_well_known = mock("GET", "/.well-known/matrix/client")
            .with_status(200)
            .with_body(
                test_json::WELL_KNOWN.to_string().replace("HOMESERVER_URL", server_url.as_ref()),
            )
            .create();

        let _m_versions = mock("GET", "/_matrix/client/versions")
            .with_status(200)
            .with_body(test_json::VERSIONS.to_string())
            .create();
        let client = Client::new_from_user_id(alice).await.unwrap();

        assert_eq!(client.homeserver().await, Url::parse(server_url.as_ref()).unwrap());
    }

    #[tokio::test]
    async fn discovery_broken_server() {
        let server_url = mockito::server_url();
        let domain = server_url.strip_prefix("http://").unwrap();
        let alice = UserId::try_from("@alice:".to_string() + domain).unwrap();

        let _m = mock("GET", "/.well-known/matrix/client")
            .with_status(200)
            .with_body(
                test_json::WELL_KNOWN.to_string().replace("HOMESERVER_URL", server_url.as_ref()),
            )
            .create();

        if Client::new_from_user_id(alice).await.is_ok() {
            panic!(
                "Creating a client from a user ID should fail when the \
                   .well-known server returns no version information."
            );
        }
    }

    #[tokio::test]
    async fn login() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let client = Client::new(homeserver).unwrap();

        let _m_types = mock("GET", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body(test_json::LOGIN_TYPES.to_string())
            .create();

        let can_password = client
            .get_login_types()
            .await
            .unwrap()
            .flows
            .iter()
            .any(|flow| matches!(flow, LoginType::Password(_)));
        assert!(can_password);

        let _m_login = mock("POST", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body(test_json::LOGIN.to_string())
            .create();

        client.login("example", "wordpass", None, None).await.unwrap();

        let logged_in = client.logged_in().await;
        assert!(logged_in, "Client should be logged in");
    }

    #[cfg(feature = "sso_login")]
    #[tokio::test]
    async fn login_with_sso() {
        let _m_login = mock("POST", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body(test_json::LOGIN.to_string())
            .create();

        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();

        client
            .login_with_sso(
                |sso_url| async move {
                    let sso_url = Url::parse(sso_url.as_str()).unwrap();

                    let (_, redirect) =
                        sso_url.query_pairs().find(|(key, _)| key == "redirectUrl").unwrap();

                    let mut redirect_url = Url::parse(redirect.into_owned().as_str()).unwrap();
                    redirect_url.set_query(Some("loginToken=tinytoken"));

                    reqwest::get(redirect_url.to_string()).await.unwrap();

                    Ok(())
                },
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let logged_in = client.logged_in().await;
        assert!(logged_in, "Client should be logged in");
    }

    #[tokio::test]
    async fn login_with_sso_token() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let client = Client::new(homeserver).unwrap();

        let _m = mock("GET", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body(test_json::LOGIN_TYPES.to_string())
            .create();

        let can_sso = client
            .get_login_types()
            .await
            .unwrap()
            .flows
            .iter()
            .any(|flow| matches!(flow, LoginType::Sso(_)));
        assert!(can_sso);

        let sso_url = client.get_sso_login_url("http://127.0.0.1:3030").await;
        assert!(sso_url.is_ok());

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body(test_json::LOGIN.to_string())
            .create();

        client.login_with_token("averysmalltoken", None, None).await.unwrap();

        let logged_in = client.logged_in().await;
        assert!(logged_in, "Client should be logged in");
    }

    #[tokio::test]
    async fn devices() {
        let client = logged_in_client().await;

        let _m = mock("GET", "/_matrix/client/r0/devices")
            .with_status(200)
            .with_body(test_json::DEVICES.to_string())
            .create();

        assert!(client.devices().await.is_ok());
    }

    #[tokio::test]
    async fn test_join_leave_room() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost"),
            device_id: "DEVICEID".into(),
        };

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .with_body(test_json::SYNC.to_string())
            .create();

        let client = Client::new(homeserver.clone()).unwrap();
        client.restore_login(session.clone()).await.unwrap();

        let room = client.get_joined_room(&room_id);
        assert!(room.is_none());

        client.sync_once(SyncSettings::default()).await.unwrap();

        let room = client.get_left_room(&room_id);
        assert!(room.is_none());

        let room = client.get_joined_room(&room_id);
        assert!(room.is_some());

        // test store reloads with correct room state from the sled store
        let path = tempfile::tempdir().unwrap();
        let config = ClientConfig::default().store_path(path);
        let joined_client = Client::new_with_config(homeserver, config).unwrap();
        joined_client.restore_login(session).await.unwrap();

        // joined room reloaded from state store
        joined_client.sync_once(SyncSettings::default()).await.unwrap();
        let room = joined_client.get_joined_room(&room_id);
        assert!(room.is_some());

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .with_body(test_json::LEAVE_SYNC_EVENT.to_string())
            .create();

        joined_client.sync_once(SyncSettings::default()).await.unwrap();

        let room = joined_client.get_joined_room(&room_id);
        assert!(room.is_none());

        let room = joined_client.get_left_room(&room_id);
        assert!(room.is_some());
    }

    #[tokio::test]
    async fn account_data() {
        let client = logged_in_client().await;

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .with_body(test_json::SYNC.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync_once(sync_settings).await.unwrap();

        // let bc = &client.base_client;
        // let ignored_users = bc.ignored_users.read().await;
        // assert_eq!(1, ignored_users.len())
    }

    #[tokio::test]
    async fn event_handler() {
        use std::sync::atomic::{AtomicU8, Ordering::SeqCst};

        let client = logged_in_client().await;

        let member_count = Arc::new(AtomicU8::new(0));
        let typing_count = Arc::new(AtomicU8::new(0));
        let power_levels_count = Arc::new(AtomicU8::new(0));

        client
            .register_event_handler({
                let member_count = member_count.clone();
                move |_ev: SyncStateEvent<MemberEventContent>, _room: room::Room| {
                    member_count.fetch_add(1, SeqCst);
                    future::ready(())
                }
            })
            .await
            .register_event_handler({
                let typing_count = typing_count.clone();
                move |_ev: SyncStateEvent<MemberEventContent>| {
                    typing_count.fetch_add(1, SeqCst);
                    future::ready(())
                }
            })
            .await
            .register_event_handler({
                let power_levels_count = power_levels_count.clone();
                move |_ev: SyncStateEvent<MemberEventContent>,
                      _client: Client,
                      _room: room::Room| {
                    power_levels_count.fetch_add(1, SeqCst);
                    future::ready(())
                }
            })
            .await;

        let response = EventBuilder::default()
            .add_room_event(EventsJson::Member)
            .add_ephemeral(EventsJson::Typing)
            .add_state_event(EventsJson::PowerLevels)
            .build_sync_response();
        client.process_sync(response).await.unwrap();

        assert_eq!(member_count.load(SeqCst), 1);
        assert_eq!(typing_count.load(SeqCst), 1);
        assert_eq!(power_levels_count.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn room_creation() {
        let client = logged_in_client().await;

        let response = EventBuilder::default()
            .add_state_event(EventsJson::Member)
            .add_state_event(EventsJson::PowerLevels)
            .build_sync_response();

        client.base_client.receive_sync_response(response).await.unwrap();
        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        assert_eq!(client.homeserver().await, Url::parse(&mockito::server_url()).unwrap());

        let room = client.get_joined_room(&room_id);
        assert!(room.is_some());
    }

    #[tokio::test]
    async fn login_error() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(403)
            .with_body(test_json::LOGIN_RESPONSE_ERR.to_string())
            .create();

        if let Err(err) = client.login("example", "wordpass", None, None).await {
            if let crate::Error::Http(HttpError::ClientApi(FromHttpResponseError::Http(
                ServerError::Known(client_api::Error { kind, message, status_code }),
            ))) = err
            {
                if let client_api::error::ErrorKind::Forbidden = kind {
                } else {
                    panic!("found the wrong `ErrorKind` {:?}, expected `Forbidden", kind);
                }
                assert_eq!(message, "Invalid password".to_string());
                assert_eq!(status_code, http::StatusCode::from_u16(403).unwrap());
            } else {
                panic!("found the wrong `Error` type {:?}, expected `Error::RumaResponse", err);
            }
        } else {
            panic!("this request should return an `Err` variant")
        }
    }

    #[tokio::test]
    async fn register_error() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/register\?.*$".to_string()))
            .with_status(403)
            .with_body(test_json::REGISTRATION_RESPONSE_ERR.to_string())
            .create();

        let user = assign!(RegistrationRequest::new(), {
            username: Some("user"),
            password: Some("password"),
            auth: Some(uiaa::AuthData::FallbackAcknowledgement(
                uiaa::FallbackAcknowledgement::new("foobar"),
            )),
            kind: RegistrationKind::User,
        });

        if let Err(err) = client.register(user).await {
            if let HttpError::UiaaError(FromHttpResponseError::Http(ServerError::Known(
                UiaaResponse::MatrixError(client_api::Error { kind, message, status_code }),
            ))) = err
            {
                if let client_api::error::ErrorKind::Forbidden = kind {
                } else {
                    panic!("found the wrong `ErrorKind` {:?}, expected `Forbidden", kind);
                }
                assert_eq!(message, "Invalid password".to_string());
                assert_eq!(status_code, http::StatusCode::from_u16(403).unwrap());
            } else {
                panic!("found the wrong `Error` type {:#?}, expected `UiaaResponse`", err);
            }
        } else {
            panic!("this request should return an `Err` variant")
        }
    }

    #[tokio::test]
    async fn join_room_by_id() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/join".to_string()))
            .with_status(200)
            .with_body(test_json::ROOM_ID.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let room_id = room_id!("!testroom:example.org");

        assert_eq!(
            // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
            // field
            client.join_room_by_id(&room_id).await.unwrap().room_id,
            room_id
        );
    }

    #[tokio::test]
    async fn join_room_by_id_or_alias() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/join/".to_string()))
            .with_status(200)
            .with_body(test_json::ROOM_ID.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let room_id = room_id!("!testroom:example.org").into();

        assert_eq!(
            // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
            // field
            client
                .join_room_by_id_or_alias(&room_id, &["server.com".try_into().unwrap()])
                .await
                .unwrap()
                .room_id,
            room_id!("!testroom:example.org")
        );
    }

    #[tokio::test]
    async fn invite_user_by_id() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_string()))
            .with_status(200)
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let user = user_id!("@example:localhost");
        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.invite_user_by_id(&user).await.unwrap();
    }

    #[tokio::test]
    async fn invite_user_by_3pid() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_string()))
            .with_status(200)
            // empty JSON object
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.invite_user_by_3pid(
            Invite3pidInit {
                id_server: "example.org",
                id_access_token: "IdToken",
                medium: thirdparty::Medium::Email,
                address: "address",
            }
            .into(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn room_search_all() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/publicRooms".to_string()))
            .with_status(200)
            .with_body(test_json::PUBLIC_ROOMS.to_string())
            .create();

        let get_public_rooms::Response { chunk, .. } =
            client.public_rooms(Some(10), None, None).await.unwrap();
        assert_eq!(chunk.len(), 1);
    }

    #[tokio::test]
    async fn room_search_filtered() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/publicRooms".to_string()))
            .with_status(200)
            .with_body(test_json::PUBLIC_ROOMS.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let generic_search_term = Some("cheese");
        let filter = assign!(Filter::new(), { generic_search_term });
        let request = assign!(PublicRoomsFilterRequest::new(), { filter });

        let get_public_rooms_filtered::Response { chunk, .. } =
            client.public_rooms_filtered(request).await.unwrap();
        assert_eq!(chunk.len(), 1);
    }

    #[tokio::test]
    async fn leave_room() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/leave".to_string()))
            .with_status(200)
            // this is an empty JSON object
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.leave().await.unwrap();
    }

    #[tokio::test]
    async fn ban_user() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/ban".to_string()))
            .with_status(200)
            // this is an empty JSON object
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let user = user_id!("@example:localhost");
        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.ban_user(&user, None).await.unwrap();
    }

    #[tokio::test]
    async fn kick_user() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/kick".to_string()))
            .with_status(200)
            // this is an empty JSON object
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let user = user_id!("@example:localhost");
        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.kick_user(&user, None).await.unwrap();
    }

    #[tokio::test]
    async fn forget_room() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/forget".to_string()))
            .with_status(200)
            // this is an empty JSON object
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::LEAVE_SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_left_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.forget().await.unwrap();
    }

    #[tokio::test]
    async fn read_receipt() {
        let client = logged_in_client().await;

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/receipt".to_string()))
            .with_status(200)
            // this is an empty JSON object
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let event_id = event_id!("$xxxxxx:example.org");
        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.read_receipt(&event_id).await.unwrap();
    }

    #[tokio::test]
    async fn read_marker() {
        let client = logged_in_client().await;

        let _m =
            mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/read_markers".to_string()))
                .with_status(200)
                // this is an empty JSON object
                .with_body(test_json::LOGOUT.to_string())
                .match_header("authorization", "Bearer 1234")
                .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let event_id = event_id!("$xxxxxx:example.org");
        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.read_marker(&event_id, None).await.unwrap();
    }

    #[tokio::test]
    async fn typing_notice() {
        let client = logged_in_client().await;

        let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/typing".to_string()))
            .with_status(200)
            // this is an empty JSON object
            .with_body(test_json::LOGOUT.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        room.typing_notice(true).await.unwrap();
    }

    #[tokio::test]
    async fn room_state_event_send() {
        use ruma::events::{
            room::member::{MemberEventContent, MembershipState},
            AnyStateEventContent,
        };

        let client = logged_in_client().await;

        let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/state/.*".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::EVENT_ID.to_string())
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        let room = client.get_joined_room(&room_id).unwrap();

        let avatar_url = mxc_uri!("mxc://example.org/avA7ar");
        let member_event = assign!(MemberEventContent::new(MembershipState::Join), {
            avatar_url: Some(avatar_url)
        });
        let content = AnyStateEventContent::RoomMember(member_event);
        let response = room.send_state_event(content, "").await.unwrap();
        assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id);
    }

    #[tokio::test]
    async fn room_message_send() {
        use matrix_sdk_common::uuid::Uuid;

        let client = logged_in_client().await;

        let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::EVENT_ID.to_string())
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        let content =
            AnyMessageEventContent::RoomMessage(MessageEventContent::text_plain("Hello world"));
        let txn_id = Uuid::new_v4();
        let response = room.send(content, Some(txn_id)).await.unwrap();

        assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
    }

    #[tokio::test]
    async fn room_attachment_send() {
        let client = logged_in_client().await;

        let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::EVENT_ID.to_string())
            .create();

        let _m = mock("POST", Matcher::Regex(r"^/_matrix/media/r0/upload".to_string()))
            .with_status(200)
            .match_header("content-type", "image/jpeg")
            .with_body(
                json!({
                  "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
                })
                .to_string(),
            )
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        let mut media = Cursor::new("Hello world");

        let response =
            room.send_attachment("image", &mime::IMAGE_JPEG, &mut media, None).await.unwrap();

        assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
    }

    #[tokio::test]
    async fn room_redact() {
        use matrix_sdk_common::uuid::Uuid;

        let client = logged_in_client().await;

        let _m =
            mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?".to_string()))
                .with_status(200)
                .match_header("authorization", "Bearer 1234")
                .with_body(test_json::EVENT_ID.to_string())
                .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        let event_id = event_id!("$xxxxxxxx:example.com");

        let txn_id = Uuid::new_v4();
        let reason = Some("Indecent material");
        let response = room.redact(&event_id, reason, Some(txn_id)).await.unwrap();

        assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
    }

    #[tokio::test]
    async fn user_presence() {
        let client = logged_in_client().await;

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .create();

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/members".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::MEMBERS.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();
        let members: Vec<RoomMember> = room.active_members().await.unwrap();

        assert_eq!(1, members.len());
        // assert!(room.power_levels.is_some())
    }

    #[tokio::test]
    async fn calculate_room_names_from_summary() {
        let client = logged_in_client().await;

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::DEFAULT_SYNC_SUMMARY.to_string())
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync_once(sync_settings).await.unwrap();
        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        assert_eq!("example2", room.display_name().await.unwrap());
    }

    #[tokio::test]
    async fn invited_rooms() {
        let client = logged_in_client().await;

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::INVITE_SYNC.to_string())
            .create();

        let _response = client.sync_once(SyncSettings::default()).await.unwrap();

        assert!(client.joined_rooms().is_empty());
        assert!(client.left_rooms().is_empty());
        assert!(!client.invited_rooms().is_empty());

        assert!(client.get_invited_room(&room_id!("!696r7674:example.com")).is_some());
    }

    #[tokio::test]
    async fn left_rooms() {
        let client = logged_in_client().await;

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::LEAVE_SYNC.to_string())
            .create();

        let _response = client.sync_once(SyncSettings::default()).await.unwrap();

        assert!(client.joined_rooms().is_empty());
        assert!(!client.left_rooms().is_empty());
        assert!(client.invited_rooms().is_empty());

        assert!(client.get_left_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).is_some())
    }

    #[tokio::test]
    async fn sync() {
        let client = logged_in_client().await;

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .with_body(test_json::SYNC.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let response = client.sync_once(sync_settings).await.unwrap();

        assert_ne!(response.next_batch, "");

        assert!(client.sync_token().await.is_some());
    }

    #[tokio::test]
    async fn room_names() {
        let client = logged_in_client().await;

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::SYNC.to_string())
            .expect_at_least(1)
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync_once(sync_settings).await.unwrap();

        let room = client.get_joined_room(&room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

        assert_eq!("tutorial".to_string(), room.display_name().await.unwrap());

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(test_json::INVITE_SYNC.to_string())
            .expect_at_least(1)
            .create();

        let _response = client.sync_once(SyncSettings::new()).await.unwrap();

        let invited_room = client.get_invited_room(&room_id!("!696r7674:example.com")).unwrap();

        assert_eq!("My Room Name".to_string(), invited_room.display_name().await.unwrap());
    }

    #[tokio::test]
    async fn delete_devices() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/delete_devices")
            .with_status(401)
            .with_body(
                json!({
                    "flows": [
                        {
                            "stages": [
                                "m.login.password"
                            ]
                        }
                    ],
                    "params": {},
                    "session": "vBslorikviAjxzYBASOBGfPp"
                })
                .to_string(),
            )
            .create();

        let _m = mock("POST", "/_matrix/client/r0/delete_devices")
            .with_status(401)
            // empty response
            // TODO rename that response type.
            .with_body(test_json::LOGOUT.to_string())
            .create();

        let devices = &["DEVICEID".into()];

        if let Err(e) = client.delete_devices(devices, None).await {
            if let Some(info) = e.uiaa_response() {
                let mut auth_parameters = BTreeMap::new();

                let identifier = json!({
                    "type": "m.id.user",
                    "user": "example",
                });
                auth_parameters.insert("identifier".to_owned(), identifier);
                auth_parameters.insert("password".to_owned(), "wordpass".into());

                let auth_data = uiaa::AuthData::Password(assign!(
                    uiaa::Password::new(uiaa::UserIdentifier::MatrixId("example"), "wordpass"),
                    { session: info.session.as_deref() }
                ));

                client.delete_devices(devices, Some(auth_data)).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn retry_limit_http_requests() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let config = ClientConfig::default().request_config(RequestConfig::new().retry_limit(3));
        assert!(config.request_config.retry_limit.unwrap() == 3);
        let client = Client::new_with_config(homeserver, config).unwrap();

        let m = mock("POST", "/_matrix/client/r0/login").with_status(501).expect(3).create();

        if client.login("example", "wordpass", None, None).await.is_err() {
            m.assert();
        } else {
            panic!("this request should return an `Err` variant")
        }
    }

    #[tokio::test]
    async fn retry_timeout_http_requests() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        // Keep this timeout small so that the test doesn't take long
        let retry_timeout = Duration::from_secs(5);
        let config = ClientConfig::default()
            .request_config(RequestConfig::new().retry_timeout(retry_timeout));
        assert!(config.request_config.retry_timeout.unwrap() == retry_timeout);
        let client = Client::new_with_config(homeserver, config).unwrap();

        let m =
            mock("POST", "/_matrix/client/r0/login").with_status(501).expect_at_least(2).create();

        if client.login("example", "wordpass", None, None).await.is_err() {
            m.assert();
        } else {
            panic!("this request should return an `Err` variant")
        }
    }

    #[tokio::test]
    async fn no_retry_http_requests() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let config = ClientConfig::default().request_config(RequestConfig::new().disable_retry());
        assert!(config.request_config.retry_limit.unwrap() == 0);
        let client = Client::new_with_config(homeserver, config).unwrap();

        let m = mock("POST", "/_matrix/client/r0/login").with_status(501).create();

        if client.login("example", "wordpass", None, None).await.is_err() {
            m.assert();
        } else {
            panic!("this request should return an `Err` variant")
        }
    }

    #[tokio::test]
    async fn get_media_content() {
        let client = logged_in_client().await;

        let request = MediaRequest {
            media_type: MediaType::Uri(mxc_uri!("mxc://localhost/textfile")),
            format: MediaFormat::File,
        };

        let m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/media/r0/download/localhost/textfile\?.*$".to_string()),
        )
        .with_status(200)
        .with_body("Some very interesting text.")
        .expect(2)
        .create();

        assert!(client.get_media_content(&request, true).await.is_ok());
        assert!(client.get_media_content(&request, true).await.is_ok());
        assert!(client.get_media_content(&request, false).await.is_ok());
        m.assert();
    }

    #[tokio::test]
    async fn get_media_file() {
        let client = logged_in_client().await;

        let event_content = ImageMessageEventContent::plain(
            "filename.jpg".into(),
            mxc_uri!("mxc://example.org/image"),
            Some(Box::new(assign!(ImageInfo::new(), {
                height: Some(uint!(398)),
                width: Some(uint!(394)),
                mimetype: Some("image/jpeg".into()),
                size: Some(uint!(31037)),
            }))),
        );

        let m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/media/r0/download/example%2Eorg/image\?.*$".to_string()),
        )
        .with_status(200)
        .with_body("binaryjpegdata")
        .create();

        assert!(client.get_file(event_content.clone(), true).await.is_ok());
        assert!(client.get_file(event_content.clone(), true).await.is_ok());
        m.assert();

        let m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/media/r0/thumbnail/example%2Eorg/image\?.*$".to_string()),
        )
        .with_status(200)
        .with_body("smallerbinaryjpegdata")
        .create();

        assert!(client
            .get_thumbnail(
                event_content,
                MediaThumbnailSize { method: Method::Scale, width: uint!(100), height: uint!(100) },
                true
            )
            .await
            .is_ok());
        m.assert();
    }

    #[tokio::test]
    async fn whoami() {
        let client = logged_in_client().await;

        let _m = mock("GET", "/_matrix/client/r0/account/whoami")
            .with_status(200)
            .with_body(test_json::WHOAMI.to_string())
            .match_header("authorization", "Bearer 1234")
            .create();

        let user_id = user_id!("@joe:example.org");

        assert_eq!(client.whoami().await.unwrap().user_id, user_id);
    }

    #[tokio::test]
    async fn test_state_event_getting() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();
        let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost"),
            device_id: "DEVICEID".into(),
        };

        let sync = json!({
            "next_batch": "1234",
            "rooms": {
                "join": {
                    "!SVkFJHzfwvuaIEawgC:localhost": {
                        "state": {
                          "events": [
                            {
                              "type": "m.custom.note",
                              "sender": "@example:localhost",
                              "content": {
                                "body": "Note 1",
                              },
                              "state_key": "note.1",
                              "origin_server_ts": 1611853078727u64,
                              "unsigned": {
                                "replaces_state": "$2s9GcbVxbbFS3EZY9vN1zhavaDJnF32cAIGAxi99NuQ",
                                "age": 15458166523u64
                              },
                              "event_id": "$NVCTvrlxodf3ZGjJ6foxepEq8ysSkTq8wG0wKeQBVZg"
                            },
                            {
                              "type": "m.custom.note",
                              "sender": "@example2:localhost",
                              "content": {
                                "body": "Note 2",
                              },
                              "state_key": "note.2",
                              "origin_server_ts": 1611853078727u64,
                              "unsigned": {
                                "replaces_state": "$2s9GcbVxbbFS3EZY9vN1zhavaDJnF32cAIGAxi99NuQ",
                                "age": 15458166523u64
                              },
                              "event_id": "$NVCTvrlxodf3ZGjJ6foxepEq8ysSkTq8wG0wKeQBVZg"
                            },
                            {
                              "type": "m.room.encryption",
                              "sender": "@example:localhost",
                              "content": {
                                "algorithm": "m.megolm.v1.aes-sha2"
                              },
                              "state_key": "",
                              "origin_server_ts": 1586437448151u64,
                              "unsigned": {
                                "age": 40873797099u64
                              },
                              "event_id": "$vyG3wu1QdJSh5gc-09SwjXBXlXo8gS7s4QV_Yxha0Xw"
                            },
                          ]
                        }
                    }
                }
            }
        });

        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .with_body(sync.to_string())
            .create();

        let client = Client::new(homeserver.clone()).unwrap();
        client.restore_login(session.clone()).await.unwrap();

        let room = client.get_joined_room(&room_id);
        assert!(room.is_none());

        client.sync_once(SyncSettings::default()).await.unwrap();

        let room = client.get_joined_room(&room_id).unwrap();

        let state_events = room.get_state_events(EventType::RoomEncryption).await.unwrap();
        assert_eq!(state_events.len(), 1);

        let state_events = room.get_state_events("m.custom.note".into()).await.unwrap();
        assert_eq!(state_events.len(), 2);

        let encryption_event = room
            .get_state_event(EventType::RoomEncryption, "")
            .await
            .unwrap()
            .unwrap()
            .deserialize()
            .unwrap();

        matches::assert_matches!(encryption_event, AnySyncStateEvent::RoomEncryption(_));
    }
}
