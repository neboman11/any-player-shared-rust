use std::{io::ErrorKind, sync::Once, time::Duration};

use bytes::Bytes;
use librespot_core::http_client::HttpClient;
use log::{LevelFilter, Log, Metadata, Record};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

struct DebugLogger;

impl Log for DebugLogger {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, _: &Record<'_>) {}

    fn flush(&self) {}
}

static DEBUG_LOGGER: DebugLogger = DebugLogger;
static LOGGER_INIT: Once = Once::new();

fn enable_debug_logging() {
    LOGGER_INIT.call_once(|| {
        log::set_logger(&DEBUG_LOGGER).expect("install test logger");
        log::set_max_level(LevelFilter::Debug);
    });
}

#[tokio::test]
async fn debug_error_logging_does_not_wait_for_an_unfinished_error_body() {
    enable_debug_logging();

    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping loopback HTTP test: {error}");
            return;
        }
        Err(error) => panic!("bind loopback listener: {error}"),
    };
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        stream.read_u8().await.expect("read request");
        stream
            .write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 513\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("write response headers");
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
    .expect("error response logging must not wait for the body to finish");

    assert!(result.is_err());
}
