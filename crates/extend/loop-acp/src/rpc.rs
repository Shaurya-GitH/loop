//! Line-delimited JSON-RPC 2.0 over any byte stream.
//!
//! One [`Connection`] owns a reader task and a writer task. Everything the
//! peer sends arrives on one ordered [`Incoming`] channel, responses
//! included: the consumer hands each [`Incoming::Response`] to
//! [`Connection::resolve`]. Keeping responses in that order means a request's
//! result is seen only after every notification the peer sent before it (an
//! ACP turn's output before its end). When the stream ends, every pending
//! request fails with [`RpcError::Closed`] and the incoming channel closes.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

/// Longest line accepted from the peer. Longer lines are dropped so a
/// misbehaving agent cannot exhaust memory.
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// JSON-RPC error codes Loop sends.
pub mod codes {
    /// The method does not exist or is not offered.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The parameters could not be parsed.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Loop failed while handling the request.
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    /// The peer answered with an error.
    Remote {
        /// JSON-RPC error code.
        code: i64,
        /// Error message.
        message: String,
        /// Extra data.
        data: Option<Value>,
    },
    /// The connection closed before an answer arrived.
    Closed,
    /// The answer could not be parsed.
    Invalid(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote { code, message, .. } => write!(f, "{message} (code {code})"),
            Self::Closed => f.write_str("the agent connection closed"),
            Self::Invalid(why) => write!(f, "invalid message from the agent: {why}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// A request id as the peer sent it, echoed back unchanged.
pub type PeerRequestId = Value;

/// A message from the peer.
#[derive(Debug)]
pub enum Incoming {
    /// The result of one of Loop's requests. Pass it to
    /// [`Connection::resolve`] once everything before it is handled.
    Response {
        /// Loop's request id.
        id: i64,
        /// The result.
        result: Result<Value, RpcError>,
    },
    /// The peer asks for something and waits for [`Connection::respond`].
    Request {
        /// Id to answer with.
        id: PeerRequestId,
        /// Method name.
        method: String,
        /// Parameters (`null` when absent).
        params: Value,
    },
    /// The peer reports something; no answer.
    Notification {
        /// Method name.
        method: String,
        /// Parameters (`null` when absent).
        params: Value,
    },
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// One JSON-RPC connection.
pub struct Connection {
    next_id: AtomicI64,
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Pending,
    closed: CancellationToken,
}

impl Connection {
    /// Start reading `reader` and writing `writer`. Returns the connection and
    /// the channel of incoming requests and notifications.
    pub fn new<R, W>(reader: R, writer: W) -> (Arc<Self>, mpsc::UnboundedReceiver<Incoming>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing, mut to_write) = mpsc::unbounded_channel::<Value>();
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();
        let closed = CancellationToken::new();

        let writer_closed = closed.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            loop {
                let message = tokio::select! {
                    _ = writer_closed.cancelled() => break,
                    message = to_write.recv() => match message {
                        Some(message) => message,
                        None => break,
                    },
                };
                let mut line = message.to_string();
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err()
                {
                    break;
                }
            }
            // Dropping the writer closes the agent's input.
            let _ = writer.shutdown().await;
        });

        tokio::spawn(async move {
            // The incoming channel closes when `incoming_tx` drops here; the
            // consumer then calls `fail_pending`.
            read_loop(reader, &incoming_tx).await;
        });

        (
            Arc::new(Self {
                next_id: AtomicI64::new(1),
                outgoing,
                pending,
                closed,
            }),
            incoming,
        )
    }

    /// Send a request and wait for its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.outgoing.send(message).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(RpcError::Closed);
        }
        rx.await.unwrap_or(Err(RpcError::Closed))
    }

    /// Deliver a response from the incoming channel to its waiting request.
    pub async fn resolve(&self, id: i64, result: Result<Value, RpcError>) {
        match self.pending.lock().await.remove(&id) {
            Some(waiter) => {
                let _ = waiter.send(result);
            }
            None => tracing::debug!(id, "response to an unknown or finished request"),
        }
    }

    /// Fail every request still waiting with [`RpcError::Closed`]. Call once
    /// the incoming channel has closed and everything in it was handled, so
    /// no queued response is lost.
    pub async fn fail_pending(&self) {
        for (_, waiter) in self.pending.lock().await.drain() {
            let _ = waiter.send(Err(RpcError::Closed));
        }
    }

    /// Stop writing and close the agent's input. Reading continues until the
    /// agent closes its output.
    pub fn close(&self) {
        self.closed.cancel();
    }

    /// Send a notification.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.outgoing
            .send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .map_err(|_| RpcError::Closed)
    }

    /// Answer a request from the peer.
    pub fn respond(&self, id: PeerRequestId, result: Result<Value, (i64, String)>) {
        let message = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let _ = self.outgoing.send(message);
    }
}

async fn read_loop<R>(reader: R, incoming: &mpsc::UnboundedSender<Incoming>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_line_capped(&mut reader, &mut line).await {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "agent stream read failed");
                return;
            }
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(_) => {
                // Agents sometimes print logs to stdout; skip anything that
                // is not JSON instead of failing the connection.
                tracing::debug!(line = %truncate(text, 200), "skipping non-JSON line from agent");
                continue;
            }
        };
        dispatch(message, incoming);
    }
}

