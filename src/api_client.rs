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
        let envelope: ApiEnvelope<T> = serde_json::from_slice(&response_body)
            .with_context(|| format!("decode JSON response for {path}"))?;
        if !(200..300).contains(&status) || envelope.code != 0 {
            bail!("{}", envelope.message);
        }
        Ok(envelope.data)
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
        self.send("/events/upsert", event)
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
    use super::{decode_chunked, parse_http_response};

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
