mod task_manager;

use std::collections::HashMap;

use demand_share_accounting_ext::*;
use parser::{PoolExtMessages, ShareAccountingMessages};
use roles_logic_sv2::{mining_sv2::SubmitSharesSuccess, parsers::Mining};
use task_manager::TaskManager;

use crate::shared::utils::AbortOnDrop;

pub async fn start(
    receiver: tokio::sync::mpsc::Receiver<Mining<'static>>,
    sender: tokio::sync::mpsc::Sender<Mining<'static>>,
    up_receiver: tokio::sync::mpsc::Receiver<PoolExtMessages<'static>>,
    up_sender: tokio::sync::mpsc::Sender<PoolExtMessages<'static>>,
) -> AbortOnDrop {
    let task_manager = TaskManager::initialize();
    let abortable = task_manager
        .safe_lock(|t| t.get_aborter())
        .unwrap()
        .unwrap();
    let relay_up_task = relay_up(receiver, up_sender);
    TaskManager::add_relay_up(task_manager.clone(), relay_up_task)
        .await
        .expect("Task Manager failed");
    let relay_down_task = relay_down(up_receiver, sender);
    TaskManager::add_relay_down(task_manager.clone(), relay_down_task)
        .await
        .expect("Task Manager failed");
    abortable
}

pub fn relay_up(
    mut receiver: tokio::sync::mpsc::Receiver<Mining<'static>>,
    up_sender: tokio::sync::mpsc::Sender<PoolExtMessages<'static>>,
) -> AbortOnDrop {
    let task = tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            let msg = PoolExtMessages::Mining(msg);
            if up_sender.send(msg).await.is_err() {
                break;
            }
        }
    });
    task.into()
}

pub fn relay_down(
    mut up_receiver: tokio::sync::mpsc::Receiver<PoolExtMessages<'static>>,
    sender: tokio::sync::mpsc::Sender<Mining<'static>>,
) -> AbortOnDrop {
    let task = tokio::spawn(async move {
        let mut shares_sent_up = HashMap::new();
        while let Some(msg) = up_receiver.recv().await {
            match msg {
                PoolExtMessages::Mining(msg) => {
                    if let Mining::SubmitSharesExtended(m) = &msg {
                        shares_sent_up.insert(m.job_id, m.clone());
                    };
                    if sender.send(msg).await.is_err() {
                        break;
                    }
                }
                PoolExtMessages::ShareAccountingMessages(msg) => {
                    if let ShareAccountingMessages::ShareOk(msg) = msg {
                        let job_id_bytes = msg.ref_job_id.to_le_bytes();
                        let job_id = u32::from_le_bytes(job_id_bytes[4..8].try_into().unwrap());
                        let share_sent_up = shares_sent_up
                            .remove(&job_id)
                            .expect("Pool sent invalid share success");
                        let success = Mining::SubmitSharesSuccess(SubmitSharesSuccess {
                            channel_id: share_sent_up.channel_id,
                            last_sequence_number: share_sent_up.sequence_number,
                            new_submits_accepted_count: 1,
                            new_shares_sum: 1,
                        });
                        if sender.send(success).await.is_err() {
                            break;
                        }
                    };
                }
                _ => panic!("Pool send unexpected message on mining connection"),
            }
        }
    });
    task.into()
}

#[cfg(test)]
#[allow(dead_code)]
mod test {
    use crate::share_accounter::{
        parser::{PoolExtMessages, ShareAccountingMessages},
        start, ShareOk,
    };
    use binary_sv2::Sv2DataType;
    use binary_sv2::B032;
    use roles_logic_sv2::parsers::Mining;

    #[tokio::test]
    async fn test_share_accounter() {
        let (tx_mining, rx_mining) = tokio::sync::mpsc::channel::<Mining<'static>>(10);
        let (tx_pool, rx_pool) = tokio::sync::mpsc::channel::<PoolExtMessages<'static>>(10);

        let tx_mining_clone = tx_mining.clone();
        let tx_pool_clone = tx_pool.clone();

        let abort_handle = tokio::spawn(async move {
            start(rx_mining, tx_mining_clone, rx_pool, tx_pool_clone).await;
        });

        let submit_shares_extended = roles_logic_sv2::mining_sv2::SubmitSharesExtended {
            channel_id: 1,
            sequence_number: 42,
            job_id: 1,
            nonce: 6789,
            ntime: 1609459200,
            version: 2,
            extranonce: B032::from_vec_([0u8; 32].to_vec()).unwrap(),
        };

        let mining_msg = Mining::SubmitSharesExtended(submit_shares_extended);
        tx_mining.send(mining_msg.clone()).await.unwrap();

        //Check if relay_up forwards it to up_sender as PoolExtMessages
        //  let pool_msg = PoolExtMessages::Mining(mining_msg.clone());
        // tx_pool.send(pool_msg).await.unwrap();
        // if let Some(PoolExtMessages::Mining(Mining::SubmitSharesExtended(msg))) = rx_pool.recv().await {
        //     assert_eq!(msg.job_id, mining_msg.job_id);
        // } else {
        //     panic!("Expected Mining message in PoolExtMessages channel");
        // }

        // Check if relay_down forwards it to sender as Mining::SubmitSharesExtended
        // if let Some(Mining::SubmitSharesExtended(received_msg)) = rx_mining.recv().await {
        //     if let Mining::SubmitSharesExtended(expected_msg) = mining_msg {
        //         assert_eq!(received_msg.channel_id, expected_msg.channel_id);
        //         assert_eq!(received_msg.sequence_number, expected_msg.sequence_number);
        //         assert_eq!(received_msg.job_id, expected_msg.job_id);
        //         assert_eq!(received_msg.nonce, expected_msg.nonce);
        //         assert_eq!(received_msg.ntime, expected_msg.ntime);
        //         assert_eq!(received_msg.version, expected_msg.version);
        //         assert_eq!(received_msg.extranonce, expected_msg.extranonce);
        //     } else {
        //         panic!("Expected Mining::SubmitSharesExtended Type");
        //     }
        // } else {
        //     panic!("Expected Mining message");
        // }

        let share = ShareOk {
            ref_job_id: 1,
            share_index: 2,
        };
        // Send a ShareOk message
        let share_ok_msg = ShareAccountingMessages::ShareOk(share);
        let pool_msg = PoolExtMessages::ShareAccountingMessages(share_ok_msg.clone());
        tx_pool.send(pool_msg).await.unwrap();

        // Check if relay_down forwards ShareOk into Mining::SubmitSharesSuccess
        // if let Some(Mining::SubmitSharesSuccess(success_msg)) = rx_mining.recv().await {
        //     // Validate success_msg fields here
        //     assert_eq!(success_msg.new_submits_accepted_count, 1);
        // } else {
        //     panic!("Expected SubmitSharesSuccess message");
        // }

        abort_handle.abort();
    }
}