/// Read one line, dropping lines over [`MAX_LINE_BYTES`]. Returns the bytes
/// read including the newline; 0 at end of stream.
async fn read_line_capped<R>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> std::io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    let mut total = 0;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (&available[..=i], true),
            None => (available, false),
        };
        let used = chunk.len();
        if !oversized && line.len() + used <= MAX_LINE_BYTES {
            line.extend_from_slice(chunk);
        } else if !oversized {
            oversized = true;
            line.clear();
            tracing::warn!("dropping a message from the agent larger than {MAX_LINE_BYTES} bytes");
        }
        reader.consume(used);
        total += used;
        if done {
            return Ok(total);
        }
    }
}

fn dispatch(message: Value, incoming: &mpsc::UnboundedSender<Incoming>) {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = message.get("id").cloned().filter(|id| !id.is_null());
    let params = || message.get("params").cloned().unwrap_or(Value::Null);
    let incoming_message = match (method, id) {
        (Some(method), Some(id)) => Incoming::Request {
            id,
            method,
            params: params(),
        },
        (Some(method), None) => Incoming::Notification {
            method,
            params: params(),
        },
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else {
                tracing::debug!(%id, "response with an id Loop never sent");
                return;
            };
            let result = if let Some(error) = message.get("error") {
                Err(RpcError::Remote {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned(),
                    data: error.get("data").cloned(),
                })
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            Incoming::Response { id, result }
        }
        (None, None) => {
            tracing::debug!("skipping a message with neither method nor id");
            return;
        }
    };
    let _ = incoming.send(incoming_message);
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    #[tokio::test]
    async fn requests_and_responses_match_by_id_across_split_frames() {
        let (client_end, agent_end) = duplex(64);
        let (read, write) = tokio::io::split(client_end);
        let (conn, incoming) = Connection::new(read, write);
        resolve_responses(&conn, incoming);
        let (agent_read, mut agent_write) = tokio::io::split(agent_end);

        let call = tokio::spawn({
            let conn = Arc::clone(&conn);
            async move { conn.request("ping", json!({"n": 1})).await }
        });
        let mut lines = BufReader::new(agent_read).lines();
        let request: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "ping");
        // A log line, then the response written in two pieces.
        let response = format!(
            "not json at all\n{}\n",
            json!({"jsonrpc": "2.0", "id": request["id"], "result": {"pong": true}})
        );
        let (a, b) = response.split_at(response.len() / 2);
        agent_write.write_all(a.as_bytes()).await.unwrap();
        agent_write.write_all(b.as_bytes()).await.unwrap();
        assert_eq!(call.await.unwrap().unwrap(), json!({"pong": true}));
    }

    #[tokio::test]
    async fn peer_requests_and_notifications_arrive_and_closing_fails_pending() {
        let (client_end, agent_end) = duplex(1024);
        let (read, write) = tokio::io::split(client_end);
        let (conn, mut incoming) = Connection::new(read, write);
        let (_agent_read, mut agent_write) = tokio::io::split(agent_end);

        agent_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"fs/read_text_file\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"x\":1}}\n")
            .await
            .unwrap();
        assert!(matches!(
            incoming.recv().await.unwrap(),
            Incoming::Request { id, method, .. } if id == json!("a") && method == "fs/read_text_file"
        ));
        assert!(matches!(
            incoming.recv().await.unwrap(),
            Incoming::Notification { method, .. } if method == "session/update"
        ));

        resolve_responses(&conn, incoming);
        let waiting = tokio::spawn({
            let conn = Arc::clone(&conn);
            async move { conn.request("slow", Value::Null).await }
        });
        tokio::task::yield_now().await;
        drop(agent_write);
        drop(_agent_read);
        assert_eq!(waiting.await.unwrap(), Err(RpcError::Closed));
    }

    /// Resolve responses the way a consumer does: in channel order.
    fn resolve_responses(conn: &Arc<Connection>, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
        let conn = Arc::clone(conn);
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Response { id, result } = message {
                    conn.resolve(id, result).await;
                }
            }
            conn.fail_pending().await;
        });
    }

    #[tokio::test]
    async fn a_response_comes_after_the_notifications_sent_before_it() {
        let (client_end, agent_end) = duplex(1 << 16);
        let (read, write) = tokio::io::split(client_end);
        let (conn, mut incoming) = Connection::new(read, write);
        let (agent_read, mut agent_write) = tokio::io::split(agent_end);
        let call = tokio::spawn({
            let conn = Arc::clone(&conn);
            async move { conn.request("go", Value::Null).await }
        });
        let mut lines = BufReader::new(agent_read).lines();
        let request: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let mut burst = String::new();
        for n in 0..50 {
            burst.push_str(&format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "method": "note", "params": {"n": n}})
            ));
        }
        burst.push_str(&format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "done"})
        ));
        agent_write.write_all(burst.as_bytes()).await.unwrap();
        let mut seen = 0;
        loop {
            match incoming.recv().await.unwrap() {
                Incoming::Notification { .. } => seen += 1,
                Incoming::Response { id, result } => {
                    assert_eq!(seen, 50, "every earlier notification arrives first");
                    conn.resolve(id, result).await;
                    break;
                }
                Incoming::Request { .. } => unreachable!(),
            }
        }
        assert_eq!(call.await.unwrap().unwrap(), json!("done"));
    }
}
