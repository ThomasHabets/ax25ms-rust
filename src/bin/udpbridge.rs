/*
   Copyright 2023 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/
use futures::FutureExt;
use futures::{pin_mut, select};
use log::{error, info};
use structopt::StructOpt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub mod ax25ms {
    tonic::include_proto!("ax25ms");
}
pub mod ax25 {
    tonic::include_proto!("ax25");
}
pub mod aprs {
    tonic::include_proto!("aprs");
}

#[derive(StructOpt, Debug)]
#[structopt()]
struct Opt {
    #[structopt(short = "u", long = "udp-dest")]
    dst: String,

    #[structopt(short = "l", long = "udp-listen")]
    listen: String,

    #[structopt(short = "L", long = "router-listen")]
    router_listen: String,
}

#[derive(Debug)]
enum MyError {
    RPCError(tonic::transport::Error),
    RPCStatus(tonic::Status),
    IOError(std::io::Error),
    AddrParseError(std::net::AddrParseError),
    StreamError(Box<dyn std::error::Error>),
}
impl From<Box<dyn std::error::Error>> for MyError {
    fn from(error: Box<dyn std::error::Error>) -> Self {
        MyError::StreamError(error)
    }
}
impl From<tonic::transport::Error> for MyError {
    fn from(error: tonic::transport::Error) -> Self {
        MyError::RPCError(error)
    }
}
impl From<tonic::Status> for MyError {
    fn from(error: tonic::Status) -> Self {
        MyError::RPCStatus(error)
    }
}
impl From<std::io::Error> for MyError {
    fn from(error: std::io::Error) -> Self {
        MyError::IOError(error)
    }
}
impl From<std::net::AddrParseError> for MyError {
    fn from(error: std::net::AddrParseError) -> Self {
        MyError::AddrParseError(error)
    }
}

struct MyServer {
    sockregger: mpsc::Sender<mpsc::Sender<Result<ax25ms::Frame, tonic::Status>>>,
    dst: String,
    sock: tokio::net::UdpSocket,
}

impl MyServer {
    async fn new(
        sockregger: mpsc::Sender<mpsc::Sender<Result<ax25ms::Frame, tonic::Status>>>,
        dst: String,
    ) -> Result<MyServer, MyError> {
        let sock = tokio::net::UdpSocket::bind("[::]:0").await?;
        Ok(MyServer {
            sockregger,
            dst,
            sock,
        })
    }
}

#[tonic::async_trait]
impl ax25ms::router_service_server::RouterService for MyServer {
    type StreamFramesStream = ReceiverStream<Result<ax25ms::Frame, tonic::Status>>;

    async fn stream_frames(
        &self,
        _request: tonic::Request<ax25ms::StreamRequest>,
    ) -> std::result::Result<tonic::Response<Self::StreamFramesStream>, tonic::Status> {
        let (tx, rx) = mpsc::channel(4);
        self.sockregger
            .send(tx)
            .await
            .map_err(|e| tonic::Status::internal(format!("Failed to register streamer: {}", e)))?;
        Ok(tonic::Response::new(ReceiverStream::new(rx)))
    }
    async fn send(
        &self,
        request: tonic::Request<ax25ms::SendRequest>,
    ) -> std::result::Result<tonic::Response<ax25ms::SendResponse>, tonic::Status> {
        info!("Packet from router to UDP (uplink)");
        let frame = request
            .into_inner()
            .frame
            .ok_or_else(|| tonic::Status::failed_precondition("frame missing"))?;
        self.sock
            .send_to(&frame.payload, &self.dst)
            .await
            .map_err(|e| tonic::Status::unavailable(format!("failed to send UDP packet: {}", e)))?;
        Ok(tonic::Response::new(ax25ms::SendResponse {}))
    }
}

async fn read_udp_packet(sock: &tokio::net::UdpSocket) -> Vec<u8> {
    loop {
        let mut buf = Vec::new();
        buf.resize(10000, 0);
        match sock.recv_from(&mut buf).await {
            Ok((n, _addr)) => return buf[0..n].to_vec(),
            Err(e) => {
                error!("Error reading UDP packet: {}", e);
            }
        };
    }
}

async fn udp_server(
    sock: tokio::net::UdpSocket,
    mut regger: mpsc::Receiver<mpsc::Sender<Result<ax25ms::Frame, tonic::Status>>>,
) {
    let mut regged = Vec::new();
    loop {
        let regger_fut = regger.recv().fuse();
        let sock_fut = read_udp_packet(&sock).fuse();

        pin_mut!(regger_fut, sock_fut);
        select! {
            r = regger_fut => match r {
                Some(r) => regged.push(r),
                None => {
                    error!("Failed to receive on regger port");
                },
            },
            buf = sock_fut  => {
                let frame = ax25ms::Frame {
                    payload: buf.to_vec(),
                };
                info!(
                    "Packet from UDP to router (downlink), {} listeners",
                    regged.len()
                );
                let remove = {
                    let mut ret = Vec::new();
                    for (n, r) in regged.iter().enumerate() {
                        r.send(Ok(frame.clone())).await.unwrap_or_else(|_|{
                            info!("Client disconnected");
                            ret.push(n);
                        });
                    }
                    ret
                };
                for n in remove.iter().rev() {
                    regged.remove(*n);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), MyError> {
    let opt = Opt::from_args();

    stderrlog::new()
        .module(module_path!())
        .quiet(false)
        .verbosity(3)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .expect("Failed to initialize logger");

    // Start UDP -> RPC client.
    let (sockregger, rx) = mpsc::channel(4);
    {
        let sock = tokio::net::UdpSocket::bind(&opt.listen)
            .await
            .expect("Failed to bind to UDP socket");
        tokio::spawn(async move {
            udp_server(sock, rx).await;
        });
    }

    info!("Running…");
    {
        let addr = opt.router_listen.parse().unwrap_or_else(|e| {
            panic!(
                "Failed to parse {} as listen address: {}",
                opt.router_listen, e
            )
        });
        let srv = MyServer::new(sockregger, opt.dst.clone()).await?;
        tonic::transport::Server::builder()
            .add_service(ax25ms::router_service_server::RouterServiceServer::new(srv))
            .serve(addr)
            .await?
    }
    info!("Ending");
    Ok(())
}
