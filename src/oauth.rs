use std::{
	collections::HashMap,
	fmt,
	sync::{atomic::Ordering, LazyLock, Mutex},
	time::{Duration, SystemTime},
};

use crate::{
	client::{send_reddit_transport_request, RedditTransport, OAUTH_CLIENT, OAUTH_IS_ROLLING_OVER, OAUTH_RATELIMIT_REMAINING},
	oauth_resources::ANDROID_APP_VERSION_LIST,
};
use base64::{engine::general_purpose, Engine as _};
use hyper::{Body, Method, StatusCode};
use log::{error, info, trace, warn};
use serde_json::json;
use tegen::tegen::TextGenerator;
use tokio::time::timeout;

const REDDIT_ANDROID_OAUTH_CLIENT_ID: &str = "ohXpoqrZYub1kg";
const REDDIT_GENERIC_WEB_OAUTH_HEADER: &str = "Basic M1hmQkpXbGlIdnFBQ25YcmZJWWxMdzo=";

const AUTH_ENDPOINT: &str = "https://www.reddit.com";

const OAUTH_TIMEOUT: Duration = Duration::from_secs(5);
const OAUTH_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const OAUTH_REFRESH_HEADROOM_SECS: u64 = 120;
const OAUTH_MIN_REFRESH_WAIT_SECS: u64 = 30;

const GENERIC_WEB_USER_AGENTS: &[&str] = &[
	"Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:145.0) Gecko/20100101 Firefox/145.0",
	"Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0",
	"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
];

#[derive(Debug, Clone, Default)]
pub struct OAuthStatusSnapshot {
	pub available: bool,
	pub last_error: Option<String>,
	pub last_attempt_at: Option<SystemTime>,
	pub last_success_at: Option<SystemTime>,
	pub active_backend: Option<String>,
	pub active_transport: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct OAuthStatus {
	available: bool,
	last_error: Option<String>,
	last_attempt_at: Option<SystemTime>,
	last_success_at: Option<SystemTime>,
	active_backend: Option<String>,
	active_transport: Option<String>,
}

static OAUTH_STATUS: LazyLock<Mutex<OAuthStatus>> = LazyLock::new(|| {
	Mutex::new(OAuthStatus {
		available: false,
		last_error: Some("OAuth initialization pending".to_string()),
		last_attempt_at: None,
		last_success_at: None,
		active_backend: None,
		active_transport: None,
	})
});

// Response from OAuth backend authentication
#[derive(Debug, Clone)]
pub struct OauthResponse {
	pub token: String,
	pub expires_in: u64,
	pub transport: RedditTransport,
}

// Trait for OAuth backend implementations
trait OauthBackend: Send + Sync {
	fn authenticate(&mut self) -> impl std::future::Future<Output = Result<OauthResponse, AuthError>> + Send;
	fn user_agent(&self) -> &str;
	fn get_headers(&self) -> HashMap<String, String>;
}

// OAuth backend implementations
#[derive(Debug, Clone)]
pub(crate) enum OauthBackendImpl {
	MobileSpoof(MobileSpoofAuth),
	GenericWeb(GenericWebAuth),
}

impl OauthBackendImpl {
	fn name(&self) -> &'static str {
		match self {
			OauthBackendImpl::MobileSpoof(_) => "mobile_spoof",
			OauthBackendImpl::GenericWeb(_) => "generic_web",
		}
	}

	fn fresh_generic_web() -> Self {
		OauthBackendImpl::GenericWeb(GenericWebAuth::new())
	}

	fn fresh_mobile_spoof() -> Self {
		OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new())
	}

	fn alternate_fresh(&self) -> Self {
		match self {
			OauthBackendImpl::GenericWeb(_) => Self::fresh_mobile_spoof(),
			OauthBackendImpl::MobileSpoof(_) => Self::fresh_generic_web(),
		}
	}
}

impl OauthBackend for OauthBackendImpl {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.authenticate().await,
			OauthBackendImpl::GenericWeb(backend) => backend.authenticate().await,
		}
	}

	fn user_agent(&self) -> &str {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.user_agent(),
			OauthBackendImpl::GenericWeb(backend) => backend.user_agent(),
		}
	}

	fn get_headers(&self) -> HashMap<String, String> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.get_headers(),
			OauthBackendImpl::GenericWeb(backend) => backend.get_headers(),
		}
	}
}

#[derive(Debug, Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	expires_in: u64,
	pub(crate) backend: OauthBackendImpl,
	available: bool,
	transport: RedditTransport,
}

