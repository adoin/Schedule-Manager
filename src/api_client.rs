use crate::models::{ApiEnvelope, AuthData, CalendarEvent, Holiday, SyncPullData};
use anyhow::{Context, Result, bail};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::X509;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    token: Option<String>,
    tls: Arc<SslConnector>,
}

#[derive(Debug)]
pub struct ApiResponseError {
    pub status: u16,
    pub code: Option<i32>,
    message: String,
}

impl std::fmt::Display for ApiResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiResponseError {}

fn decode_api_response<T: DeserializeOwned>(status: u16, body: &[u8], path: &str) -> Result<T> {
    // Error envelopes contain an empty object rather than the endpoint's success
    // payload. Check the envelope before deserializing the requested data type.
    let envelope: ApiEnvelope<Value> = match serde_json::from_slice(body) {
        Ok(envelope) => envelope,
        Err(_) if !(200..300).contains(&status) => {
            return Err(ApiResponseError {
                status,
                code: None,
                message: format!("云端服务响应异常（HTTP {status}），请稍后重试"),
            }
            .into());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("云端响应格式无效（{path}，HTTP {status}）"));
        }
    };
    if !(200..300).contains(&status) || envelope.code != 0 {
        return Err(ApiResponseError {
            status,
            code: Some(envelope.code),
            message: if envelope.message.trim().is_empty() {
                format!("云端请求失败（HTTP {status}，错误码 {}）", envelope.code)
            } else {
                envelope.message
            },
        }
        .into());
    }
    serde_json::from_value(envelope.data)
        .with_context(|| format!("云端返回的数据格式无效（{path}）"))
}

impl ApiClient {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Result<Self> {
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token,
            tls: Arc::new(tls_connector()?),
        })
    }

    fn send<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let url = Url::parse(&format!("{}{}", self.base_url, path))
            .with_context(|| format!("invalid API URL for {path}"))?;
        let host = url.host_str().context("API URL is missing a host")?;
        let port = url
            .port_or_known_default()
            .context("API URL is missing a port")?;
        let address = format!("{host}:{port}");
        let tcp = TcpStream::connect(&address).with_context(|| format!("connect {address}"))?;
        tcp.set_read_timeout(Some(Duration::from_secs(20)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(20)))?;

        let mut stream: Box<dyn ReadWrite> = match url.scheme() {
            "https" => Box::new(
                self.tls
                    .connect(host, tcp)
                    .with_context(|| format!("secure connection to {host}"))?,
            ),
            "http" => Box::new(tcp),
            scheme => bail!("unsupported API URL scheme: {scheme}"),
        };

        let body = serde_json::to_vec(body)?;
        let mut target = url.path().to_string();
        if target.is_empty() {
            target.push('/');
        }
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        let default_port =
            (url.scheme() == "https" && port == 443) || (url.scheme() == "http" && port == 80);
        let host_header = if default_port {
            host.to_string()
        } else {
            format!("{host}:{port}")
        };
        let authorization = self
            .token
            .as_ref()
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let headers = format!(
            "POST {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: ScheduleManager/0.1\r\nContent-Type: application/json\r\nAccept: application/json\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes())?;
        stream.write_all(&body)?;
        stream.flush()?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .with_context(|| format!("read {path}"))?;
        let (status, response_body) = parse_http_response(&response)
            .with_context(|| format!("decode HTTP response for {path}"))?;
        decode_api_response(status, &response_body, path)
    }

    pub fn request_code(&self, email: &str) -> Result<()> {
        let _: Value = self.send("/auth/request-code", &json!({"email": email}))?;
        Ok(())
    }

    pub fn register(
        &self,
        email: &str,
        password: &str,
        display_name: &str,
        code: &str,
    ) -> Result<AuthData> {
        self.send(
            "/auth/register",
            &json!({
                "email": email,
                "password": password,
                "displayName": display_name,
                "code": code,
            }),
        )
    }

    pub fn login(&self, email: &str, password: &str) -> Result<AuthData> {
        self.send(
            "/auth/login",
            &json!({"email": email, "password": password}),
        )
    }

    pub fn pull(&self, cursor: i64) -> Result<SyncPullData> {
        self.send("/sync/pull", &json!({"cursor": cursor}))
    }

    pub fn upsert_event(&self, event: &CalendarEvent) -> Result<CalendarEvent> {
        if !event.notification_url.is_empty() {
            let health = self.health()?;
            if health
                .pointer("/capabilities/notificationUrl")
                .and_then(Value::as_bool)
                != Some(true)
            {
                bail!("云端服务尚未支持通知链接，请更新云端服务后重试；本次输入已保留");
            }
        }
        let saved: CalendarEvent = self.send("/events/upsert", event)?;
        if !event.notification_url.is_empty() && saved.notification_url.is_empty() {
            bail!("云端未保存通知链接，本次输入已保留，请检查云端服务版本");
        }
        if !event.notification_url.is_empty()
            && url::Url::parse(&event.notification_url).ok()
                != url::Url::parse(&saved.notification_url).ok()
        {
            bail!("云端返回的通知链接与保存值不一致，本次输入已保留");
        }
        Ok(saved)
    }

    pub fn delete_event(&self, id: &str, base_version: i64) -> Result<CalendarEvent> {
        self.send(
            "/events/delete",
            &json!({"id": id, "baseVersion": base_version}),
        )
    }

    pub fn health(&self) -> Result<Value> {
        self.send("/health", &json!({}))
    }

    pub fn holidays(&self, start_year: i32, end_year: i32) -> Result<Vec<Holiday>> {
        self.send(
            "/holidays/list",
            &json!({"startYear": start_year, "endYear": end_year}),
        )
    }
}

