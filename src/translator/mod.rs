mod downstream;

mod error;
mod proxy;
mod upstream;

use bitcoin::Address;

use roles_logic_sv2::{parsers::Mining, utils::Mutex};

use std::{net::IpAddr, sync::Arc};
use tokio::sync::mpsc::channel;

use sv1_api::server_to_client;
use tokio::sync::broadcast;

use crate::shared::utils::AbortOnDrop;
use tokio::sync::mpsc::{Receiver as TReceiver, Sender as TSender};

use self::upstream::diff_management::UpstreamDifficultyConfig;
mod task_manager;
use task_manager::TaskManager;

pub async fn start(
    downstreams: TReceiver<(TSender<String>, TReceiver<String>, IpAddr)>,
    pool_connection: TSender<(
        TSender<Mining<'static>>,
        TReceiver<Mining<'static>>,
        Option<Address>,
    )>,
) -> Result<AbortOnDrop, ()> {
    let task_manager = TaskManager::initialize(pool_connection.clone());
    let abortable = task_manager
        .safe_lock(|t| t.get_aborter())
        .unwrap()
        .unwrap();

    let (send_to_up, up_recv_from_here) = channel(crate::TRANSLATOR_BUFFER_SIZE);
    let (up_send_to_here, recv_from_up) = channel(crate::TRANSLATOR_BUFFER_SIZE);
    pool_connection
        .send((up_send_to_here, up_recv_from_here, None))
        .await
        .unwrap();

    // `tx_sv1_bridge` sender is used by `Downstream` to send a `DownstreamMessages` message to
    // `Bridge` via the `rx_sv1_downstream` receiver
    // (Sender<downstream_sv1::DownstreamMessages>, Receiver<downstream_sv1::DownstreamMessages>)
    let (tx_sv1_bridge, rx_sv1_bridge) = channel(crate::TRANSLATOR_BUFFER_SIZE);

    // Sender/Receiver to send a SV2 `SubmitSharesExtended` from the `Bridge` to the `Upstream`
    // (Sender<SubmitSharesExtended<'static>>, Receiver<SubmitSharesExtended<'static>>)
    let (tx_sv2_submit_shares_ext, rx_sv2_submit_shares_ext) =
        channel(crate::TRANSLATOR_BUFFER_SIZE);

    // Sender/Receiver to send a SV2 `SetNewPrevHash` message from the `Upstream` to the `Bridge`
    // (Sender<SetNewPrevHash<'static>>, Receiver<SetNewPrevHash<'static>>)
    let (tx_sv2_set_new_prev_hash, rx_sv2_set_new_prev_hash) =
        channel(crate::TRANSLATOR_BUFFER_SIZE);

    // Sender/Receiver to send a SV2 `NewExtendedMiningJob` message from the `Upstream` to the
    // `Bridge`
    // (Sender<NewExtendedMiningJob<'static>>, Receiver<NewExtendedMiningJob<'static>>)
    let (tx_sv2_new_ext_mining_job, rx_sv2_new_ext_mining_job) =
        channel(crate::TRANSLATOR_BUFFER_SIZE);

    // Sender/Receiver to send a new extranonce from the `Upstream` to this `main` function to be
    // passed to the `Downstream` upon a Downstream role connection
    // (Sender<ExtendedExtranonce>, Receiver<ExtendedExtranonce>)
    let (tx_sv2_extranonce, mut rx_sv2_extranonce) = channel(crate::TRANSLATOR_BUFFER_SIZE);
    let target = Arc::new(Mutex::new(vec![0; 32]));

    // Sender/Receiver to send SV1 `mining.notify` message from the `Bridge` to the `Downstream`
    let (tx_sv1_notify, _): (
        broadcast::Sender<server_to_client::Notify>,
        broadcast::Receiver<server_to_client::Notify>,
    ) = broadcast::channel(crate::TRANSLATOR_BUFFER_SIZE);

    let upstream_diff = UpstreamDifficultyConfig {
        channel_diff_update_interval: crate::CHANNEL_DIFF_UPDTATE_INTERVAL,
        channel_nominal_hashrate: crate::EXPECTED_SV1_HASHPOWER,
    };
    let diff_config = Arc::new(Mutex::new(upstream_diff));

    // Instantiate a new `Upstream` (SV2 Pool)
    let upstream = match upstream::Upstream::new(
        tx_sv2_set_new_prev_hash,
        tx_sv2_new_ext_mining_job,
        crate::MIN_EXTRANONCE_SIZE - 1,
        tx_sv2_extranonce,
        target.clone(),
        diff_config.clone(),
        send_to_up,
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(_e) => {
            todo!();
        }
    };

    let upstream_abortable =
        upstream::Upstream::start(upstream, recv_from_up, rx_sv2_submit_shares_ext)
            .await
            .unwrap();
    TaskManager::add_upstream(task_manager.clone(), upstream_abortable).await?;

    let startup_task = {
        let target = target.clone();
        let task_manager = task_manager.clone();
        tokio::task::spawn(async move {
            let (extended_extranonce, up_id) = rx_sv2_extranonce.recv().await.unwrap();
            loop {
                let target: [u8; 32] = target.safe_lock(|t| t.clone()).unwrap().try_into().unwrap();
                if target != [0; 32] {
                    break;
                };
                tokio::task::yield_now().await;
            }

            // Instantiate a new `Bridge` and begins handling incoming messages
            let b = proxy::Bridge::new(
                tx_sv2_submit_shares_ext,
                tx_sv1_notify.clone(),
                extended_extranonce,
                target,
                up_id,
            );
            let bridge_aborter = proxy::Bridge::start(
                b.clone(),
                rx_sv2_set_new_prev_hash,
                rx_sv2_new_ext_mining_job,
                rx_sv1_bridge,
            )
            .await
            .unwrap();

            let downstream_aborter = downstream::Downstream::accept_connections(
                tx_sv1_bridge,
                tx_sv1_notify,
                b,
                diff_config,
                downstreams,
            )
            .await
            .unwrap();

            TaskManager::add_bridge(task_manager.clone(), bridge_aborter)
                .await
                .unwrap();
            TaskManager::add_downstream_listener(task_manager.clone(), downstream_aborter)
                .await
                .unwrap();
        })
    };
    TaskManager::add_startup_task(task_manager.clone(), startup_task.into())
        .await
        .unwrap();

    Ok(abortable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tracing::info;

    #[tokio::test]
    async fn test_translator() {
        tracing_subscriber::fmt::init();

        // create downstream and pool connection
        let (downstreams_tx, downstreams_rx) = mpsc::channel(10);
        let (pool_tx, mut pool_rx) = mpsc::channel::<(
            TSender<Mining<'static>>,
            TReceiver<Mining<'static>>,
            Option<Address>,
        )>(10);

        // Start translator
        let start_handle = start(downstreams_rx, pool_tx.clone())
            .await
            .expect("Error ");

        // Create channels for interacting with downstream
        let (sender_to_translator_downstream, receive_in_translator_downstream) = mpsc::channel(10);
        let (send_to_mock_miner, _receive_in_mock_miner) = mpsc::channel(10);

        // Create mock miner client ip
        let miner_ip: IpAddr = "127.0.0.1".parse().expect("Unable to parse IP address");

        // Sample sv1 messages
        let mining_subscribe_message =
            "{\"id\": 1, \"method\": \"mining.subscribe\", \"params\": [\"cpuminer/2.5.1\"]}\n"
                .to_string();
        let _submit_msg = r#"{"id": 3, "method": "mining.submit", "params": [“username”, “4f”, “fe36a31b” “504e86ed”,“e9695791”]}"#.to_string();

        // Use sender_to_translator_downstream to send message (received by receive_in_translator_downstream) to the downstream which will get translated by bridge and then send the sv2 message to the pool.
        // The translator downstream then sends pool message sent back to the miner in string format (pool -> bridge-> miner) using send_to_mock_miner (received by receive_in_mock_miner);
        sender_to_translator_downstream
            .send(mining_subscribe_message.clone())
            .await
            .expect("Error sending subscribe message");

        // Make downstreams_tx to communicate using send_to_mock_miner and receive_in_translator_downstream
        downstreams_tx
            .send((
                send_to_mock_miner.clone(),
                receive_in_translator_downstream,
                miner_ip,
            ))
            .await
            .expect("Error sending subscribe message");
        // let (sender_to_pool, mut receive_in_mock_miner_pool) = mpsc::channel(10);
        // let (send_to_mock_miner_pool, receive_in_pool) = mpsc::channel(10);
        // pool_tx
        //     .send((sender_to_pool.clone(), receive_in_pool, None))
        //     .await
        //     .expect("Error sending subscribe message");

        // let hashed: Target = [255_u8; 32].into();
        // let t = SetTarget {
        //     channel_id: 1,
        //     maximum_target: hashed.into(),
        // };
        // send_to_mock_miner_pool
        //     .send(Mining::SetTarget(t))
        //     .await
        //     .expect("Error sending Target");

        // Receive message sent by downstream here
        let (_send, mut receive, _) = pool_rx.recv().await.unwrap();
        let response = receive.recv().await.unwrap();
        info!("Received: {:?}", response);

        if start_handle.is_finished() {
            drop(start_handle);
        }
    }
}
