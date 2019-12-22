#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[macro_use]
extern crate clap;

mod config;
mod constants;
mod dns;
mod errors;
mod globals;
mod utils;

use crate::config::*;
use crate::constants::*;
use crate::errors::*;
use crate::globals::*;

use clap::Arg;
use futures::future;
use futures::prelude::*;
use futures::task::{Context, Poll};
use hyper::http;
use hyper::server::conn::Http;
use hyper::{Body, Method, Request, Response, StatusCode};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};

#[cfg(feature = "tls")]
use native_tls::{self, Identity};
#[cfg(feature = "tls")]
use std::fs::File;
#[cfg(feature = "tls")]
use std::io;
#[cfg(feature = "tls")]
use std::io::Read;
#[cfg(feature = "tls")]
use std::path::Path;
#[cfg(feature = "tls")]
use tokio_tls::TlsAcceptor;

#[derive(Clone, Debug)]
struct DoH {
    globals: Arc<Globals>,
}

#[cfg(feature = "tls")]
fn create_tls_acceptor<P>(path: P, password: &str) -> io::Result<TlsAcceptor>
where
    P: AsRef<Path>,
{
    let identity_bin = {
        let mut fp = File::open(path)?;
        let mut identity_bin = vec![];
        fp.read_to_end(&mut identity_bin)?;
        identity_bin
    };
    let identity = Identity::from_pkcs12(&identity_bin, password).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unusable PKCS12-encoded identity. The encoding and/or the password may be wrong",
        )
    })?;
    let native_acceptor = native_tls::TlsAcceptor::new(identity).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unable to use the provided PKCS12-encoded identity",
        )
    })?;
    Ok(TlsAcceptor::from(native_acceptor))
}

impl hyper::service::Service<http::Request<Body>> for DoH {
    type Response = Response<Body>;
    type Error = http::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let globals = &self.globals;
        if req.uri().path() != globals.path {
            let response = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
            return Box::pin(async { Ok(response) });
        }
        let self_inner = self.clone();
        match *req.method() {
            Method::POST => {
                if globals.disable_post {
                    let response = Response::builder()
                        .status(StatusCode::METHOD_NOT_ALLOWED)
                        .body(Body::empty())
                        .unwrap();
                    return Box::pin(async { Ok(response) });
                }
                if let Err(response) = Self::check_content_type(&req) {
                    return Box::pin(async { Ok(response) });
                }
                let fut = async move {
                    match self_inner.read_body_and_proxy(req.into_body()).await {
                        Err(e) => Response::builder()
                            .status(StatusCode::from(e))
                            .body(Body::empty()),
                        Ok(res) => Ok(res),
                    }
                };
                Box::pin(fut)
            }
            Method::GET => {
                let query = req.uri().query().unwrap_or("");
                let mut question_str = None;
                for parts in query.split('&') {
                    let mut kv = parts.split('=');
                    if let Some(k) = kv.next() {
                        if k == DNS_QUERY_PARAM {
                            question_str = kv.next();
                        }
                    }
                }
                let question = match question_str.and_then(|question_str| {
                    base64::decode_config(question_str, base64::URL_SAFE_NO_PAD).ok()
                }) {
                    Some(question) => question,
                    _ => {
                        let response = Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .body(Body::empty())
                            .unwrap();
                        return Box::pin(future::ok(response));
                    }
                };
                let fut = async move {
                    match self_inner.proxy(question).await {
                        Err(e) => Response::builder()
                            .status(StatusCode::from(e))
                            .body(Body::empty()),
                        Ok(res) => Ok(res),
                    }
                };
                Box::pin(fut)
            }
            _ => {
                let response = Response::builder()
                    .status(StatusCode::METHOD_NOT_ALLOWED)
                    .body(Body::empty())
                    .unwrap();
                Box::pin(async { Ok(response) })
            }
        }
    }
}