fn tls_connector() -> Result<SslConnector> {
    let mut builder = SslConnector::builder(SslMethod::tls_client())?;
    builder.set_verify(SslVerifyMode::PEER);
    builder.set_alpn_protos(b"\x08http/1.1")?;

    let native = rustls_native_certs::load_native_certs();
    let mut trusted_roots = 0usize;
    for certificate in native.certs {
        if let Ok(certificate) = X509::from_der(certificate.as_ref()) {
            if builder.cert_store_mut().add_cert(certificate).is_ok() {
                trusted_roots += 1;
            }
        }
    }
    if trusted_roots == 0 {
        bail!("no trusted root certificates are available");
    }
    Ok(builder.build())
}

fn parse_http_response(response: &[u8]) -> Result<(u16, Vec<u8>)> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("response headers are incomplete")?;
    let headers = std::str::from_utf8(&response[..header_end])?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("response status is missing")?
        .parse::<u16>()?;
    let body = &response[header_end + 4..];
    let chunked = headers.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    });
    Ok((
        status,
        if chunked {
            decode_chunked(body)?
        } else {
            body.to_vec()
        },
    ))
}

fn decode_chunked(mut input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("chunk size is incomplete")?;
        let size_text = std::str::from_utf8(&input[..line_end])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or(""), 16)?;
        input = &input[line_end + 2..];
        if size == 0 {
            break;
        }
        if input.len() < size + 2 || &input[size..size + 2] != b"\r\n" {
            bail!("chunk body is incomplete");
        }
        output.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{
        ApiClient, ApiResponseError, decode_api_response, decode_chunked, parse_http_response,
    };
    use crate::models::CalendarEvent;

    fn mock_api(
        responses: Vec<serde_json::Value>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let read = socket.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(
                    String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .unwrap()
                        .to_string(),
                );
                let body = serde_json::to_vec(&response).unwrap();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(&body).unwrap();
            }
            requests
        });
        (address, thread)
    }

    #[test]
    fn legacy_cloud_is_rejected_before_writing_a_linked_event() {
        let (base, mock) = mock_api(vec![
            serde_json::json!({"code":0,"message":"ok","data":{"status":"ok"}}),
        ]);
        let now = chrono::Utc::now();
        let mut event = CalendarEvent::draft(now, now + chrono::Duration::minutes(1));
        event.notification_url = "https://example.com/orders/#/food-order".into();
        let error = ApiClient::new(base, None)
            .unwrap()
            .upsert_event(&event)
            .unwrap_err();
        assert!(error.to_string().contains("尚未支持通知链接"));
        assert_eq!(mock.join().unwrap(), ["POST /health HTTP/1.1"]);
    }

    #[test]
    fn linked_event_round_trip_and_missing_link_response_are_checked() {
        let now = chrono::Utc::now();
        let mut event = CalendarEvent::draft(now, now + chrono::Duration::minutes(1));
        event.notification_url = "https://example.com/orders/#/food-order".into();
        for mode in [0, 1, 2] {
            let mut saved = serde_json::to_value(&event).unwrap();
            if mode == 1 {
                saved.as_object_mut().unwrap().remove("notificationUrl");
            } else if mode == 2 {
                saved["notificationUrl"] = serde_json::json!("https://example.com/wrong-link");
            }
            let (base, mock) = mock_api(vec![
                serde_json::json!({"code":0,"message":"ok","data":{"capabilities":{"notificationUrl":true}}}),
                serde_json::json!({"code":0,"message":"ok","data":saved}),
            ]);
            let result = ApiClient::new(base, None).unwrap().upsert_event(&event);
            if mode == 1 {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("云端未保存通知链接")
                );
            } else if mode == 2 {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("通知链接与保存值不一致")
                );
            } else {
                assert_eq!(result.unwrap().notification_url, event.notification_url);
            }
            assert_eq!(
                mock.join().unwrap(),
                ["POST /health HTTP/1.1", "POST /events/upsert HTTP/1.1"]
            );
        }
    }

    #[test]
    fn error_envelope_preserves_auth_conflict_and_validation_messages() {
        for (status, message) in [
            (401, "登录已过期，请重新登录"),
            (409, "此日程已在其他设备更新，请先同步"),
            (400, "通知链接格式无效"),
        ] {
            let body =
                serde_json::to_vec(&serde_json::json!({"code":status,"message":message,"data":{}}))
                    .unwrap();
            let error =
                decode_api_response::<CalendarEvent>(status, &body, "/events/upsert").unwrap_err();
            assert_eq!(error.to_string(), message);
            assert_eq!(
                error.downcast_ref::<ApiResponseError>().unwrap().status,
                status
            );
        }
    }

    #[test]
    fn application_errors_are_checked_even_with_http_success() {
        let error = decode_api_response::<CalendarEvent>(
            200,
            br#"{"code":409,"message":"conflict","data":null}"#,
            "/events/upsert",
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "conflict");
        assert_eq!(
            error.downcast_ref::<ApiResponseError>().unwrap().code,
            Some(409)
        );
    }

    #[test]
    fn invalid_http_error_response_reports_status_without_echoing_body() {
        let error = decode_api_response::<CalendarEvent>(
            502,
            b"<html>upstream error</html>",
            "/events/upsert",
        )
        .unwrap_err();
        assert!(error.to_string().contains("HTTP 502"));
        assert!(!error.to_string().contains("<html>"));
    }

    #[test]
    fn successful_event_response_still_decodes_and_checks_payload() {
        let now = chrono::Utc::now();
        let event = CalendarEvent::draft(now, now + chrono::Duration::minutes(30));
        let body =
            serde_json::to_vec(&serde_json::json!({"code":0,"message":"ok","data":event})).unwrap();
        let decoded: CalendarEvent = decode_api_response(200, &body, "/events/upsert").unwrap();
        assert_eq!(decoded, event);
        assert!(
            decode_api_response::<CalendarEvent>(
                200,
                br#"{"code":0,"message":"ok","data":{}}"#,
                "/events/upsert"
            )
            .is_err()
        );
    }

    #[test]
    fn parses_content_length_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        let (status, body) = parse_http_response(response).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{\"ok\":true}");
    }

    #[test]
    fn decodes_chunked_body() {
        let body = decode_chunked(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").unwrap();
        assert_eq!(body, b"hello world");
    }
}
