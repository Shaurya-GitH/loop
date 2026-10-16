//! Test helpers shared across Loop crates. Dev-dependency only.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A canned HTTP response.
#[derive(Debug, Clone)]
pub struct FakeResponse {
    /// Status code.
    pub status: u16,
    /// `content-type` header.
    pub content_type: String,
    /// Extra headers (e.g. `Location` for redirects).
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
    /// Delay between the response headers and body.
    pub body_delay: Option<Duration>,
}

impl FakeResponse {
    /// `200` with a JSON body.
    pub fn json(body: impl Into<String>) -> Self {
        Self::new(200, "application/json", body.into().into_bytes())
    }

    /// Any status with a body.
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            headers: Vec::new(),
            body: body.into(),
            body_delay: None,
        }
    }

    /// Add an extra header (e.g. `Location` for a redirect).
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Delay sending the body after response headers, for cancellation tests.
    pub fn with_body_delay(mut self, delay: Duration) -> Self {
        self.body_delay = Some(delay);
        self
    }
}

/// One request the server received.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Method, e.g. `GET`.
    pub method: String,
    /// Path including query.
    pub path: String,
    /// Headers with lower-cased names.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// First header value by (case-insensitive) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
}

type Router = dyn Fn(&RecordedRequest) -> FakeResponse + Send + Sync;

/// Minimal HTTP/1.1 server on `127.0.0.1` for tests: routes each request through a
/// closure and records it. Stops when dropped.
pub struct FakeHttpServer {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeHttpServer {
    /// Start serving; `route` builds the response for each request.
    pub fn start(route: impl Fn(&RecordedRequest) -> FakeResponse + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let route: Arc<Router> = Arc::new(route);
        let thread = {
            let (requests, stop) = (Arc::clone(&requests), Arc::clone(&stop));
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (requests, route) = (Arc::clone(&requests), Arc::clone(&route));
                    std::thread::spawn(move || serve(stream, &requests, route.as_ref()));
                }
            })
        };
        Self {
            base_url,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// Serve the same response to every request.
    pub fn always(response: FakeResponse) -> Self {
        Self::start(move |_| response.clone())
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Requests received so far.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the stop flag.
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(stream: TcpStream, requests: &Mutex<Vec<RecordedRequest>>, route: &Router) {
    let Some(request) = read_request(&stream) else {
        return;
    };
    let response = route(&request);
    requests.lock().unwrap().push(request);
    let mut stream = stream;
    let mut head = format!(
        "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    if let Some(delay) = response.body_delay {
        std::thread::sleep(delay);
    }
    let _ = stream.write_all(&response.body);
}

fn read_request(stream: &TcpStream) -> Option<RecordedRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(base: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(base.trim_start_matches("http://")).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nhost: x\r\nx-test: 1\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn routes_and_records_requests() {
        let server = FakeHttpServer::start(|req| match req.path.as_str() {
            "/ok" => FakeResponse::json(r#"{"a":1}"#),
            _ => FakeResponse::new(404, "text/plain", "nope"),
        });
        assert!(get(server.base_url(), "/ok").ends_with(r#"{"a":1}"#));
        assert!(get(server.base_url(), "/missing").starts_with("HTTP/1.1 404"));
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].header("X-Test"), Some("1"));
    }
}
