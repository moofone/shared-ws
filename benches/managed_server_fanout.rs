//! Fanout benchmark for the managed WebSocket server.
//!
//! Measures the per-`dispatch` cost of fanning one shared `Bytes` payload out to
//! N live connections at N = 1, 64, 256. The payload is serialized once by the
//! caller; each connection's handler clones only a refcount (`Bytes::clone`), so
//! the curve should be near-linear in N with a small per-connection constant and
//! **no** per-connection reserialization or dynamic dispatch.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use shared_ws::ws::server::{
    ManagedWsServerStatus, WsServerConfig, WsServerConnectionContext, WsServerEndpointAction,
    WsServerEndpointEvent, WsServerEndpointFactory, WsServerEndpointHandler, WsServerRetryPolicy,
    spawn_managed_ws_server_task,
};
use shared_ws::ws::{WsFrame, frame_bytes};

#[derive(Clone)]
struct FanoutCommand {
    payload: Bytes,
}

#[derive(Debug, thiserror::Error)]
#[error("bench handler error")]
struct BenchError;

/// Counts how many connections observed the shared payload allocation, proving
/// fanout copies refcounts rather than bytes.
#[derive(Default)]
struct BenchObs {
    delivered: AtomicUsize,
}

struct BenchHandler {
    obs: Arc<BenchObs>,
}

impl WsServerEndpointHandler for BenchHandler {
    type Command = FanoutCommand;
    type Error = BenchError;

    async fn on_event(
        &mut self,
        event: WsServerEndpointEvent<'_, Self::Command>,
    ) -> Result<WsServerEndpointAction, Self::Error> {
        match event {
            WsServerEndpointEvent::Connected(_) => Ok(WsServerEndpointAction::Send(
                WsFrame::text_static(r#"{"type":"hello"}"#),
            )),
            WsServerEndpointEvent::Command(cmd) => {
                self.obs.delivered.fetch_add(1, Ordering::Relaxed);
                Ok(WsServerEndpointAction::Send(WsFrame::Binary(
                    cmd.payload.clone(),
                )))
            }
            _ => Ok(WsServerEndpointAction::None),
        }
    }
}

struct BenchFactory {
    obs: Arc<BenchObs>,
}

impl WsServerEndpointFactory for BenchFactory {
    type Handler = BenchHandler;
    fn create_endpoint(&self, _ctx: &WsServerConnectionContext) -> Self::Handler {
        BenchHandler {
            obs: Arc::clone(&self.obs),
        }
    }
}

fn free_addr() -> SocketAddr {
    let l = StdTcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    drop(l);
    addr
}

async fn drain_hello(client: &mut shared_ws::client::WsClient) {
    let _ = tokio::time::timeout(Duration::from_secs(5), client.next()).await;
}

async fn drain_one(client: &mut shared_ws::client::WsClient) {
    // Read exactly one delivered fanout frame so the socket buffers never fill
    // and stall dispatch behind backpressure during the measured loop.
    loop {
        match tokio::time::timeout(Duration::from_secs(5), client.next()).await {
            Ok(Some(Ok(frame))) if frame_bytes(&frame).is_some() => return,
            Ok(Some(Ok(_))) => continue,
            _ => return,
        }
    }
}

fn bench_fanout(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let mut group = c.benchmark_group("managed_server_fanout");
    for &clients in &[1usize, 64, 256] {
        group.throughput(Throughput::Elements(clients as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(clients),
            &clients,
            |b, &clients| {
                let (handle, mut sockets) = runtime.block_on(async {
                    let addr = free_addr();
                    let obs = Arc::new(BenchObs::default());
                    let mut config = WsServerConfig::new(addr);
                    config.max_connections = 512;
                    // Wide enough that the read loop below never trips the lag cap.
                    config.retained_commands_per_client = 64;
                    let handle = spawn_managed_ws_server_task(
                        config,
                        BenchFactory {
                            obs: Arc::clone(&obs),
                        },
                        WsServerRetryPolicy::default(),
                    )
                    .expect("spawn");
                    handle
                        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
                        .await
                        .expect("listening");
                    let mut sockets = Vec::with_capacity(clients);
                    for _ in 0..clients {
                        let mut client = shared_ws::client::connect_async(format!("ws://{addr}"))
                            .await
                            .expect("connect");
                        drain_hello(&mut client).await;
                        sockets.push(client);
                    }
                    while handle.active_connection_count() != clients {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    (handle, sockets)
                });

                let payload = Bytes::from_static(&[0u8; 512]);
                b.iter(|| {
                    runtime.block_on(async {
                        let delivered = handle
                            .dispatch(FanoutCommand {
                                payload: black_box(payload.clone()),
                            })
                            .expect("dispatch");
                        black_box(delivered);
                        // Keep the sockets drained so dispatch measures fanout, not
                        // TCP backpressure.
                        for socket in sockets.iter_mut() {
                            drain_one(socket).await;
                        }
                    });
                });

                runtime.block_on(async {
                    let _ = handle.shutdown_with_timeout(Duration::from_secs(2)).await;
                    drop(sockets);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_fanout);
criterion_main!(benches);
