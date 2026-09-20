//! Connect-over-HTTP/1.1 transport.
//!
//! Every RPC is `POST {base_url}/sdk.v1.<Service>/<Method>` carrying a binary
//! protobuf body and `Authorization: Bearer <token>` — on unary calls *and* on
//! streams. The bridge serves HTTP/1.1 only, so a classic gRPC client cannot
//! talk to it; this module implements the small part of the
//! [Connect protocol](https://connectrpc.com/docs/protocol) the contract uses.

use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::io::StreamReader;

use crate::error::{rpc_error_from_connect, Error, Result};

/// Connect envelope flag marking the end-of-stream frame.
const FLAG_END_STREAM: u8 = 0b0000_0010;
/// Connect envelope flag marking a compressed payload.
const FLAG_COMPRESSED: u8 = 0b0000_0001;

const HEADER_PROTOCOL_VERSION: HeaderName = HeaderName::from_static("connect-protocol-version");
const HEADER_ACCEPT_ENCODING: HeaderName = HeaderName::from_static("connect-accept-encoding");

/// An authenticated Connect client for one bridge endpoint.
#[derive(Clone)]
pub(crate) struct Transport {
    http: HyperClient<HttpConnector, Full<Bytes>>,
    base_url: String,
    authorization: HeaderValue,
    request_timeout: Duration,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The bearer token must never reach a log line.
        formatter
            .debug_struct("Transport")
            .field("base_url", &self.base_url)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl Transport {
    pub(crate) fn new(base_url: &str, token: &str, request_timeout: Duration) -> Result<Self> {
        let mut authorization = HeaderValue::try_from(format!("Bearer {token}")).map_err(|_| {
            Error::Config("the bridge auth token is not a valid header value".into())
        })?;
        authorization.set_sensitive(true);

        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        // The bridge is always loopback; a long connect timeout only delays
        // reporting a bridge that died.
        connector.set_connect_timeout(Some(Duration::from_secs(10)));

        Ok(Self {
            http: HyperClient::builder(TokioExecutor::new()).build(connector),
            base_url: base_url.trim_end_matches('/').to_string(),
            authorization,
            request_timeout,
        })
    }

    /// The endpoint this transport talks to, for diagnostics and for handing
    /// to another process that wants to attach to the same bridge.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    fn build(
        &self,
        service: &str,
        method: &str,
        content_type: &'static str,
        body: Bytes,
    ) -> Result<Request<Full<Bytes>>> {
        let uri = format!(
            "{}/{}.{service}/{method}",
            self.base_url,
            crate::proto::PACKAGE
        );
        Request::builder()
            .method("POST")
            .uri(&uri)
            .header(hyper::header::CONTENT_TYPE, content_type)
            .header(hyper::header::AUTHORIZATION, self.authorization.clone())
            .header(HEADER_PROTOCOL_VERSION, "1")
            // We do not decompress payloads, so ask for none.
            .header(hyper::header::ACCEPT_ENCODING, "identity")
            .header(HEADER_ACCEPT_ENCODING, "identity")
            .body(Full::new(body))
            .map_err(|source| {
                Error::transport(format!("could not build a request for {uri}: {source}"))
            })
    }

    /// A unary RPC: one POST, protobuf in, protobuf out.
    ///
    /// Connect reports unary errors as a non-200 status with a JSON body.
    pub(crate) async fn unary<Req, Res>(
        &self,
        service: &str,
        method: &str,
        request: &Req,
    ) -> Result<Res>
    where
        Req: Message,
        Res: Message + Default,
    {
        let rpc = format!("{service}/{method}");
        let http_request = self.build(
            service,
            method,
            "application/proto",
            Bytes::from(request.encode_to_vec()),
        )?;

        let response = tokio::time::timeout(self.request_timeout, self.http.request(http_request))
            .await
            .map_err(|_| Error::Timeout {
                operation: "an RPC to the bridge",
                timeout: self.request_timeout,
            })?
            .map_err(|source| Error::transport(format!("{rpc}: {source}")))?;

        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|source| {
                Error::transport(format!("{rpc}: reading the response failed: {source}"))
            })?
            .to_bytes();