impl Oauth {
	pub(crate) async fn try_new() -> Result<Self, AuthError> {
		let mut primary_error = None;
		for backend in [OauthBackendImpl::fresh_generic_web(), OauthBackendImpl::fresh_mobile_spoof()] {
			match Self::try_new_with_backend(backend.clone()).await {
				Ok(oauth) => return Ok(oauth),
				Err(error) => {
					warn!("[⛔] Failed to create OAuth client using {}: {}", backend.name(), error);
					primary_error = Some(error);
				}
			}
		}

		Err(primary_error.unwrap_or_else(|| AuthError::Message("no OAuth backends available".to_string())))
	}

	pub(crate) async fn try_new_with_backend(mut backend: OauthBackendImpl) -> Result<Self, AuthError> {
		let response = timeout(OAUTH_TIMEOUT, backend.authenticate()).await.map_err(|_| AuthError::Timeout)??;

		let mut headers_map = backend.get_headers();
		headers_map.insert("Authorization".to_owned(), format!("Bearer {}", response.token));

		Ok(Self {
			headers_map,
			expires_in: response.expires_in,
			backend,
			available: true,
			transport: response.transport,
		})
	}

	pub(crate) fn unavailable(_reason: &str) -> Self {
		Self {
			headers_map: HashMap::new(),
			expires_in: OAUTH_RETRY_INTERVAL.as_secs(),
			backend: OauthBackendImpl::GenericWeb(GenericWebAuth::placeholder()),
			available: false,
			transport: RedditTransport::Hyper,
		}
	}

	pub fn user_agent(&self) -> &str {
		self.backend.user_agent()
	}

	pub fn is_available(&self) -> bool {
		self.available
	}

	pub(crate) fn expires_in(&self) -> u64 {
		self.expires_in
	}

	fn transport_name(&self) -> &'static str {
		match self.transport {
			RedditTransport::Wreq => "wreq",
			RedditTransport::Hyper => "hyper",
		}
	}
}

#[derive(Debug)]
pub(crate) enum AuthError {
	Hyper(hyper::Error),
	SerdeDeserialize(serde_json::Error),
	Field((serde_json::Value, &'static str)),
	Timeout,
	Message(String),
}

impl fmt::Display for AuthError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			AuthError::Hyper(error) => write!(f, "{error}"),
			AuthError::SerdeDeserialize(error) => write!(f, "{error}"),
			AuthError::Field((value, field)) => write!(f, "{field}: {value}"),
			AuthError::Timeout => write!(f, "timed out while creating OAuth client"),
			AuthError::Message(message) => write!(f, "{message}"),
		}
	}
}

impl From<hyper::Error> for AuthError {
	fn from(err: hyper::Error) -> Self {
		AuthError::Hyper(err)
	}
}

impl From<serde_json::Error> for AuthError {
	fn from(err: serde_json::Error) -> Self {
		AuthError::SerdeDeserialize(err)
	}
}

pub fn oauth_status_snapshot() -> OAuthStatusSnapshot {
	OAUTH_STATUS
		.lock()
		.map(|status| OAuthStatusSnapshot {
			available: status.available,
			last_error: status.last_error.clone(),
			last_attempt_at: status.last_attempt_at,
			last_success_at: status.last_success_at,
			active_backend: status.active_backend.clone(),
			active_transport: status.active_transport.clone(),
		})
		.unwrap_or_default()
}

pub fn oauth_is_available() -> bool {
	oauth_status_snapshot().available
}

fn update_oauth_status<F>(mutator: F)
where
	F: FnOnce(&mut OAuthStatus),
{
	if let Ok(mut status) = OAUTH_STATUS.lock() {
		mutator(&mut status);
	}
}

fn mark_oauth_attempt(backend: &str) {
	update_oauth_status(|status| {
		status.last_attempt_at = Some(SystemTime::now());
		status.active_backend = Some(backend.to_string());
	});
}

fn mark_oauth_success(oauth: &Oauth) {
	update_oauth_status(|status| {
		status.available = true;
		status.last_error = None;
		status.last_success_at = Some(SystemTime::now());
		status.active_backend = Some(oauth.backend.name().to_string());
		status.active_transport = Some(oauth.transport_name().to_string());
	});
}

fn mark_oauth_failure(backend: &str, error: &str, available: bool) {
	update_oauth_status(|status| {
		status.available = available;
		status.last_error = Some(error.to_string());
		status.active_backend = Some(backend.to_string());
		status.active_transport = Some("hyper".to_string());
	});
}

