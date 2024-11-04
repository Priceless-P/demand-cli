use std::net::{IpAddr, SocketAddr};

use crate::shared::{error::Sv1IngressError, utils::AbortOnDrop};
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc::{channel, Receiver, Sender},
};
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{error, info, warn};

pub fn start_listen_for_downstream(
    downstreams: Sender<(Sender<String>, Receiver<String>, IpAddr)>,
) -> AbortOnDrop {
    info!("Starting downstream listner");
    tokio::task::spawn(async move {
        let down_addr: String = crate::SV1_DOWN_LISTEN_ADDR.to_string();
        let downstream_addr: SocketAddr = down_addr.parse().expect("Invalid listen address");
        let downstream_listener = TcpListener::bind(downstream_addr)
            .await
            .expect("impossible to bind downstream");
        while let Ok((stream, addr)) = downstream_listener.accept().await {
            info!("Try to connect {:#?}", addr);
            Downstream::initialize(
                stream,
                crate::MAX_LEN_DOWN_MSG,
                addr.ip(),
                downstreams.clone(),
            );
        }
    })
    .into()
}
struct Downstream {}

impl Downstream {
    pub fn initialize(
        stream: TcpStream,
        max_len_for_downstream_messages: u32,
        address: IpAddr,
        downstreams: Sender<(Sender<String>, Receiver<String>, IpAddr)>,
    ) {
        tokio::spawn(async move {
            info!("spawning downstream");
            let (send_to_upstream, recv) = channel(10);
            let (send, recv_from_upstream) = channel(10);
            downstreams
                .send((send, recv, address))
                .await
                .expect("Translator busy");
            let codec = LinesCodec::new_with_max_length(max_len_for_downstream_messages as usize);
            let framed = Framed::new(stream, codec);
            Self::start(framed, recv_from_upstream, send_to_upstream).await
        });
    }
    async fn start(
        framed: Framed<TcpStream, LinesCodec>,
        receiver: Receiver<String>,
        sender: Sender<String>,
    ) {
        let (writer, reader) = framed.split();
        let result = tokio::select! {
            result1 = Self::receive_from_downstream_and_relay_up(reader, sender) => result1,
            result2 = Self::receive_from_upstream_and_relay_down(writer, receiver) => result2,
        };
        // upstream disconnected make sure to clean everything before exit
        match result {
            Sv1IngressError::DownstreamDropped => (),
            Sv1IngressError::TranslatorDropped => (),
            Sv1IngressError::TaskFailed => (),
        }
    }
    async fn receive_from_downstream_and_relay_up(
        mut recv: SplitStream<Framed<TcpStream, LinesCodec>>,
        send: Sender<String>,
    ) -> Sv1IngressError {
        let task = tokio::spawn(async move {
            while let Some(Ok(message)) = recv.next().await {
                if send.send(message).await.is_err() {
                    error!("Upstream dropped trying to send");
                    return Sv1IngressError::TranslatorDropped;
                }
            }
            warn!("Downstream dropped while trying to send message up");
            Sv1IngressError::DownstreamDropped
        })
        .await;
        match task {
            Ok(err) => err,
            Err(_) => Sv1IngressError::TaskFailed,
        }
    }
    async fn receive_from_upstream_and_relay_down(
        mut send: SplitSink<Framed<TcpStream, LinesCodec>, String>,
        mut recv: Receiver<String>,
    ) -> Sv1IngressError {
        let task = tokio::spawn(async move {
            while let Some(message) = recv.recv().await {
                let message = message.replace(['\n', '\r'], "");
                if send.send(message).await.is_err() {
                    warn!("Downstream dropped while trying to send message down");
                    return Sv1IngressError::DownstreamDropped;
                };
            }
            error!("Upstream dropped trying to receive");
            Sv1IngressError::TranslatorDropped
        })
        .await;
        match task {
            Ok(err) => err,
            Err(_) => Sv1IngressError::TaskFailed,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::ingress::sv1_ingress::Downstream;
    use futures::StreamExt;
    use tokio::io::AsyncReadExt;
    use tokio::{net::TcpStream, sync::mpsc};
    use tokio_util::codec::{Framed, LinesCodec};

    async fn setup_mock_server() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    async fn setup_mock_client(address: std::net::SocketAddr) -> TcpStream {
        let stream = TcpStream::connect(address).await.unwrap();
        stream
    }

    async fn run_test(
        mut mock_client: TcpStream,
        mining_subscribe_message: String,
        mut up_rx: mpsc::Receiver<String>,
        up_tx: mpsc::Sender<String>,
        down_tx: mpsc::Sender<String>,
        response_message: String,
        downstream_handle: tokio::task::JoinHandle<()>,
    ) {
        down_tx
            .send(mining_subscribe_message.clone())
            .await
            .unwrap();

        let received_message = up_rx.recv().await.unwrap();

        assert_eq!(received_message.trim(), mining_subscribe_message.trim());

        up_tx.send(response_message.clone()).await.unwrap();

        let mut buf = vec![0; 1024];
        let len = mock_client.read(&mut buf).await.unwrap();
        let received_response = String::from_utf8_lossy(&buf[..len]);
        assert_eq!(received_response.trim(), response_message.trim());

        downstream_handle.abort();
    }

    #[tokio::test]
    async fn test_sv1_ingress_relay() {
        let (down_tx, up_rx) = mpsc::channel(10);
        let (up_tx, down_rx) = mpsc::channel(10);
        let (listener, address) = setup_mock_server().await;
        let down_tx_clone = down_tx.clone();

        let downstream_handle = tokio::spawn(async move {
            let (mock_server, _) = listener.accept().await.unwrap();
            let codec =
                LinesCodec::new_with_max_length(crate::MAX_LEN_DOWN_MSG.try_into().unwrap());
            let framed = Framed::new(mock_server, codec);
            let (send_to_downstream, recv_from_downstream) = framed.split();

            // Run both relays
            tokio::join!(
                Downstream::receive_from_downstream_and_relay_up(
                    recv_from_downstream,
                    down_tx_clone
                ),
                Downstream::receive_from_upstream_and_relay_down(send_to_downstream, down_rx)
            );
        });

        let mock_client = setup_mock_client(address).await;
        let mining_subscribe_message =
            "{\"id\": 1, \"method\": \"mining.subscribe\", \"params\": [\"cpuminer/2.5.1\"]}\n"
                .to_string();

        let response_message = "{\"id\": 1, \"result\": true, \"error\": null}\n".to_string();
        run_test(
            mock_client,
            mining_subscribe_message,
            up_rx,
            up_tx,
            down_tx,
            response_message,
            downstream_handle,
        )
        .await;
    }
}
