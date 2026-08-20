//! 服务器集成测试：TCP 往返、token 鉴权、订阅推送、UDS 互通、裸 socket。

use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use xtbp_api::client::connect_tcp;
use xtbp_api::protocol::methods;
use xtbp_api::server::test_support::{EchoHandler, random_token};
use xtbp_api::{EventBus, ServerConfig, serve_tcp};

/// 起一个 TCP 服务器，返回 (addr, token, bus, shutdown, join_handle)。
async fn spawn_tcp_server() -> (
    String,
    String,
    EventBus,
    CancellationToken,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let bus = EventBus::new(64);
    let token = random_token();
    let config = Arc::new(ServerConfig {
        token: token.clone(),
        event_bus: bus.clone(),
    });
    let shutdown = CancellationToken::new();
    let handler = Arc::new(EchoHandler);
    let handle = tokio::spawn(serve_tcp(listener, handler, config, shutdown.clone()));
    (addr, token, bus, shutdown, handle)
}

#[tokio::test]
async fn sys_health_roundtrip_over_tcp() {
    let (addr, token, _bus, shutdown, handle) = spawn_tcp_server().await;
    let mut client = connect_tcp(&addr, token).await.unwrap();
    let resp = client
        .call(methods::SYS_HEALTH, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(resp["ok"], true);
    shutdown.cancel();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn unknown_method_returns_structured_error() {
    let (addr, token, _bus, shutdown, handle) = spawn_tcp_server().await;
    let mut client = connect_tcp(&addr, token).await.unwrap();
    let err = client
        .call("nope.method", serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32601);
    shutdown.cancel();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscription_receives_pushed_events() {
    let (addr, token, bus, shutdown, handle) = spawn_tcp_server().await;
    let mut client = connect_tcp(&addr, token).await.unwrap();
    client
        .subscribe(methods::JOB_EVENTS, true, Some("job-1"))
        .await
        .unwrap();
    bus.publish(xtbp_core::job::JobEvent::Status {
        job_id: "job-1".into(),
        status: "running".into(),
    });
    bus.publish(xtbp_core::job::JobEvent::Status {
        job_id: "job-2".into(),
        status: "running".into(),
    });
    let event = client.read_event().await.unwrap().unwrap();
    assert_eq!(event.method, "job.events");
    assert_eq!(event.params["job_id"], "job-1"); // job-2 被过滤
    shutdown.cancel();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn bad_token_connection_is_rejected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let bus = EventBus::new(64);
    let config = Arc::new(ServerConfig {
        token: random_token(),
        event_bus: bus.clone(),
    });
    let shutdown = CancellationToken::new();
    let handler = Arc::new(EchoHandler);
    let handle = tokio::spawn(serve_tcp(listener, handler, config, shutdown.clone()));

    let mut client = connect_tcp(&addr, "wrong-token").await.unwrap();
    let err = client
        .call(methods::SYS_HEALTH, serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.structured_code(), Some("AUTH_FAILED"));
    shutdown.cancel();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn uds_roundtrip_works() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("xtbp.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let bus = EventBus::new(64);
    let config = Arc::new(ServerConfig {
        token: String::new(), // UDS 开发场景不鉴权
        event_bus: bus.clone(),
    });
    let shutdown = CancellationToken::new();
    let handler = Arc::new(EchoHandler);
    let handle = tokio::spawn(xtbp_api::serve_uds(
        listener,
        handler,
        config,
        shutdown.clone(),
    ));

    let mut client = xtbp_api::client::connect_uds(sock.to_str().unwrap(), "")
        .await
        .unwrap();
    let resp = client
        .call(methods::SYS_HEALTH, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(resp["ok"], true);
    shutdown.cancel();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn raw_socket_agent_style_roundtrip() {
    // P4 验收形态：裸 socket 手写 NDJSON（与 Windows 侧脚本同构），无 token 场景
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let bus = EventBus::new(64);
    let config = Arc::new(ServerConfig {
        token: String::new(),
        event_bus: bus.clone(),
    });
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(serve_tcp(
        listener,
        Arc::new(EchoHandler),
        config,
        shutdown.clone(),
    ));

    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let req = "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"sys.health\",\"params\":{}}\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(stream);
    use tokio::io::AsyncBufReadExt;
    reader.read_line(&mut line).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["result"]["ok"], true);
    shutdown.cancel();
    handle.await.unwrap().unwrap();
}