impl DoH {
    fn check_content_type(req: &Request<Body>) -> Result<(), Response<Body>> {
        let headers = req.headers();
        let content_type = match headers.get(hyper::header::CONTENT_TYPE) {
            None => {
                let response = Response::builder()
                    .status(StatusCode::NOT_ACCEPTABLE)
                    .body(Body::empty())
                    .unwrap();
                return Err(response);
            }
            Some(content_type) => content_type.to_str(),
        };
        let content_type = match content_type {
            Err(_) => {
                let response = Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::empty())
                    .unwrap();
                return Err(response);
            }
            Ok(content_type) => content_type.to_lowercase(),
        };
        if content_type != "application/dns-message" {
            let response = Response::builder()
                .status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
                .body(Body::empty())
                .unwrap();
            return Err(response);
        }
        Ok(())
    }

    async fn read_body_and_proxy(&self, mut body: Body) -> Result<Response<Body>, DoHError> {
        let mut sum_size = 0;
        let mut query = vec![];
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|_| DoHError::TooLarge)?;
            sum_size += chunk.len();
            if sum_size >= MAX_DNS_QUESTION_LEN {
                return Err(DoHError::TooLarge);
            }
            query.extend(chunk);
        }
        let response = self.proxy(query).await?;
        Ok(response)
    }

    async fn proxy(&self, mut query: Vec<u8>) -> Result<Response<Body>, DoHError> {
        if query.len() < MIN_DNS_PACKET_LEN {
            return Err(DoHError::Incomplete);
        }
        let _ = dns::set_edns_max_payload_size(&mut query, MAX_DNS_RESPONSE_LEN as u16);
        let globals = &self.globals;
        let mut socket = UdpSocket::bind(&globals.local_bind_address)
            .await
            .map_err(DoHError::Io)?;
        let expected_server_address = globals.server_address;
        let (min_ttl, max_ttl, err_ttl) = (globals.min_ttl, globals.max_ttl, globals.err_ttl);
        socket
            .send_to(&query, &globals.server_address)
            .map_err(DoHError::Io)
            .await?;
        let mut packet = vec![0; MAX_DNS_RESPONSE_LEN];
        let (len, response_server_address) =
            socket.recv_from(&mut packet).map_err(DoHError::Io).await?;
        if len < MIN_DNS_PACKET_LEN || expected_server_address != response_server_address {
            return Err(DoHError::UpstreamIssue);
        }
        packet.truncate(len);
        let ttl = if dns::is_recoverable_error(&packet) {
            err_ttl
        } else {
            match dns::min_ttl(&packet, min_ttl, max_ttl, err_ttl) {
                Err(_) => return Err(DoHError::UpstreamIssue),
                Ok(ttl) => ttl,
            }
        };
        let packet_len = packet.len();
        let response = Response::builder()
            .header(hyper::header::CONTENT_LENGTH, packet_len)
            .header(hyper::header::CONTENT_TYPE, "application/dns-message")
            .header("X-Padding", utils::padding_string(packet_len, BLOCK_SIZE))
            .header(
                hyper::header::CACHE_CONTROL,
                format!("max-age={}", ttl).as_str(),
            )
            .body(Body::from(packet))
            .unwrap();
        Ok(response)
    }

    async fn entrypoint(self) -> Result<(), Error> {
        let listen_address = self.globals.listen_address;
        let mut listener = TcpListener::bind(&listen_address).await?;
        let path = &self.globals.path;

        #[cfg(feature = "tls")]
        let tls_acceptor = match (&self.globals.tls_cert_path, &self.globals.tls_cert_password) {
            (Some(tls_cert_path), Some(tls_cert_password)) => {
                println!("Listening on https://{}{}", listen_address, path);
                Some(create_tls_acceptor(tls_cert_path, tls_cert_password).unwrap())
            }
            _ => {
                println!("Listening on http://{}{}", listen_address, path);
                None
            }
        };
        #[cfg(not(feature = "tls"))]
        println!("Listening on http://{}{}", listen_address, path);

        let mut server = Http::new();
        server.keep_alive(self.globals.keepalive);
        let listener_service = async {
            while let Some(stream) = listener.incoming().next().await {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => continue,
                };
                let clients_count = self.globals.clients_count.clone();
                if clients_count.increment() > self.globals.max_clients {
                    clients_count.decrement();
                    continue;
                }
                let self_inner = self.clone();
                let server_inner = server.clone();
                tokio::spawn(async move {
                    tokio::time::timeout(
                        self_inner.globals.timeout,
                        server_inner.serve_connection(stream, self_inner),
                    )
                    .await
                    .ok();
                    clients_count.decrement();
                });
            }
            Ok(()) as Result<(), Error>
        };
        listener_service.await?;
        Ok(())
    }
}

fn main() {
    let mut globals = Globals {
        #[cfg(feature = "tls")]
        tls_cert_path: None,
        #[cfg(feature = "tls")]
        tls_cert_password: None,

        listen_address: LISTEN_ADDRESS.parse().unwrap(),
        local_bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        server_address: SERVER_ADDRESS.parse().unwrap(),
        path: PATH.to_string(),
        max_clients: MAX_CLIENTS,
        timeout: Duration::from_secs(TIMEOUT_SEC),
        clients_count: ClientsCount::default(),
        min_ttl: MIN_TTL,
        max_ttl: MAX_TTL,
        err_ttl: ERR_TTL,
        keepalive: true,
        disable_post: false,
    };
    parse_opts(&mut globals);
    let doh = DoH {
        globals: Arc::new(globals),
    };
    let mut runtime_builder = tokio::runtime::Builder::new();
    runtime_builder.enable_all();
    runtime_builder.threaded_scheduler();
    runtime_builder.thread_name("doh-proxy");
    let mut runtime = runtime_builder.build().unwrap();
    runtime.block_on(doh.entrypoint()).unwrap();
}
