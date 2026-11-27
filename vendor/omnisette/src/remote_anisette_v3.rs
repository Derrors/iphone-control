// Implementing the SideStore Anisette v3 protocol

use std::{collections::HashMap, fs, io::Cursor, path::PathBuf, time::Duration};

use crate::adi_proxy::LOOKUP_CLIENT_INFO;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose;
use chrono::{DateTime, SubsecRound, Utc};
use futures_util::{SinkExt, stream::StreamExt};
use log::debug;
use plist::{Data, Dictionary};
use reqwest::{Client, ClientBuilder, RequestBuilder};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

use crate::{AnisetteError, anisette_headers_provider::AnisetteHeadersProvider};

fn plist_to_string<T: serde::Serialize>(value: &T) -> Result<String, plist::Error> {
    plist_to_buf(value).map(|val| String::from_utf8(val).unwrap())
}

fn plist_to_buf<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, plist::Error> {
    let mut buf: Vec<u8> = Vec::new();
    let writer = Cursor::new(&mut buf);
    plist::to_writer_xml(writer, &value)?;
    Ok(buf)
}

fn bin_serialize<S>(x: &[u8], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(x)
}

fn bin_serialize_opt<S>(x: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    x.clone().map(|i| Data::new(i)).serialize(s)
}

fn bin_deserialize_opt<'de, D>(d: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<Data> = Deserialize::deserialize(d)?;
    Ok(s.map(|i| i.into()))
}

fn bin_deserialize_16<'de, D>(d: D) -> Result<[u8; 16], D::Error>
where
    D: Deserializer<'de>,
{
    let s: Data = Deserialize::deserialize(d)?;
    let s: Vec<u8> = s.into();
    // iphone-control: return a deserialization error instead of panicking on a corrupt state.plist.
    s.try_into().map_err(|v: Vec<u8>| {
        serde::de::Error::invalid_length(v.len(), &"16-byte keychain identifier")
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}
fn base64_encode(data: &[u8]) -> String {
    general_purpose::STANDARD.encode(data)
}

// iphone-control: propagate invalid base64 from remote responses instead of panicking.
fn base64_decode(data: &str) -> Result<Vec<u8>, AnisetteError> {
    general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| AnisetteError::Anyhow(anyhow::anyhow!("invalid base64 from anisette server: {e}")))
}

#[derive(Deserialize)]
struct AnisetteClientInfo {
    client_info: String,
    user_agent: String,
}

#[derive(Serialize, Deserialize)]
pub struct AnisetteState {
    #[serde(
        serialize_with = "bin_serialize",
        deserialize_with = "bin_deserialize_16"
    )]
    keychain_identifier: [u8; 16],
    #[serde(
        serialize_with = "bin_serialize_opt",
        deserialize_with = "bin_deserialize_opt"
    )]
    adi_pb: Option<Vec<u8>>,
}

impl Default for AnisetteState {
    fn default() -> Self {
        AnisetteState {
            keychain_identifier: rand::random::<[u8; 16]>(),
            adi_pb: None,
        }
    }
}

impl AnisetteState {
    pub fn new() -> AnisetteState {
        AnisetteState::default()
    }

    pub fn is_provisioned(&self) -> bool {
        self.adi_pb.is_some()
    }

    fn md_lu(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(&self.keychain_identifier);
        hasher.finalize().into()
    }