        if status != StatusCode::OK {
            return Err(rpc_error_from_connect(&body, Some(status.as_u16()), &rpc).into());
        }
        Res::decode(body).map_err(|source| Error::decode("response", source))
    }

    /// A server-streaming RPC.
    ///
    /// The returned [`ServerStream`] yields decoded messages until the bridge
    /// sends the end-of-stream frame. The response is not read eagerly, so a
    /// caller can start consuming events while the run is still executing.
    pub(crate) async fn server_stream<Req, Res>(
        &self,
        service: &str,
        method: &str,
        request: &Req,
    ) -> Result<ServerStream<Res>>
    where
        Req: Message,
        Res: Message + Default,
    {
        let rpc = format!("{service}/{method}");
        let payload = request.encode_to_vec();
        let mut body = Vec::with_capacity(payload.len() + 5);
        body.push(0);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(&payload);

        let http_request = self.build(
            service,
            method,
            "application/connect+proto",
            Bytes::from(body),
        )?;

        // Only the response *headers* get a deadline. Run streams idle for as
        // long as a tool call takes, and the bridge keeps them alive with
        // empty frames roughly every 15 seconds.
        let response = tokio::time::timeout(self.request_timeout, self.http.request(http_request))
            .await
            .map_err(|_| Error::Timeout {
                operation: "opening a stream to the bridge",
                timeout: self.request_timeout,
            })?
            .map_err(|source| Error::transport(format!("{rpc}: {source}")))?;

        let status = response.status();
        let http_body = response.into_body();

        if status != StatusCode::OK {
            let body = http_body
                .collect()
                .await
                .map_err(|source| {
                    Error::transport(format!("{rpc}: reading the error failed: {source}"))
                })?
                .to_bytes();
            return Err(rpc_error_from_connect(&body, Some(status.as_u16()), &rpc).into());
        }

        let bytes = http_body_util::BodyStream::new(http_body).filter_map(|frame| async move {
            match frame {
                Ok(frame) => frame.into_data().ok().map(Ok),
                Err(source) => Some(Err(std::io::Error::other(source))),
            }
        });

        Ok(ServerStream {
            reader: Box::pin(StreamReader::new(bytes)),
            rpc,
            finished: false,
            _message: std::marker::PhantomData,
        })
    }
}

/// A live server stream of decoded protobuf messages.
///
/// Dropping the stream closes the HTTP connection. For run streams that does
/// **not** cancel the run — reconnect with `ObserveRun` or call `CancelRun`.
pub struct ServerStream<T> {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    rpc: String,
    finished: bool,
    _message: std::marker::PhantomData<fn() -> T>,
}

impl<T> std::fmt::Debug for ServerStream<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerStream")
            .field("rpc", &self.rpc)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<T: Message + Default> ServerStream<T> {
    /// Read the next message, or `None` once the bridge ends the stream.
    pub async fn next(&mut self) -> Option<Result<T>> {
        if self.finished {
            return None;
        }
        match self.read_frame().await {
            Ok(Some(message)) => Some(Ok(message)),
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }

    /// Adapt this stream to a [`futures_core::Stream`] for use with combinators.
    pub fn into_stream(self) -> impl futures_core::Stream<Item = Result<T>> + Send
    where
        T: Send + 'static,
    {
        futures_util::stream::unfold(self, |mut stream| async move {
            stream.next().await.map(|item| (item, stream))
        })
    }

    async fn read_frame(&mut self) -> Result<Option<T>> {
        let mut header = [0u8; 5];
        match self.reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(Error::transport(format!(
                    "{}: the stream ended without an end-of-stream frame",
                    self.rpc
                )));
            }
            Err(source) => {
                return Err(Error::transport(format!("{}: {source}", self.rpc)));
            }
        }

        let flags = header[0];
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;

        let mut payload = vec![0u8; length];
        self.reader
            .read_exact(&mut payload)
            .await
            .map_err(|source| {
                Error::transport(format!("{}: truncated frame: {source}", self.rpc))
            })?;

        if flags & FLAG_COMPRESSED != 0 {
            return Err(Error::transport(format!(
                "{}: the bridge sent a compressed frame although this client asked for identity \
                 encoding",
                self.rpc
            )));
        }

        if flags & FLAG_END_STREAM != 0 {
            // EndStreamResponse: JSON, carrying the error if the stream failed.
            let end: EndStream = if payload.is_empty() {
                EndStream::default()
            } else {
                serde_json::from_slice(&payload).unwrap_or_default()
            };
            if let Some(error) = end.error {
                let body = serde_json::to_vec(&error).unwrap_or_default();
                return Err(rpc_error_from_connect(&body, None, &self.rpc).into());
            }
            return Ok(None);
        }

        T::decode(&payload[..])
            .map(Some)
            .map_err(|source| Error::decode("stream message", source))
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct EndStream {
    error: Option<serde_json::Value>,
}
