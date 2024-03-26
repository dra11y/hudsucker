use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use http_body_util::Empty;
use hudsucker::{
    certificate_authority::{CertificateAuthority, RcgenAuthority},
    hyper::{body::Incoming, service::service_fn, Method, Request, Response},
    hyper_util::client::legacy::{connect::HttpConnector, Client},
    hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto,
    },
    Body, Proxy,
};
use reqwest::Certificate;
use rustls_pemfile as pemfile;
use std::{convert::Infallible, net::SocketAddr};
use tokio::{net::TcpListener, sync::oneshot::Sender};
use tokio_graceful::Shutdown;
use tokio_native_tls::native_tls;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap()
}

fn build_ca() -> RcgenAuthority {
    let mut private_key_bytes: &[u8] = include_bytes!("../examples/ca/hudsucker.key");
    let mut ca_cert_bytes: &[u8] = include_bytes!("../examples/ca/hudsucker.cer");
    let private_key = pemfile::private_key(&mut private_key_bytes)
        .unwrap()
        .expect("Failed to parse private key");
    let ca_cert = pemfile::certs(&mut ca_cert_bytes)
        .next()
        .unwrap()
        .expect("Failed to parse CA certificate");

    RcgenAuthority::new(private_key, ca_cert, 1_000)
        .expect("Failed to create Certificate Authority")
}

async fn test_server(req: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/hello") => Ok(Response::new(Body::from("hello, world"))),
        _ => Ok(Response::new(Body::from(Empty::new()))),
    }
}

pub async fn start_http_server() -> Result<(SocketAddr, Sender<()>), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let server = auto::Builder::new(TokioExecutor::new());
        let shutdown = Shutdown::new(async { rx.await.unwrap_or_default() });
        let guard = shutdown.guard_weak();

        loop {
            tokio::select! {
                res = listener.accept() => {
                    let (tcp, _) = res.unwrap();
                    let server = server.clone();

                    shutdown.spawn_task(async move {
                        server
                            .serve_connection_with_upgrades(TokioIo::new(tcp), service_fn(test_server))
                            .await
                            .unwrap();
                    });
                }
                _ = guard.cancelled() => {
                    break;
                }
            }
        }

        shutdown.shutdown().await;
    });

    Ok((addr, tx))
}

pub async fn start_https_server(
    ca: impl CertificateAuthority,
) -> Result<(SocketAddr, Sender<()>), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = listener.local_addr()?;
    let acceptor: tokio_rustls::TlsAcceptor = ca
        .gen_server_config(&"localhost".parse().unwrap())
        .await
        .into();
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let server = auto::Builder::new(TokioExecutor::new());
        let shutdown = Shutdown::new(async { rx.await.unwrap_or_default() });
        let guard = shutdown.guard_weak();

        loop {
            tokio::select! {
                res = listener.accept() => {
                    let (tcp, _) = res.unwrap();
                    let tcp = acceptor.accept(tcp).await.unwrap();
                    let server = server.clone();

                    shutdown.spawn_task(async move {
                        server
                            .serve_connection_with_upgrades(TokioIo::new(tcp), service_fn(test_server))
                            .await
                            .unwrap();
                    });
                }
                _ = guard.cancelled() => {
                    break;
                }
            }
        }

        shutdown.shutdown().await;
    });

    Ok((addr, tx))
}

fn native_tls_client() -> Client<hyper_tls::HttpsConnector<HttpConnector>, Body> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let ca_cert =
        native_tls::Certificate::from_pem(include_bytes!("../examples/ca/hudsucker.cer")).unwrap();

    let tls = native_tls::TlsConnector::builder()
        .add_root_certificate(ca_cert)
        .build()
        .unwrap()
        .into();

    let https: hyper_tls::HttpsConnector<HttpConnector> = (http, tls).into();

    Client::builder(TokioExecutor::new()).build(https)
}

