use std::{
    io::ErrorKind,
    sync::{Mutex, Once, OnceLock},
    time::Duration,
};

use bytes::Bytes;
use librespot_core::http_client::HttpClient;
use log::{LevelFilter, Log, Metadata, Record};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

struct DebugLogger {
    messages: OnceLock<Mutex<Vec<String>>>,
}

impl Log for DebugLogger {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        self.messages
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .expect("test log buffer mutex")
            .push(format!("{}: {}", record.level(), record.args()));
    }

    fn flush(&self) {}
}

static DEBUG_LOGGER: DebugLogger = DebugLogger {
    messages: OnceLock::new(),
};
static LOGGER_INIT: Once = Once::new();

fn enable_debug_logging() {
    LOGGER_INIT.call_once(|| {
        log::set_logger(&DEBUG_LOGGER).expect("install test logger");
        log::set_max_level(LevelFilter::Debug);
    });
}

fn logged_messages() -> Vec<String> {
    DEBUG_LOGGER
        .messages
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("test log buffer mutex")
        .clone()
}

async fn bind_test_listener() -> Option<TcpListener> {
    match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => Some(listener),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping loopback HTTP test: {error}");
            None
        }
        Err(error) => panic!("bind loopback listener: {error}"),
    }
}

#[tokio::test]
async fn error_logging_redacts_uri_credentials_and_query() {
    enable_debug_logging();
    let Some(listener) = bind_test_listener().await else {
        return;
    };
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        stream.read_u8().await.expect("read request");
        stream
            .write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write response");
    });

    let request = http::Request::builder()
        .uri(format!(
            "http://username:password@{address}/error?access_token=query-secret"
        ))
        .body(Bytes::new())
        .expect("build request");

    assert!(HttpClient::new(None).request(request).await.is_err());

    let expected_uri = format!("http://{address}/error");
    let messages = logged_messages();
    assert!(
        messages.iter().any(|message| {
            message.contains(&format!(
                "HTTP 500 Internal Server Error for {expected_uri}"
            ))
        }),
        "expected redacted URI in logs, got {messages:?}"
    );
    assert!(
        messages
            .iter()
            .all(|message| !message.contains("username:password")
                && !message.contains("query-secret")),
        "credentials or query appeared in logs: {messages:?}"
    );
}

#[tokio::test]
async fn debug_error_logging_does_not_truncate_an_exactly_limited_body() {
    enable_debug_logging();
    let Some(listener) = bind_test_listener().await else {
        return;
    };
    let address = listener.local_addr().expect("listener address");
    let body = vec![b'x'; 512];
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        stream.read_u8().await.expect("read request");
        stream
            .write_all(
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("write response headers");
        stream.write_all(&body).await.expect("write response body");
    });

    let request = http::Request::builder()
        .uri(format!("http://{address}/exact-limit"))
        .body(Bytes::new())
        .expect("build request");

    assert!(HttpClient::new(None).request(request).await.is_err());

    let expected_uri = format!("http://{address}/exact-limit");
    let messages = logged_messages();
    let body_log = messages
        .iter()
        .find(|message| message.contains(&format!("for {expected_uri} body:")))
        .expect("expected exact-limit body log");
    assert!(
        !body_log.contains("<truncated after 512 bytes>"),
        "exactly limited body was marked truncated: {body_log}"
    );
}

#[tokio::test]
async fn debug_error_logging_truncates_oversized_body_without_waiting() {
    enable_debug_logging();
    let Some(listener) = bind_test_listener().await else {
        return;
    };
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        stream.read_u8().await.expect("read request");
        let body = b"retained-error-prefix-".repeat(30);
        assert!(body.len() > 512);
        stream
            .write_all(
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len() + 1
                )
                .as_bytes(),
            )
            .await
            .expect("write response headers");
        stream.write_all(&body).await.expect("write oversized body");
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let request = http::Request::builder()
        .uri(format!("http://{address}/error"))
        .body(Bytes::new())
        .expect("build request");
    let result = timeout(
        Duration::from_millis(250),
        HttpClient::new(None).request(request),
    )
    .await
    .expect("error response logging must not wait for body to finish");
    assert!(result.is_err());

    let messages = logged_messages();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("body: retained-error-prefix-")),
        "expected retained body prefix in logs, got {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("<truncated after 512 bytes>")),
        "expected truncation marker in logs, got {messages:?}"
    );
}