async fn try_refresh_from_existing(current: &Oauth) -> Result<Oauth, AuthError> {
	let mut errors = Vec::new();

	mark_oauth_attempt(current.backend.name());
	match Oauth::try_new_with_backend(current.backend.clone()).await {
		Ok(oauth) => return Ok(oauth),
		Err(error) => errors.push(format!("{}: {}", current.backend.name(), error)),
	}

	let fallback = current.backend.alternate_fresh();
	mark_oauth_attempt(fallback.name());
	match Oauth::try_new_with_backend(fallback.clone()).await {
		Ok(oauth) => Ok(oauth),
		Err(error) => {
			errors.push(format!("{}: {}", fallback.name(), error));
			Err(AuthError::Message(errors.join("; ")))
		}
	}
}

fn oauth_maintenance_delay(current: &Oauth) -> Duration {
	if !current.is_available() {
		return OAUTH_RETRY_INTERVAL + Duration::from_secs(fastrand::u64(0..=5));
	}

	let refresh_secs = current.expires_in().saturating_sub(OAUTH_REFRESH_HEADROOM_SECS).max(OAUTH_MIN_REFRESH_WAIT_SECS);
	Duration::from_secs(refresh_secs)
}

pub fn spawn_oauth_maintenance() {
	tokio::spawn(async {
		force_refresh_token().await;

		loop {
			let wait_duration = {
				let current = OAUTH_CLIENT.load_full();
				oauth_maintenance_delay(&current)
			};
			trace!("Next OAuth maintenance run in {:?}", wait_duration);
			tokio::time::sleep(wait_duration).await;
			force_refresh_token().await;
		}
	});
}

pub async fn force_refresh_token() {
	if OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		trace!("Skipping refresh token roll over, already in progress");
		return;
	}

	trace!("Rolling over refresh token. Current rate limit: {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst));

	let previous = OAUTH_CLIENT.load_full();
	let refresh_result = if previous.is_available() {
		try_refresh_from_existing(&previous).await
	} else {
		mark_oauth_attempt("generic_web");
		Oauth::try_new().await
	};

	match refresh_result {
		Ok(new_client) => {
			info!("[✅] Successfully created OAuth client using {}", new_client.backend.name());
			mark_oauth_success(&new_client);
			OAUTH_CLIENT.swap(new_client.into());
			OAUTH_RATELIMIT_REMAINING.store(99, Ordering::SeqCst);
		}
		Err(error) => {
			let error_message = error.to_string();
			error!("[⛔] Failed to refresh OAuth client: {error_message}");
			if previous.is_available() {
				mark_oauth_failure(previous.backend.name(), &error_message, true);
				warn!("Keeping previous OAuth client while background refresh retries continue");
			} else {
				mark_oauth_failure(previous.backend.name(), &error_message, false);
				OAUTH_CLIENT.swap(Oauth::unavailable(&error_message).into());
			}
		}
	}

	OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
}

#[derive(Debug, Clone, Default)]
struct Device {
	oauth_id: String,
	initial_headers: HashMap<String, String>,
	headers: HashMap<String, String>,
	user_agent: String,
	loid: Option<String>,
	session: Option<String>,
}

impl Device {
	fn android() -> Self {
		let uuid = uuid::Uuid::new_v4().to_string();
		let android_app_version = choose(ANDROID_APP_VERSION_LIST).to_string();
		let android_version = fastrand::u8(10..=16);
		let android_user_agent = format!("Reddit/{android_app_version}/Android {android_version}");
		let qos = format!("{:.3}", fastrand::u32(1000..=100_000) as f32 / 1000.0);
		let codecs = TextGenerator::new().generate("available-codecs=video/avc, video/hevc{, video/x-vnd.on2.vp9|}");

		let headers: HashMap<String, String> = HashMap::from([
			("User-Agent".into(), android_user_agent.clone()),
			("x-reddit-retry".into(), "algo=no-retries".into()),
			("x-reddit-compression".into(), "1".into()),
			("x-reddit-qos".into(), qos),
			("x-reddit-media-codecs".into(), codecs),
			("Content-Type".into(), "application/json; charset=UTF-8".into()),
			("client-vendor-id".into(), uuid.clone()),
			("X-Reddit-Device-Id".into(), uuid.clone()),
		]);

		info!("[🔄] Spoofing Android client with headers: {headers:?}, uuid: \"{uuid}\", and OAuth ID \"{REDDIT_ANDROID_OAUTH_CLIENT_ID}\"");

		Self {
			oauth_id: REDDIT_ANDROID_OAUTH_CLIENT_ID.to_string(),
			headers: headers.clone(),
			initial_headers: headers,
			user_agent: android_user_agent,
			loid: None,
			session: None,
		}
	}