async fn start_proxy(
    ca: impl CertificateAuthority,
) -> Result<(SocketAddr, Sender<()>), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let proxy = Proxy::builder()
        .with_listener(listener)
        .with_client(native_tls_client())
        .with_ca(ca)
        .with_graceful_shutdown(async {
            rx.await.unwrap_or_default();
        })
        .build();

    tokio::spawn(proxy.start());

    Ok((addr, tx))
}

fn build_client() -> reqwest::Client {
    let ca_cert = Certificate::from_pem(include_bytes!("../examples/ca/hudsucker.cer")).unwrap();

    reqwest::Client::builder()
        .add_root_certificate(ca_cert)
        .build()
        .unwrap()
}

fn build_proxied_client(proxy: &str) -> reqwest::Client {
    let proxy = reqwest::Proxy::all(proxy).unwrap();
    let ca_cert = Certificate::from_pem(include_bytes!("../examples/ca/hudsucker.cer")).unwrap();

    reqwest::Client::builder()
        .proxy(proxy)
        .add_root_certificate(ca_cert)
        .build()
        .unwrap()
}

fn bench_local(c: &mut Criterion) {
    let runtime = runtime();
    let _guard = runtime.enter();

    let (proxy_addr, stop_proxy) = runtime.block_on(start_proxy(build_ca())).unwrap();
    let (http_addr, stop_http) = runtime.block_on(start_http_server()).unwrap();
    let (https_addr, stop_https) = runtime.block_on(start_https_server(build_ca())).unwrap();
    let client = build_client();
    let proxied_client = build_proxied_client(&proxy_addr.to_string());

    let mut group = c.benchmark_group("proxy local site");
    group.throughput(Throughput::Elements(1));
    group.bench_function("HTTP without proxy", |b| {
        b.to_async(&runtime).iter(|| async {
            client
                .get(format!("http://{}/hello", http_addr))
                .send()
                .await
                .unwrap()
        })
    });
    group.bench_function("HTTP with proxy", |b| {
        b.to_async(&runtime).iter(|| async {
            proxied_client
                .get(format!("http://{}/hello", http_addr))
                .send()
                .await
                .unwrap()
        })
    });
    group.bench_function("HTTPS without proxy", |b| {
        b.to_async(&runtime).iter(|| async {
            client
                .get(format!("https://localhost:{}/hello", https_addr.port()))
                .send()
                .await
                .unwrap()
        })
    });
    group.bench_function("HTTPS with proxy", |b| {
        b.to_async(&runtime).iter(|| async {
            proxied_client
                .get(format!("https://localhost:{}/hello", https_addr.port()))
                .send()
                .await
                .unwrap()
        })
    });
    group.finish();

    stop_http.send(()).unwrap();
    stop_https.send(()).unwrap();
    stop_proxy.send(()).unwrap();
}

fn bench_remote(c: &mut Criterion) {
    let runtime = runtime();
    let _guard = runtime.enter();

    let (proxy_addr, stop_proxy) = runtime.block_on(start_proxy(build_ca())).unwrap();
    let client = build_client();
    let proxied_client = build_proxied_client(&proxy_addr.to_string());

    let mut group = c.benchmark_group("proxy remote site");
    group.throughput(Throughput::Elements(1));
    group.bench_function("HTTP without proxy", |b| {
        b.to_async(&runtime)
            .iter(|| async { client.get("http://echo.omjad.as").send().await.unwrap() })
    });
    group.bench_function("HTTP with proxy", |b| {
        b.to_async(&runtime).iter(|| async {
            proxied_client
                .get("http://echo.omjad.as")
                .send()
                .await
                .unwrap()
        })
    });
    group.bench_function("HTTPS without proxy", |b| {
        b.to_async(&runtime)
            .iter(|| async { client.get("https://echo.omjad.as").send().await.unwrap() })
    });
    group.bench_function("HTTPS with proxy", |b| {
        b.to_async(&runtime).iter(|| async {
            proxied_client
                .get("https://echo.omjad.as")
                .send()
                .await
                .unwrap()
        })
    });
    group.finish();

    let _ = stop_proxy.send(());
}

criterion_group!(benches, bench_local, bench_remote);
criterion_main!(benches);