    fn device_id(&self) -> String {
        Uuid::from_bytes(self.keychain_identifier).to_string()
    }
}
pub struct AnisetteClient {
    client_info: AnisetteClientInfo,
    url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ProvisionBodyData {
    header: Dictionary,
    request: Dictionary,
}

#[derive(Debug)]
pub struct AnisetteData {
    machine_id: String,
    one_time_password: String,
    routing_info: String,
    device_description: String,
    local_user_id: String,
    device_unique_identifier: String,
}

impl AnisetteData {
    pub fn get_headers(&self, serial: String) -> HashMap<String, String> {
        let dt: DateTime<Utc> = Utc::now().round_subsecs(0);

        HashMap::from_iter(
            [
                (
                    "X-Apple-I-Client-Time".to_string(),
                    dt.format("%+").to_string().replace("+00:00", "Z"),
                ),
                ("X-Apple-I-SRL-NO".to_string(), serial),
                ("X-Apple-I-TimeZone".to_string(), "UTC".to_string()),
                ("X-Apple-Locale".to_string(), "en_US".to_string()),
                ("X-Apple-I-MD-RINFO".to_string(), self.routing_info.clone()),
                ("X-Apple-I-MD-LU".to_string(), self.local_user_id.clone()),
                (
                    "X-Mme-Device-Id".to_string(),
                    self.device_unique_identifier.clone(),
                ),
                ("X-Apple-I-MD".to_string(), self.one_time_password.clone()),
                ("X-Apple-I-MD-M".to_string(), self.machine_id.clone()),
                (
                    "X-Mme-Client-Info".to_string(),
                    self.device_description.clone(),
                ),
            ]
            .into_iter(),
        )
    }
}

// iphone-control: bound Anisette HTTP requests so a hung public node fails fast enough to retry
// instead of consuming the caller's entire timeout budget.
const REQUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// iphone-control: bypass HTTP(S)_PROXY only for loopback self-hosted/mock Anisette servers;
// public Anisette and Apple endpoints keep system-proxy passthrough.
fn is_loopback_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(h) = hostport.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn make_reqwest(no_proxy: bool) -> Result<Client, AnisetteError> {
    let mut builder = ClientBuilder::new()
        .connect_timeout(REQUEST_CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .http1_title_case_headers()
        .danger_accept_invalid_certs(true); // TODO: pin the apple certificate
    if no_proxy {
        builder = builder.no_proxy();
    }
    Ok(builder.build()?)
}

impl AnisetteClient {
    pub async fn new(url: String) -> Result<AnisetteClient, AnisetteError> {
        // iphone-control: normalize trailing slash and fail fast on non-2xx /v3/client_info responses.
        let url = url.trim_end_matches('/').to_string();
        let path = format!("{}/v3/client_info", url);
        let http_client = make_reqwest(is_loopback_url(&url))?;
        let client_info = http_client
            .get(path)
            .send()
            .await?
            .error_for_status()?
            .json::<AnisetteClientInfo>()
            .await?;
        Ok(AnisetteClient { client_info, url })
    }

    fn build_apple_request(
        &self,
        state: &AnisetteState,
        builder: RequestBuilder,
        client_info: &str,
    ) -> RequestBuilder {
        let dt: DateTime<Utc> = Utc::now().round_subsecs(0);

        builder
            .header("X-Mme-Client-Info", client_info)
            .header("User-Agent", &self.client_info.user_agent)
            .header("Content-Type", "text/x-xml-plist")
            .header("X-Apple-I-MD-LU", encode_hex(&state.md_lu()))
            .header("X-Mme-Device-Id", state.device_id())
            .header("X-Apple-I-Client-Time", dt.format("%+").to_string())
            .header("X-Apple-I-TimeZone", "UTC")
            .header("X-Apple-Locale", "en_US")
    }

    pub async fn get_headers(&self, state: &AnisetteState) -> Result<AnisetteData, AnisetteError> {
        let path = format!("{}/v3/get_headers", self.url);
        let http_client = make_reqwest(is_loopback_url(&self.url))?;

        #[derive(Serialize)]
        struct GetHeadersBody {
            identifier: String,
            adi_pb: String,
        }
        let body = GetHeadersBody {
            identifier: base64_encode(&state.keychain_identifier),
            adi_pb: base64_encode(
                state
                    .adi_pb
                    .as_ref()
                    .ok_or(AnisetteError::AnisetteNotProvisioned)?,
            ),
        };

        #[derive(Deserialize)]
        #[serde(tag = "result")]
        enum AnisetteHeaders {
            GetHeadersError {
                message: String,
            },
            Headers {
                #[serde(rename = "X-Apple-I-MD-M")]
                machine_id: String,
                #[serde(rename = "X-Apple-I-MD")]
                one_time_password: String,
                #[serde(rename = "X-Apple-I-MD-RINFO")]
                routing_info: String,
            },
        }

        // iphone-control: still parse JSON GetHeadersError on non-2xx responses (so -45061 keeps
        // triggering re-provisioning), surface non-JSON HTTP errors cleanly, and never panic on
        // unexpected server errors.
        let resp = http_client.post(path).json(&body).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        let headers: AnisetteHeaders = match serde_json::from_slice(&bytes) {
            Ok(h) => h,
            Err(e) => {
                if !status.is_success() {
                    let snippet: String =
                        String::from_utf8_lossy(&bytes).chars().take(200).collect();
                    return Err(AnisetteError::Anyhow(anyhow::anyhow!(
                        "Anisette get_headers HTTP {}: {}",
                        status,
                        snippet.trim()
                    )));
                }
                return Err(AnisetteError::SerdeError(e));
            }
        };
        match headers {
            AnisetteHeaders::GetHeadersError { message } => {
                if message.contains("-45061") {
                    Err(AnisetteError::AnisetteNotProvisioned)
                } else {
                    Err(AnisetteError::Anyhow(anyhow::anyhow!(
                        "Anisette get_headers error: {}",
                        message
                    )))
                }
            }
            AnisetteHeaders::Headers {
                machine_id,
                one_time_password,
                routing_info,
            } => Ok(AnisetteData {
                machine_id,
                one_time_password,
                routing_info,
                device_description: self.client_info.client_info.clone(),
                local_user_id: encode_hex(&state.md_lu()),
                device_unique_identifier: state.device_id(),
            }),
        }
    }

    pub async fn provision(&self, state: &mut AnisetteState) -> Result<(), AnisetteError> {
        debug!("Provisioning Anisette");
        let http_client = make_reqwest(false)?;
        let resp = self
            .build_apple_request(
                &state,
                http_client.get("https://gsa.apple.com/grandslam/GsService2/lookup"),
                LOOKUP_CLIENT_INFO,
            )
            .send()
            .await?
            .error_for_status()?;
        let text = resp.text().await?;

        // iphone-control: return structured errors instead of panicking on malformed Apple/server responses.
        let protocol_val = plist::Value::from_reader(Cursor::new(text.as_str()))?;
        let urls = protocol_val
            .as_dictionary()
            .and_then(|d| d.get("urls"))
            .and_then(|v| v.as_dictionary())
            .ok_or_else(|| {
                AnisetteError::Anyhow(anyhow::anyhow!("Apple lookup plist missing urls dictionary"))
            })?;

        let start_provisioning_url = urls
            .get("midStartProvisioning")
            .and_then(|v| v.as_string())
            .ok_or_else(|| {
                AnisetteError::Anyhow(anyhow::anyhow!("Apple lookup plist missing midStartProvisioning"))
            })?;
        let end_provisioning_url = urls
            .get("midFinishProvisioning")
            .and_then(|v| v.as_string())
            .ok_or_else(|| {
                AnisetteError::Anyhow(anyhow::anyhow!("Apple lookup plist missing midFinishProvisioning"))
            })?;
        debug!(
            "Got provisioning urls: {} and {}",
            start_provisioning_url, end_provisioning_url
        );

        let provision_ws_url = if let Some(rest) = self.url.strip_prefix("https://") {
            format!("wss://{rest}/v3/provisioning_session")
        } else if let Some(rest) = self.url.strip_prefix("http://") {
            format!("ws://{rest}/v3/provisioning_session")
        } else {
            format!("{}/v3/provisioning_session", self.url)
        };
        let (mut connection, _) = connect_async(&provision_ws_url).await?;

        #[derive(Deserialize)]
        #[serde(tag = "result")]
        enum ProvisionInput {
            GiveIdentifier,
            GiveStartProvisioningData,
            GiveEndProvisioningData {
                #[allow(dead_code)] // it's not even dead, rust just has problems
                cpim: String,
            },
            ProvisioningSuccess {
                #[allow(dead_code)] // it's not even dead, rust just has problems
                adi_pb: String,
            },
        }

        loop {
            // iphone-control: fail out when the provisioning WebSocket drops or errors instead of
            // spinning forever on None / Err.
            let data = match connection.next().await {
                Some(Ok(data)) => data,
                Some(Err(e)) => return Err(AnisetteError::WsError(e)),
                None => {
                    return Err(AnisetteError::Anyhow(anyhow::anyhow!(
                        "Anisette provisioning socket closed before completion"
                    )));
                }
            };
            if data.is_text() {
                let txt = data.to_text()?;
                let msg: ProvisionInput = serde_json::from_str(txt)?;
                match msg {
                    ProvisionInput::GiveIdentifier => {
                        #[derive(Serialize)]
                        struct Identifier {
                            identifier: String, // base64
                        }
                        let identifier = Identifier {
                            identifier: base64_encode(&state.keychain_identifier),
                        };
                        connection
                            .send(Message::Text(serde_json::to_string(&identifier)?.into()))
                            .await?;
                    }
                    ProvisionInput::GiveStartProvisioningData => {
                        let http_client = make_reqwest(false)?;
                        let body_data = ProvisionBodyData {
                            header: Dictionary::new(),
                            request: Dictionary::new(),
                        };
                        let resp = self
                            .build_apple_request(
                                state,
                                http_client.post(start_provisioning_url),
                                &self.client_info.client_info,
                            )
                            .body(plist_to_string(&body_data)?)
                            .send()
                            .await?
                            .error_for_status()?;
                        let text = resp.text().await?;

                        let protocol_val = plist::Value::from_reader(Cursor::new(text.as_str()))?;
                        let spim = protocol_val
                            .as_dictionary()
                            .and_then(|d| d.get("Response"))
                            .and_then(|v| v.as_dictionary())
                            .and_then(|d| d.get("spim"))
                            .and_then(|v| v.as_string())
                            .ok_or_else(|| {
                                AnisetteError::Anyhow(anyhow::anyhow!(
                                    "Apple start provisioning response missing Response.spim"
                                ))
                            })?;

                        debug!("GiveStartProvisioningData");
                        #[derive(Serialize)]
                        struct Spim {
                            spim: String, // base64
                        }
                        let spim = Spim {
                            spim: spim.to_string(),
                        };
                        connection
                            .send(Message::Text(serde_json::to_string(&spim)?.into()))
                            .await?;
                    }
                    ProvisionInput::GiveEndProvisioningData { cpim } => {
                        let http_client = make_reqwest(false)?;
                        let body_data = ProvisionBodyData {
                            header: Dictionary::new(),
                            request: Dictionary::from_iter([("cpim", cpim)].into_iter()),
                        };
                        let resp = self
                            .build_apple_request(
                                state,
                                http_client.post(end_provisioning_url),
                                &self.client_info.client_info,
                            )
                            .body(plist_to_string(&body_data)?)
                            .send()
                            .await?
                            .error_for_status()?;
                        let text = resp.text().await?;

                        let protocol_val = plist::Value::from_reader(Cursor::new(text.as_str()))?;
                        let response = protocol_val
                            .as_dictionary()
                            .and_then(|d| d.get("Response"))
                            .and_then(|v| v.as_dictionary())
                            .ok_or_else(|| {
                                AnisetteError::Anyhow(anyhow::anyhow!(
                                    "Apple finish provisioning response missing Response dictionary"
                                ))
                            })?;

                        debug!("GiveEndProvisioningData");

                        #[derive(Serialize)]
                        struct EndProvisioning<'t> {
                            ptm: &'t str,
                            tk: &'t str,
                        }
                        let ptm = response
                            .get("ptm")
                            .and_then(|v| v.as_string())
                            .ok_or_else(|| {
                                AnisetteError::Anyhow(anyhow::anyhow!(
                                    "Apple finish provisioning response missing Response.ptm"
                                ))
                            })?;
                        let tk = response
                            .get("tk")
                            .and_then(|v| v.as_string())
                            .ok_or_else(|| {
                                AnisetteError::Anyhow(anyhow::anyhow!(
                                    "Apple finish provisioning response missing Response.tk"
                                ))
                            })?;
                        let end_provisioning = EndProvisioning { ptm, tk };
                        connection
                            .send(Message::Text(
                                serde_json::to_string(&end_provisioning)?.into(),
                            ))
                            .await?;
                    }
                    ProvisionInput::ProvisioningSuccess { adi_pb } => {
                        debug!("ProvisioningSuccess");
                        state.adi_pb = Some(base64_decode(&adi_pb)?);
                        let _ = connection.close(None).await;
                        break;
                    }
                }
            } else if data.is_close() {
                break;
            }
        }

        if !state.is_provisioned() {
            return Err(AnisetteError::Anyhow(anyhow::anyhow!(
                "Anisette provisioning closed before receiving adi_pb"
            )));
        }

        Ok(())
    }
}

pub struct RemoteAnisetteProviderV3 {
    client_url: String,
    client: Option<AnisetteClient>,
    pub state: Option<AnisetteState>,
    configuration_path: PathBuf,
    serial: String,
}

impl RemoteAnisetteProviderV3 {
    pub fn new(
        url: String,
        configuration_path: PathBuf,
        serial: String,
    ) -> RemoteAnisetteProviderV3 {
        RemoteAnisetteProviderV3 {
            client_url: url,
            client: None,
            state: None,
            configuration_path,
            serial,
        }
    }
}

#[async_trait]
impl AnisetteHeadersProvider for RemoteAnisetteProviderV3 {
    async fn get_anisette_headers(
        &mut self,
        _skip_provisioning: bool,
    ) -> Result<HashMap<String, String>, AnisetteError> {
        if self.client.is_none() {
            self.client = Some(AnisetteClient::new(self.client_url.clone()).await?);
        }
        let client = self.client.as_ref().unwrap();

        fs::create_dir_all(&self.configuration_path)?;

        let config_path = self.configuration_path.join("state.plist");
        if self.state.is_none() {
            self.state = Some(if let Ok(text) = plist::from_file(&config_path) {
                text
            } else {
                AnisetteState::new()
            });
        }

        let state = self.state.as_mut().unwrap();
        if !state.is_provisioned() {
            client.provision(state).await?;
            plist::to_file_xml(&config_path, state)?;
        }
        let data = match client.get_headers(&state).await {
            Ok(data) => data,
            Err(err) => {
                if matches!(err, AnisetteError::AnisetteNotProvisioned) {
                    state.adi_pb = None;
                    client.provision(state).await?;
                    plist::to_file_xml(config_path, state)?;
                    client.get_headers(&state).await?
                } else {
                    // iphone-control: propagate network/server errors instead of panicking the thread.
                    return Err(err);
                }
            }
        };
        Ok(data.get_headers(self.serial.clone()))
    }
}

#[cfg(test)]
mod tests {
    use crate::anisette_headers_provider::AnisetteHeadersProvider;
    use crate::remote_anisette_v3::RemoteAnisetteProviderV3;
    use crate::{AnisetteError, DEFAULT_ANISETTE_URL_V3};
    use log::info;

    #[tokio::test]
    async fn fetch_anisette_remote_v3() -> Result<(), AnisetteError> {
        crate::tests::init_logger();

        let mut provider = RemoteAnisetteProviderV3::new(
            DEFAULT_ANISETTE_URL_V3.to_string(),
            "anisette_test".into(),
            "0".to_string(),
        );
        info!(
            "Remote headers: {:?}",
            (&mut provider as &mut dyn AnisetteHeadersProvider)
                .get_authentication_headers()
                .await?
        );
        Ok(())
    }
}
