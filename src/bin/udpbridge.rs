use async_std::sync::Mutex;
use ax25ms::router_service_client::RouterServiceClient;
use log::info;
use std::sync::Arc;
use structopt::StructOpt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

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
    #[structopt(short = "r", long = "router")]
    router: String,

    #[structopt(short = "u", long = "udp-dest")]
    dst: String,

    #[structopt(short = "l", long = "udp-listen")]
    listen: String,

    #[structopt(short = "l", long = "router-listen")]
    router_listen: String,
}

#[derive(Debug)]
enum MyError {
    RPCError(tonic::transport::Error),
    RPCStatusError(tonic::Status),
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
        MyError::RPCStatusError(error)
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
    client: Arc<Mutex<RouterServiceClient<tonic::transport::Channel>>>,
    sockregger: mpsc::Sender<mpsc::Sender<Result<ax25ms::Frame, tonic::Status>>>,
}

impl MyServer {
    fn new(
        client: RouterServiceClient<tonic::transport::Channel>,
        sockregger: mpsc::Sender<mpsc::Sender<Result<ax25ms::Frame, tonic::Status>>>,
    ) -> MyServer {
        MyServer {
            client: Arc::new(Mutex::new(client)),
            sockregger,
        }
    }
}

#[tonic::async_trait]
impl ax25ms::router_service_server::RouterService for MyServer {
    //type StreamFramesStream = tonic::Streaming<ax25ms::Frame>;
    type StreamFramesStream = ReceiverStream<Result<ax25ms::Frame, tonic::Status>>;

    async fn stream_frames(
        &self,
        _request: tonic::Request<ax25ms::StreamRequest>,
    ) -> std::result::Result<tonic::Response<Self::StreamFramesStream>, tonic::Status> {
        let (tx, rx) = mpsc::channel(4);
        self.sockregger.send(tx).await.unwrap();
        Ok(tonic::Response::new(ReceiverStream::new(rx)))
    }
    async fn send(
        &self,
        request: tonic::Request<ax25ms::SendRequest>,
    ) -> std::result::Result<tonic::Response<ax25ms::SendResponse>, tonic::Status> {
        self.client
            .lock()
            .await
            .send(tonic::Request::new(ax25ms::SendRequest {
                frame: request.into_inner().frame,
            }))
            .await?;
        Ok(tonic::Response::new(ax25ms::SendResponse {}))
    }
}

async fn udp_server(
    sock: tokio::net::UdpSocket,
    mut rx: mpsc::Receiver<mpsc::Sender<Result<ax25ms::Frame, tonic::Status>>>,
) {
    let mut regged = Vec::new();
    regged.push(rx.recv().await.unwrap());

    let mut buf = Vec::new();
    buf.resize(10000, 0);
    loop {
        let (n, _addr) = sock.recv_from(&mut buf).await.unwrap();
        let frame = ax25ms::Frame {
            payload: buf[0..n].to_vec(),
        };
        info!("Packet from UDP to router (downlink)");
        // TODO: remove the clients that have disconnected.
        for tx in &regged {
            tx.send(Ok(frame.clone())).await.unwrap();
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
        .unwrap();

    // Start UDP -> RPC client.
    let (sockregger, rx) = mpsc::channel(4);
    {
        let sock = tokio::net::UdpSocket::bind(&opt.listen).await?;
        tokio::spawn(async move {
            udp_server(sock, rx).await;
        });
    }

    {
        let addr = opt.router_listen.parse()?;
        let client = RouterServiceClient::connect(opt.router.clone()).await?;
        let srv = MyServer::new(client, sockregger);
        tonic::transport::Server::builder()
            .add_service(ax25ms::router_service_server::RouterServiceServer::new(srv))
            .serve(addr)
            .await?;
    }
    /*
        tokio::spawn(async move {
            let mut buf = Vec::new();
            buf.resize(10000, 0);
            loop {
            let (n, addr) = sock.recv_from(&mut buf).await.unwrap();
            let req = tonic::Request::new(ax25ms::SendRequest {
                frame: Some(ax25ms::Frame {
                    payload: buf[0..n].to_vec(),
                }),
            });
            info!("Packet from UDP to router");
                client.send(req).await.unwrap();
            }
        });
    */

    let sock = tokio::net::UdpSocket::bind("[::]:0").await?;

    // Start router->UDP.
    let mut stream_client = RouterServiceClient::connect(opt.router.clone()).await?;
    info!("Running…");
    let mut stream = stream_client
        .stream_frames(ax25ms::StreamRequest {})
        .await?
        .into_inner();
    loop {
        let frame = stream.next().await.unwrap().unwrap().payload;
        info!("Packet from router to UDP (uplink)");
        sock.send_to(&frame, &opt.dst).await?;
    }
}