	fn new() -> Self {
		Self::android()
	}

	fn update_persisted_headers(&mut self, headers: &hyper::HeaderMap) {
		if let Some(header) = headers.get("x-reddit-loid").and_then(|header| header.to_str().ok()) {
			self.loid = Some(header.to_string());
		}
		if let Some(header) = headers.get("x-reddit-session").and_then(|header| header.to_str().ok()) {
			self.session = Some(header.to_string());
		}
	}

	fn merged_headers(&self) -> HashMap<String, String> {
		let mut headers = self.headers.clone();
		if let Some(loid) = &self.loid {
			headers.insert("x-reddit-loid".to_owned(), loid.clone());
		}
		if let Some(session) = &self.session {
			headers.insert("x-reddit-session".to_owned(), session.clone());
		}
		headers
	}
}

#[derive(Debug, Clone)]
pub struct MobileSpoofAuth {
	device: Device,
}

impl MobileSpoofAuth {
	fn new() -> Self {
		Self { device: Device::new() }
	}
}

impl OauthBackend for MobileSpoofAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		let url = format!("{AUTH_ENDPOINT}/auth/v2/oauth/access-token/loid");
		let auth = general_purpose::STANDARD.encode(format!("{}:", self.device.oauth_id));
		let json = json!({
			"scopes": ["*","email", "pii"]
		});
		let mut headers = self.device.initial_headers.iter().map(|(key, value)| (key.clone(), value.clone())).collect::<Vec<_>>();
		if let Some(loid) = &self.device.loid {
			headers.push(("x-reddit-loid".to_string(), loid.clone()));
		}
		if let Some(session) = &self.device.session {
			headers.push(("x-reddit-session".to_string(), session.clone()));
		}
		headers.push(("Authorization".to_string(), format!("Basic {auth}")));

		trace!("Sending MobileSpoofAuth token request to {url}");

		let (response, transport) = send_reddit_transport_request(&Method::POST, &url, &headers, json.to_string().into_bytes())
			.await
			.map_err(AuthError::Message)?;

		trace!(
			"Received mobile auth response with status {} and length {:?}",
			response.status(),
			response.headers().get("content-length")
		);
		self.device.update_persisted_headers(response.headers());

		parse_oauth_response(response, transport).await
	}

	fn user_agent(&self) -> &str {
		&self.device.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		self.device.merged_headers()
	}
}

#[derive(Debug, Clone)]
pub struct GenericWebAuth {
	device_id: String,
	user_agent: String,
	loid: Option<String>,
	session: Option<String>,
}

impl GenericWebAuth {
	fn new() -> Self {
		let device_id: String = (0..20)
			.map(|_| {
				let idx = fastrand::usize(..62);
				let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
				chars[idx] as char
			})
			.collect();
		let user_agent = choose(GENERIC_WEB_USER_AGENTS).to_string();

		info!("[🔄] Using GenericWebAuth with device_id: \"{device_id}\" and user agent: {user_agent}");

		Self {
			device_id,
			user_agent,
			loid: None,
			session: None,
		}
	}

	fn placeholder() -> Self {
		Self {
			device_id: "redlib-placeholder-device".to_string(),
			user_agent: GENERIC_WEB_USER_AGENTS[0].to_string(),
			loid: None,
			session: None,
		}
	}
}

