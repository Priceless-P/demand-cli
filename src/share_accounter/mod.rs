mod task_manager;

use std::sync::Arc;

use dashmap::DashMap;
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
    let shares_sent_up = Arc::new(DashMap::with_capacity(100));
    let abortable = task_manager
        .safe_lock(|t| t.get_aborter())
        .unwrap()
        .unwrap();
    let relay_up_task = relay_up(receiver, up_sender, shares_sent_up.clone());
    TaskManager::add_relay_up(task_manager.clone(), relay_up_task)
        .await
        .expect("Task Manager failed");
    let relay_down_task = relay_down(up_receiver, sender, shares_sent_up.clone());
    TaskManager::add_relay_down(task_manager.clone(), relay_down_task)
        .await
        .expect("Task Manager failed");
    abortable
}

struct ShareSentUp {
    channel_id: u32,
    sequence_number: u32,
}

fn relay_up(
    mut receiver: tokio::sync::mpsc::Receiver<Mining<'static>>,
    up_sender: tokio::sync::mpsc::Sender<PoolExtMessages<'static>>,
    shares_sent_up: Arc<DashMap<u32, ShareSentUp>>,
) -> AbortOnDrop {
    let task = tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            if let Mining::SubmitSharesExtended(m) = &msg {
                shares_sent_up.insert(
                    m.job_id,
                    ShareSentUp {
                        channel_id: m.channel_id,
                        sequence_number: m.sequence_number,
                    },
                );
            };
            let msg = PoolExtMessages::Mining(msg);
            if up_sender.send(msg).await.is_err() {
                break;
            }
        }
    });
    task.into()
}

fn relay_down(
    mut up_receiver: tokio::sync::mpsc::Receiver<PoolExtMessages<'static>>,
    sender: tokio::sync::mpsc::Sender<Mining<'static>>,
    shares_sent_up: Arc<DashMap<u32, ShareSentUp>>,
) -> AbortOnDrop {
    let task = tokio::spawn(async move {
        while let Some(msg) = up_receiver.recv().await {
            match msg {
                PoolExtMessages::ShareAccountingMessages(msg) => {
                    if let ShareAccountingMessages::ShareOk(msg) = msg {
                        let job_id_bytes = msg.ref_job_id.to_le_bytes();
                        let job_id = u32::from_le_bytes(job_id_bytes[4..8].try_into().unwrap());
                        let share_sent_up = shares_sent_up
                            .remove(&job_id)
                            .expect("Pool sent invalid share success")
                            .1;
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
                PoolExtMessages::Mining(msg) => {
                    if sender.send(msg).await.is_err() {
                        break;
                    }
                }
                _ => panic!("Pool send unexpected message on mining connection"),
            }
        }
    });
    task.into()
}

#[cfg(test)]
mod test {
    use super::start;
    use crate::share_accounter::{
        parser::{PoolExtMessages, ShareAccountingMessages},
        ShareOk,
    };
    use binary_sv2::Sv2DataType;
    use binary_sv2::B032;
    use roles_logic_sv2::parsers::Mining;
    use tokio::sync::mpsc;

    fn get_submit_share_extended() -> Mining<'static> {
        let submit_shares_extended = roles_logic_sv2::mining_sv2::SubmitSharesExtended {
            channel_id: 1,
            sequence_number: 42,
            job_id: 1,
            nonce: 6789,
            ntime: 1609459200,
            version: 2,
            extranonce: B032::from_vec_([0u8; 32].to_vec()).unwrap(),
        };

        Mining::SubmitSharesExtended(submit_shares_extended)
    }

    #[tokio::test]
    async fn test_share_accounter() {
        let (send_to_share_accounter, mut from_pool_receiver) = mpsc::channel(10);
        let (_from_pool_sender, receive_from_pool) = mpsc::channel(10);
        let (send_to_pool, mut receive_from_share_accounter) = mpsc::channel(10);
        let (_from_downstream_sender, pool_receiver) = mpsc::channel(10);

        let start_handle = start(
            pool_receiver,
            send_to_pool.clone(),
            receive_from_pool,
            send_to_share_accounter.clone(),
        );

        let submit_shares_msg = get_submit_share_extended();

        // Send SubmitSharesExtended message to pool to mimic mining share submission
        send_to_pool.send(submit_shares_msg.clone()).await.unwrap();

        // Check that the PoolExtMessages::Mining message sent by Pool was received by share accounter and its value correspond with the submit_shares_msg sent
        if let Some(Mining::SubmitSharesExtended(received_msg)) =
            receive_from_share_accounter.recv().await
        {
            if let Mining::SubmitSharesExtended(expected_msg) = submit_shares_msg.clone() {
                assert_eq!(received_msg.channel_id, expected_msg.channel_id);
                assert_eq!(received_msg.sequence_number, expected_msg.sequence_number);
                assert_eq!(received_msg.job_id, expected_msg.job_id);
                assert_eq!(received_msg.nonce, expected_msg.nonce);
                assert_eq!(received_msg.ntime, expected_msg.ntime);
                assert_eq!(received_msg.version, expected_msg.version);
                assert_eq!(received_msg.extranonce, expected_msg.extranonce);
            } else {
                panic!("Expected Mining::SubmitSharesExtended Type");
            }
        } else {
            panic!("Expected Mining message");
        }

        // Create and send a ShareOk message to mimic pool validation of share
        let share = match submit_shares_msg {
            Mining::SubmitSharesExtended(ref _msg) => ShareOk {
                ref_job_id: 1,
                share_index: 2,
            },
            _ => panic!("Expected SubmitSharesExtended variant"),
        };

        let share_ok_msg = ShareAccountingMessages::ShareOk(share);

        send_to_share_accounter
            .send(PoolExtMessages::ShareAccountingMessages(
                share_ok_msg.clone(),
            ))
            .await
            .unwrap();

        let response = from_pool_receiver.recv().await;

        // Check if that the sent :ShareOk message is received by the pool
        if let Some(PoolExtMessages::ShareAccountingMessages(ShareAccountingMessages::ShareOk(
            success_msg,
        ))) = response.clone()
        {
            assert_eq!(success_msg.ref_job_id, 1);
            assert_eq!(success_msg.share_index, 2);
        } else {
            panic!("Expected SubmitSharesSuccess message");
        }

        // Check if the mining client receives success message  back
        if let Some(PoolExtMessages::Mining(Mining::SubmitSharesSuccess(success))) = response {
            assert_eq!(success.channel_id, 1);
            assert_eq!(success.last_sequence_number, 42);
        }

        tokio::join!(start_handle);
    }
}
