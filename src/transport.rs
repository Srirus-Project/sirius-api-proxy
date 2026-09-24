//! Explicit per-region CONNECT transport beneath the game's verified TLS/HTTP2 layer.
use crate::{
    config::{secret, UpstreamConfig},
    error::AppError,
};
use hyper::Uri;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder, MaybeHttpsStream};
use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioIo};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tower_service::Service;

type Stream = MaybeHttpsStream<TokioIo<TcpStream>>;
#[derive(Clone)]
struct Proxy {
    uri: Uri,
    authorization: Option<String>,
}
#[derive(Clone)]
pub(crate) struct Connector {
    socket: HttpsConnector<HttpConnector>,
    proxy: Option<Proxy>,
    timeout: Duration,
}
impl Connector {
    pub(crate) fn new(config: &UpstreamConfig) -> Result<Self, AppError> {
        config.validate()?;
        let proxy = config
            .proxy_url_env
            .as_ref()
            .map(|name| {
                let value = secret(name)?;
                let uri = proxy_uri(&value)?;
                let authorization = config
                    .proxy_authorization_env
                    .as_ref()
                    .map(|name| secret(name))
                    .transpose()?;
                if authorization.as_ref().is_some_and(|v| v.len() > 4096) {
                    return Err(AppError::Config("proxy authorization exceeds limit"));
                }
                Ok(Proxy { uri, authorization })
            })
            .transpose()?;
        let mut socket = HttpConnector::new();
        socket.enforce_http(false);
        socket.set_connect_timeout(Some(Duration::from_millis(config.connect_timeout_ms)));
        let socket = HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
            .map_err(|_| AppError::Config("TLS provider initialization failed"))?
            .https_or_http()
            .enable_http1()
            .wrap_connector(socket);
        Ok(Self {
            socket,
            proxy,
            timeout: Duration::from_millis(config.connect_timeout_ms),
        })
    }
}
pub(crate) fn proxy_uri(value: &str) -> Result<Uri, AppError> {
    let invalid = || AppError::Config("proxy URL must be an HTTP(S) origin without credentials");
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    if value.len() > 4096
        || value
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || value.contains('@')
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || value.contains('\\')
    {
        return Err(invalid());
    }
    url.as_str().parse().map_err(|_| invalid())
}
impl Service<Uri> for Connector {
    type Response = Stream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Stream, io::Error>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, uri: Uri) -> Self::Future {
        let mut socket = self.socket.clone();
        let proxy = self.proxy.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            let work = async {
                let authority = target_authority(&uri)?;
                // Outer hyper-rustls applies origin TLS after this connector returns.
                let direct: Uri = format!("http://{authority}/")
                    .parse()
                    .map_err(|_| transport_error())?;
                let stream = socket
                    .call(proxy.as_ref().map_or(direct, |p| p.uri.clone()))
                    .await
                    .map_err(|_| transport_error())?;
                let Some(proxy) = proxy else {
                    return Ok(stream);
                };
                let mut io = TokioIo::new(stream);
                let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
                if let Some(auth) = proxy.authorization {
                    request.push_str("Proxy-Authorization: ");
                    request.push_str(&auth);
                    request.push_str("\r\n");
                }
                request.push_str("\r\n");
                io.write_all(request.as_bytes())
                    .await
                    .map_err(|_| transport_error())?;
                io.flush().await.map_err(|_| transport_error())?;
                let mut header = Vec::new();
                let mut total = 0usize;
                let mut informational = 0;
                loop {
                    if total >= 16 * 1024 {
                        return Err(io::Error::other(ProxyRejected));
                    }
                    // Read exactly through the header terminator; never consume tunnel bytes.
                    let byte = io.read_u8().await.map_err(|_| transport_error())?;
                    total += 1;
                    header.push(byte);
                    if !header.ends_with(b"\r\n\r\n") {
                        continue;
                    }
                    let mut headers = [httparse::EMPTY_HEADER; 128];
                    let mut response = httparse::Response::new(&mut headers);
                    if !matches!(response.parse(&header), Ok(httparse::Status::Complete(n)) if n == header.len())
                    {
                        return Err(io::Error::other(ProxyRejected));
                    }
                    match response.code {
                        Some(200..=299) => return Ok(io.into_inner()),
                        Some(100 | 102..=199) if informational < 4 => {
                            informational += 1;
                            header.clear();
                        }
                        _ => return Err(io::Error::other(ProxyRejected)),
                    }
                }
            };
            tokio::time::timeout(timeout, work).await.map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "upstream connection timed out")
            })?
        })
    }
}
fn target_authority(uri: &Uri) -> Result<String, io::Error> {
    let host = uri.host().ok_or_else(transport_error)?;
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    Ok(format!("{host}:{port}"))
}
fn transport_error() -> io::Error {
    io::Error::other("upstream connection or proxy tunnel failed")
}

#[derive(Debug, thiserror::Error)]
#[error("upstream proxy rejected connection")]
struct ProxyRejected;
pub(crate) fn classify(error: hyper_util::client::legacy::Error) -> AppError {
    let mut current: &(dyn std::error::Error + 'static) = &error;
    loop {
        if current.is::<ProxyRejected>()
            || current
                .downcast_ref::<io::Error>()
                .and_then(|e| e.get_ref())
                .is_some_and(|e| e.is::<ProxyRejected>())
        {
            return AppError::Proxy;
        }
        match current.source() {
            Some(source) => current = source,
            None => return AppError::Transport,
        }
    }
}

/// Bound DNS, proxy TLS/CONNECT and origin TLS together, including pooled connects.
#[derive(Clone)]
pub(crate) struct TlsConnector {
    inner: HttpsConnector<Connector>,
    timeout: Duration,
}
impl TlsConnector {
    pub(crate) fn new(inner: HttpsConnector<Connector>, milliseconds: u64) -> Self {
        Self {
            inner,
            timeout: Duration::from_millis(milliseconds),
        }
    }
}
impl Service<Uri> for TlsConnector {
    type Response = MaybeHttpsStream<Stream>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, uri: Uri) -> Self::Future {
        let connect = self.inner.call(uri);
        let timeout = self.timeout;
        Box::pin(async move {
            tokio::time::timeout(timeout, connect).await.map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "upstream TLS connection timed out")
            })?
        })
    }
}