impl OauthBackend for GenericWebAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		let url = "https://www.reddit.com/api/v1/access_token";
		let body = format!("grant_type=https%3A%2F%2Foauth.reddit.com%2Fgrants%2Finstalled_client&device_id={}", self.device_id);
		let mut headers = vec![
			("Host".to_string(), "www.reddit.com".to_string()),
			("User-Agent".to_string(), self.user_agent.clone()),
			("Accept".to_string(), "*/*".to_string()),
			("Accept-Language".to_string(), "en-US,en;q=0.5".to_string()),
			("Authorization".to_string(), REDDIT_GENERIC_WEB_OAUTH_HEADER.to_string()),
			("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
			("Sec-GPC".to_string(), "1".to_string()),
			("Connection".to_string(), "keep-alive".to_string()),
		];
		if let Some(loid) = &self.loid {
			headers.push(("x-reddit-loid".to_string(), loid.clone()));
		}
		if let Some(session) = &self.session {
			headers.push(("x-reddit-session".to_string(), session.clone()));
		}

		trace!("Sending GenericWebAuth token request to {url}");

		let (response, transport) = send_reddit_transport_request(&Method::POST, url, &headers, body.into_bytes())
			.await
			.map_err(AuthError::Message)?;

		trace!(
			"Received generic web auth response with status {} and length {:?}",
			response.status(),
			response.headers().get("content-length")
		);
		if let Some(header) = response.headers().get("x-reddit-loid").and_then(|header| header.to_str().ok()) {
			self.loid = Some(header.to_string());
		}
		if let Some(header) = response.headers().get("x-reddit-session").and_then(|header| header.to_str().ok()) {
			self.session = Some(header.to_string());
		}

		parse_oauth_response(response, transport).await
	}

	fn user_agent(&self) -> &str {
		&self.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		let mut headers = HashMap::from([
			("Origin".to_owned(), "https://www.reddit.com".to_owned()),
			("User-Agent".to_owned(), self.user_agent.clone()),
		]);
		if let Some(loid) = &self.loid {
			headers.insert("x-reddit-loid".to_owned(), loid.clone());
		}
		if let Some(session) = &self.session {
			headers.insert("x-reddit-session".to_owned(), session.clone());
		}
		headers
	}
}

async fn parse_oauth_response(response: hyper::Response<Body>, transport: RedditTransport) -> Result<OauthResponse, AuthError> {
	let status = response.status();
	let body_bytes = hyper::body::to_bytes(response.into_body()).await?;
	if status == StatusCode::TOO_MANY_REQUESTS {
		return Err(AuthError::Message("rate limited while creating OAuth client".to_string()));
	}

	let json: serde_json::Value = serde_json::from_slice(&body_bytes).map_err(AuthError::SerdeDeserialize)?;
	if !status.is_success() {
		return Err(AuthError::Message(format!("OAuth endpoint returned {status}: {json}")));
	}

	let token = json
		.get("access_token")
		.ok_or_else(|| AuthError::Field((json.clone(), "access_token")))?
		.as_str()
		.ok_or_else(|| AuthError::Field((json.clone(), "access_token: as_str")))?
		.to_string();
	let expires_in = json
		.get("expires_in")
		.ok_or_else(|| AuthError::Field((json.clone(), "expires_in")))?
		.as_u64()
		.ok_or_else(|| AuthError::Field((json.clone(), "expires_in: as_u64")))?;

	info!("[✅] Retrieved OAuth token \"{}...\", expires in {} seconds", &token[..32.min(token.len())], expires_in);

	Ok(OauthResponse { token, expires_in, transport })
}

fn choose<T: Copy>(list: &[T]) -> T {
	*fastrand::choose_multiple(list.iter(), 1)[0]
}

#[cfg(test)]
async fn ensure_oauth_client_available() {
	if !oauth_is_available() {
		force_refresh_token().await;
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mobile_spoof_backend() {
	let mut backend = MobileSpoofAuth::new();
	let response = backend.authenticate().await;
	assert!(response.is_ok());
	let response = response.unwrap();
	assert!(!response.token.is_empty());
	assert!(response.expires_in > 0);
	assert!(!backend.user_agent().is_empty());
	assert!(!backend.get_headers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_generic_web_backend() {
	let mut backend = GenericWebAuth::new();
	let response = backend.authenticate().await;
	assert!(response.is_ok());
	let response = response.unwrap();
	assert!(!response.token.is_empty());
	assert!(response.expires_in > 0);
	assert!(!backend.user_agent().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_oauth_client() {
	ensure_oauth_client_available().await;
	assert!(OAUTH_CLIENT.load_full().headers_map.contains_key("Authorization"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_oauth_client_refresh() {
	force_refresh_token().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_oauth_token_exists() {
	ensure_oauth_client_available().await;
	let client = OAUTH_CLIENT.load_full();
	let auth_header = client.headers_map.get("Authorization").unwrap();
	assert!(auth_header.starts_with("Bearer "));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_oauth_headers_len() {
	ensure_oauth_client_available().await;
	assert!(OAUTH_CLIENT.load_full().headers_map.len() >= 3);
}

#[test]
fn test_creating_device() {
	Device::new();
}

#[test]
fn test_creating_backends() {
	MobileSpoofAuth::new();
	GenericWebAuth::new();
}
