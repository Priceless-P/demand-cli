pub mod message_handler;
mod task_manager;
use binary_sv2::{Seq0255, Seq064K, B016M, B064K, U256};
use bitcoin::{util::psbt::serialize::Deserialize, Transaction};
use codec_sv2::{HandshakeRole, Initiator, StandardEitherFrame, StandardSv2Frame};
use demand_sv2_connection::noise_connection_tokio::Connection;
use roles_logic_sv2::{
    handlers::SendTo_,
    job_declaration_sv2::{AllocateMiningJobTokenSuccess, SubmitSolutionJd},
    mining_sv2::SubmitSharesExtended,
    parsers::{JobDeclaration, PoolMessages},
    template_distribution_sv2::SetNewPrevHash,
    utils::{hash_lists_tuple, Mutex},
};
use std::{collections::HashMap, convert::TryInto};
use task_manager::TaskManager;
use tokio::sync::mpsc::{Receiver as TReceiver, Sender as TSender};
use tracing::{error, info};

use async_recursion::async_recursion;
use nohash_hasher::BuildNoHashHasher;
use roles_logic_sv2::{
    handlers::job_declaration::ParseServerJobDeclarationMessages,
    job_declaration_sv2::{AllocateMiningJobToken, DeclareMiningJob},
    template_distribution_sv2::NewTemplate,
    utils::Id,
};
use std::{net::SocketAddr, sync::Arc};

pub type Message = PoolMessages<'static>;
pub type SendTo = SendTo_<JobDeclaration<'static>, ()>;
pub type StdFrame = StandardSv2Frame<Message>;

mod setup_connection;
use setup_connection::SetupConnectionHandler;

use crate::shared::utils::AbortOnDrop;

use super::{error::Error, mining_upstream::Upstream};

#[derive(Debug, Clone)]
pub struct LastDeclareJob {
    declare_job: DeclareMiningJob<'static>,
    template: NewTemplate<'static>,
    coinbase_pool_output: Vec<u8>,
    tx_list: Seq064K<'static, B016M<'static>>,
}

#[derive(Debug)]
pub struct JobDeclarator {
    sender: TSender<StandardEitherFrame<PoolMessages<'static>>>,
    allocated_tokens: Vec<AllocateMiningJobTokenSuccess<'static>>,
    req_ids: Id,
    min_extranonce_size: u16,
    // (Sent DeclareMiningJob, is future, template id, merkle path)
    last_declare_mining_jobs_sent: HashMap<u32, Option<LastDeclareJob>>,
    last_set_new_prev_hash: Option<SetNewPrevHash<'static>>,
    set_new_prev_hash_counter: u8,
    #[allow(clippy::type_complexity)]
    future_jobs: HashMap<
        u64,
        (
            DeclareMiningJob<'static>,
            Seq0255<'static, U256<'static>>,
            NewTemplate<'static>,
            // pool's outputs
            Vec<u8>,
        ),
        BuildNoHashHasher<u64>,
    >,
    up: Arc<Mutex<Upstream>>,
    pub coinbase_tx_prefix: B064K<'static>,
    pub coinbase_tx_suffix: B064K<'static>,
    pub task_manager: Arc<Mutex<TaskManager>>,
}

impl JobDeclarator {
    pub async fn new(
        address: SocketAddr,
        authority_public_key: [u8; 32],
        up: Arc<Mutex<Upstream>>,
    ) -> Result<(Arc<Mutex<Self>>, AbortOnDrop), Error> {
        let stream = tokio::net::TcpStream::connect(address).await?;
        let initiator = Initiator::from_raw_k(authority_public_key)?;
        let (mut receiver, mut sender, _, _) =
            Connection::new(stream, HandshakeRole::Initiator(initiator))
                .await
                .expect("impossible to connect");

        SetupConnectionHandler::setup(&mut receiver, &mut sender, address)
            .await
            .unwrap();

        info!("JD CONNECTED");

        let min_extranonce_size = crate::MIN_EXTRANONCE_SIZE;

        let task_manager = TaskManager::initialize();
        let abortable = task_manager
            .safe_lock(|t| t.get_aborter())
            .unwrap()
            .unwrap();
        let self_ = Arc::new(Mutex::new(JobDeclarator {
            sender,
            allocated_tokens: vec![],
            req_ids: Id::new(),
            min_extranonce_size,
            last_declare_mining_jobs_sent: HashMap::with_capacity(2),
            last_set_new_prev_hash: None,
            future_jobs: HashMap::with_hasher(BuildNoHashHasher::default()),
            up,
            coinbase_tx_prefix: vec![].try_into().unwrap(),
            coinbase_tx_suffix: vec![].try_into().unwrap(),
            set_new_prev_hash_counter: 0,
            task_manager,
        }));

        Self::allocate_tokens(&self_, 2).await;
        Self::on_upstream_message(self_.clone(), receiver).await;
        Ok((self_, abortable))
    }

    fn get_last_declare_job_sent(self_mutex: &Arc<Mutex<Self>>, request_id: u32) -> LastDeclareJob {
        let id = self_mutex
            .safe_lock(|s| s.last_declare_mining_jobs_sent.remove(&request_id).clone())
            .unwrap();
        id.expect("Impossible to get last declare job sent")
            .clone()
            .expect("This is ok")
    }

    fn update_last_declare_job_sent(
        self_mutex: &Arc<Mutex<Self>>,
        request_id: u32,
        j: LastDeclareJob,
    ) {
        self_mutex
            .safe_lock(|s| {
                //check hashmap size in order to not let it grow indefinetely
                if s.last_declare_mining_jobs_sent.len() < 10 {
                    s.last_declare_mining_jobs_sent.insert(request_id, Some(j));
                } else if let Some(min_key) = s.last_declare_mining_jobs_sent.keys().min().cloned()
                {
                    s.last_declare_mining_jobs_sent.remove(&min_key);
                    s.last_declare_mining_jobs_sent.insert(request_id, Some(j));
                }
            })
            .unwrap();
    }

    #[async_recursion]
    pub async fn get_last_token(
        self_mutex: &Arc<Mutex<Self>>,
    ) -> AllocateMiningJobTokenSuccess<'static> {
        let mut token_len = self_mutex.safe_lock(|s| s.allocated_tokens.len()).unwrap();
        let task_manager = self_mutex.safe_lock(|s| s.task_manager.clone()).unwrap();
        match token_len {
            0 => {
                {
                    let task = {
                        let self_mutex = self_mutex.clone();
                        tokio::task::spawn(async move {
                            Self::allocate_tokens(&self_mutex, 2).await;
                        })
                    };
                    TaskManager::add_allocate_tokens(task_manager, task.into())
                        .await
                        .unwrap();
                }

                // we wait for token allocation to avoid infinite recursion
                while token_len == 0 {
                    tokio::task::yield_now().await;
                    token_len = self_mutex.safe_lock(|s| s.allocated_tokens.len()).unwrap();
                }

                Self::get_last_token(self_mutex).await
            }
            1 => {
                {
                    let task = {
                        let self_mutex = self_mutex.clone();
                        tokio::task::spawn(async move {
                            Self::allocate_tokens(&self_mutex, 1).await;
                        })
                    };
                    TaskManager::add_allocate_tokens(task_manager, task.into())
                        .await
                        .unwrap();
                }
                // There is a token, unwrap is safe
                self_mutex
                    .safe_lock(|s| s.allocated_tokens.pop())
                    .unwrap()
                    .unwrap()
            }
            // There are tokens, unwrap is safe
            _ => self_mutex
                .safe_lock(|s| s.allocated_tokens.pop())
                .unwrap()
                .unwrap(),
        }
    }

    pub async fn on_new_template(
        self_mutex: &Arc<Mutex<Self>>,
        template: NewTemplate<'static>,
        token: Vec<u8>,
        tx_list_: Seq064K<'static, B016M<'static>>,
        excess_data: B064K<'static>,
        coinbase_pool_output: Vec<u8>,
    ) {
        let (id, _, sender) = self_mutex
            .safe_lock(|s| (s.req_ids.next(), s.min_extranonce_size, s.sender.clone()))
            .unwrap();
        // TODO: create right nonce
        let tx_short_hash_nonce = 0;
        let mut tx_list: Vec<Transaction> = Vec::new();
        for tx in tx_list_.to_vec() {
            //TODO remove unwrap
            let tx = Transaction::deserialize(&tx).unwrap();
            tx_list.push(tx);
        }
        let declare_job = DeclareMiningJob {
            request_id: id,
            mining_job_token: token.try_into().unwrap(),
            version: template.version,
            coinbase_prefix: self_mutex
                .safe_lock(|s| s.coinbase_tx_prefix.clone())
                .unwrap(),
            coinbase_suffix: self_mutex
                .safe_lock(|s| s.coinbase_tx_suffix.clone())
                .unwrap(),
            tx_short_hash_nonce,
            tx_short_hash_list: hash_lists_tuple(tx_list.clone(), tx_short_hash_nonce).0,
            tx_hash_list_hash: hash_lists_tuple(tx_list.clone(), tx_short_hash_nonce).1,
            excess_data, // request transaction data
        };
        let last_declare = LastDeclareJob {
            declare_job: declare_job.clone(),
            template,
            coinbase_pool_output,
            tx_list: tx_list_.clone(),
        };
        Self::update_last_declare_job_sent(self_mutex, id, last_declare);
        let frame: StdFrame =
            PoolMessages::JobDeclaration(JobDeclaration::DeclareMiningJob(declare_job))
                .try_into()
                .unwrap();
        sender.send(frame.into()).await.unwrap();
    }

    pub async fn on_upstream_message(
        self_mutex: Arc<Mutex<Self>>,
        mut receiver: TReceiver<StandardEitherFrame<PoolMessages<'static>>>,
    ) {
        let up = self_mutex.safe_lock(|s| s.up.clone()).unwrap();
        let task_manager = self_mutex.safe_lock(|s| s.task_manager.clone()).unwrap();
        let main_task = tokio::task::spawn(async move {
            loop {
                let mut incoming: StdFrame = receiver
                    .recv()
                    .await
                    .unwrap()
                    .try_into()
                    .unwrap_or_else(|_| std::process::abort());
                let message_type = incoming.get_header().unwrap().msg_type();
                let payload = incoming.payload();
                let next_message_to_send =
                    ParseServerJobDeclarationMessages::handle_message_job_declaration(
                        self_mutex.clone(),
                        message_type,
                        payload,
                    );
                match next_message_to_send {
                    Ok(SendTo::None(Some(JobDeclaration::DeclareMiningJobSuccess(m)))) => {
                        let new_token = m.new_mining_job_token;
                        let last_declare =
                            Self::get_last_declare_job_sent(&self_mutex, m.request_id);
                        let mut last_declare_mining_job_sent = last_declare.declare_job;
                        let is_future = last_declare.template.future_template;
                        let id = last_declare.template.template_id;
                        let merkle_path = last_declare.template.merkle_path.clone();
                        let template = last_declare.template;

                        // TODO where we should have a sort of signaling that is green after
                        // that the token has been updated so that on_set_new_prev_hash know it
                        // and can decide if send the set_custom_job or not
                        if is_future {
                            last_declare_mining_job_sent.mining_job_token = new_token;
                            self_mutex
                                .safe_lock(|s| {
                                    s.future_jobs.insert(
                                        id,
                                        (
                                            last_declare_mining_job_sent,
                                            merkle_path,
                                            template,
                                            last_declare.coinbase_pool_output,
                                        ),
                                    );
                                })
                                .unwrap();
                        } else {
                            let set_new_prev_hash = self_mutex
                                .safe_lock(|s| s.last_set_new_prev_hash.clone())
                                .unwrap();
                            let mut template_outs = template.coinbase_tx_outputs.to_vec();
                            let mut pool_outs = last_declare.coinbase_pool_output;
                            pool_outs.append(&mut template_outs);
                            match set_new_prev_hash {
                                Some(p) => Upstream::set_custom_jobs(
                                    &up,
                                    last_declare_mining_job_sent,
                                    p,
                                    merkle_path,
                                    new_token,
                                    template.coinbase_tx_version,
                                    template.coinbase_prefix,
                                    template.coinbase_tx_input_sequence,
                                    template.coinbase_tx_value_remaining,
                                    pool_outs,
                                    template.coinbase_tx_locktime,
                                    template.template_id
                                    ).await.unwrap(),
                                None => panic!("Invalid state we received a NewTemplate not future, without having received a set new prev hash")
                            }
                        }
                    }
                    Ok(SendTo::None(Some(JobDeclaration::DeclareMiningJobError(m)))) => {
                        error!("Job is not verified: {:?}", m);
                    }
                    Ok(SendTo::None(None)) => (),
                    Ok(SendTo::Respond(m)) => {
                        let sv2_frame: StdFrame =
                            PoolMessages::JobDeclaration(m).try_into().unwrap();
                        let sender = self_mutex.safe_lock(|self_| self_.sender.clone()).unwrap();
                        sender.send(sv2_frame.into()).await.unwrap();
                    }
                    Ok(_) => unreachable!(),
                    Err(_) => todo!(),
                }
            }
        });
        TaskManager::add_allocate_tokens(task_manager, main_task.into())
            .await
            .unwrap();
    }

    pub async fn on_set_new_prev_hash(
        self_mutex: Arc<Mutex<Self>>,
        set_new_prev_hash: SetNewPrevHash<'static>,
    ) {
        let task_manager = self_mutex.safe_lock(|s| s.task_manager.clone()).unwrap();
        let task = tokio::task::spawn(async move {
            let id = set_new_prev_hash.template_id;
            let _ = self_mutex.safe_lock(|s| {
                s.last_set_new_prev_hash = Some(set_new_prev_hash.clone());
                s.set_new_prev_hash_counter += 1;
            });
            let (job, up, merkle_path, template, mut pool_outs) = loop {
                match self_mutex
                    .safe_lock(|s| {
                        if s.set_new_prev_hash_counter > 1
                            && s.last_set_new_prev_hash != Some(set_new_prev_hash.clone())
                        //it means that a new prev_hash is arrived while the previous hasn't exited the loop yet
                        {
                            s.set_new_prev_hash_counter -= 1;
                            Some(None)
                        } else {
                            s.future_jobs.remove(&id).map(
                                |(job, merkle_path, template, pool_outs)| {
                                    s.future_jobs =
                                        HashMap::with_hasher(BuildNoHashHasher::default());
                                    s.set_new_prev_hash_counter -= 1;
                                    Some((job, s.up.clone(), merkle_path, template, pool_outs))
                                },
                            )
                        }
                    })
                    .unwrap()
                {
                    Some(Some(future_job_tuple)) => break future_job_tuple,
                    Some(None) => return,
                    None => {}
                };
                tokio::task::yield_now().await;
            };
            let signed_token = job.mining_job_token.clone();
            let mut template_outs = template.coinbase_tx_outputs.to_vec();
            pool_outs.append(&mut template_outs);
            Upstream::set_custom_jobs(
                &up,
                job,
                set_new_prev_hash,
                merkle_path,
                signed_token,
                template.coinbase_tx_version,
                template.coinbase_prefix,
                template.coinbase_tx_input_sequence,
                template.coinbase_tx_value_remaining,
                pool_outs,
                template.coinbase_tx_locktime,
                template.template_id,
            )
            .await
            .unwrap();
        });
        TaskManager::add_allocate_tokens(task_manager, task.into())
            .await
            .unwrap();
    }

    async fn allocate_tokens(self_mutex: &Arc<Mutex<Self>>, token_to_allocate: u32) {
        for i in 0..token_to_allocate {
            let message = JobDeclaration::AllocateMiningJobToken(AllocateMiningJobToken {
                user_identifier: "todo".to_string().try_into().unwrap(),
                request_id: i,
            });
            let sender = self_mutex.safe_lock(|s| s.sender.clone()).unwrap();
            // Safe unwrap message is build above and is valid, below can never panic
            let frame: StdFrame = PoolMessages::JobDeclaration(message).try_into().unwrap();
            // TODO join re
            sender.send(frame.into()).await.unwrap();
        }
    }
    pub async fn on_solution(
        self_mutex: &Arc<Mutex<Self>>,
        solution: SubmitSharesExtended<'static>,
    ) {
        let prev_hash = self_mutex
            .safe_lock(|s| s.last_set_new_prev_hash.clone())
            .expect("Poison lock error")
            .expect("Impossible to get last p hash");
        let solution = SubmitSolutionJd {
            extranonce: solution.extranonce,
            prev_hash: prev_hash.prev_hash,
            ntime: solution.ntime,
            nonce: solution.nonce,
            nbits: prev_hash.n_bits,
            version: solution.version,
        };
        let frame: StdFrame =
            PoolMessages::JobDeclaration(JobDeclaration::SubmitSolution(solution))
                .try_into()
                .expect("Infallible operation");
        let sender = self_mutex
            .safe_lock(|s| s.sender.clone())
            .expect("Poison lock error");
        sender
            .send(frame.into())
            .await
            .expect("JDC Sub solution receiver unavailable");
    }
}

#[cfg(test)]
pub mod test {
    use std::{net::ToSocketAddrs, sync::Arc};

    use key_utils::Secp256k1PublicKey;
    use roles_logic_sv2::{parsers::Mining, utils::Mutex};
    use tokio::sync::mpsc;

    use super::*;
    use crate::jd_client::mining_upstream::Upstream;

    // Helper to create the JobDeclarator instance
    pub async fn setup_jd(
        up_sender: mpsc::Sender<Mining<'static>>,
    ) -> (Arc<Mutex<JobDeclarator>>, AbortOnDrop, Arc<Mutex<Upstream>>) {
        let addr = crate::POOL_ADDRESS
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap();
        let auth_pub_k: Secp256k1PublicKey =
            crate::AUTH_PUB_KEY.parse().expect("Invalid public key");
        let upstream = setup_upstream(up_sender.clone()).await;

        let (jd, jd_aborter) = JobDeclarator::new(addr, auth_pub_k.into_bytes(), upstream.clone())
            .await
            .expect("Unable to create JobDeclarator");
        (jd, jd_aborter, upstream)
    }

    // Helper to create the Upstream instance
    async fn setup_upstream(up_sender: mpsc::Sender<Mining<'static>>) -> Arc<Mutex<Upstream>> {
        Upstream::new(0, crate::POOL_SIGNATURE.to_string(), up_sender)
            .await
            .expect("Failed to create upstream")
    }

    // Helper to create a template
    pub fn create_template() -> NewTemplate<'static> {
        NewTemplate {
            version: 1,
            future_template: false,
            template_id: 1,
            coinbase_tx_version: 2,
            coinbase_prefix: vec![].try_into().unwrap(),
            coinbase_tx_input_sequence: 0,
            coinbase_tx_value_remaining: 500,
            coinbase_tx_outputs: vec![].try_into().unwrap(),
            coinbase_tx_locktime: 0,
            merkle_path: vec![].try_into().unwrap(),
            coinbase_tx_outputs_count: 1,
        }
    }

    // Helper to create a new prev hash
    pub fn get_new_prev_hash(template_id: u64) -> SetNewPrevHash<'static> {
        let prev_hash = SetNewPrevHash {
            prev_hash: [
                3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
                3, 3, 3, 3,
            ]
            .into(),
            n_bits: 9,
            template_id,
            header_timestamp: 989898,
            target: vec![0; 32].try_into().unwrap(),
        };
        prev_hash
    }

    #[tokio::test]
    async fn test_jd_client_get_last_token() {
        let (up_sender, _) = mpsc::channel(10);
        let (jd, _jd_abortable, _) = setup_jd(up_sender).await;
        let last_token = JobDeclarator::get_last_token(&jd).await;
        assert_ne!(last_token.mining_job_token.len(), 0);
    }

    #[tokio::test]
    async fn test_jd_client_on_new_template() {
        let (up_sender, _) = mpsc::channel(10);
        let (jd, _jd_abortable, _) = setup_jd(up_sender).await;

        let template = create_template();
        let last_token = JobDeclarator::get_last_token(&jd).await;
        let coinbase_pool_output = last_token.coinbase_output.to_vec();

        JobDeclarator::on_new_template(
            &jd,
            template.clone(),
            last_token.mining_job_token.to_vec(),
            vec![].try_into().unwrap(),
            vec![].try_into().unwrap(),
            coinbase_pool_output.clone(),
        )
        .await;

        let updated_declare_job = jd
            .clone()
            .safe_lock(|s| s.last_declare_mining_jobs_sent.clone())
            .expect("Failed to lock jd for last_declare_mining_jobs_sent");

        // Check that last declare job is same as template we created
        let updated_declare_job = updated_declare_job.get(&1).unwrap().clone().unwrap();
        assert_eq!(updated_declare_job.template.version, template.version);
        assert_eq!(
            updated_declare_job.coinbase_pool_output,
            coinbase_pool_output
        );
    }

    #[tokio::test]
    async fn test_jd_client_on_set_new_prev_hash() {
        let (up_sender, _) = mpsc::channel(10);
        let (jd, _jd_abortable, _) = setup_jd(up_sender).await;

        JobDeclarator::on_set_new_prev_hash(jd.clone(), get_new_prev_hash(1)).await;

        let set_new_prev_hash_counter = jd
            .clone()
            .safe_lock(|s| s.set_new_prev_hash_counter.clone())
            .expect("Failed to lock jd for set_new_prev_hash_counter");
        assert_eq!(set_new_prev_hash_counter, 0);

        // check that future jobs is empty
        let future_jobs = jd
            .clone()
            .safe_lock(|s| s.future_jobs.clone())
            .expect("Failed to lock jd for future_jobs");
        assert!(future_jobs.is_empty(), "Expected future_jobs to be cleared");
    }

    #[tokio::test]
    // Fails
    // Reason: Disconnected from client while reading : early eof - 98.67.129.74:2000
    async fn test_jd_client_jd_on_solution() {
        let (up_sender, mut receiver) = mpsc::channel(10);
        let (jd, _jd_abortable, _) = setup_jd(up_sender.clone()).await;

        let solution = SubmitSharesExtended {
            extranonce: vec![1, 2, 3].try_into().unwrap(),
            ntime: 0,
            nonce: 0,
            version: 0,
            channel_id: 1,
            sequence_number: 0,
            job_id: 1,
        };

        // Insert a previous hash for testing
        jd.clone()
            .safe_lock(|s| s.last_set_new_prev_hash = Some(get_new_prev_hash(1)))
            .expect("Failed to set new prev hash");

        JobDeclarator::on_solution(&jd, solution.clone()).await;

        // Check that the submitshareextended msg received contains same value as solution
        if let Some(received) = receiver.recv().await {
            if let Mining::SubmitSharesExtended(submit_solution) = received.into() {
                assert_eq!(
                    submit_solution.extranonce, solution.extranonce,
                    "Extranonce mismatch"
                );
                assert_eq!(submit_solution.ntime, solution.ntime, "Ntime mismatch");
                assert_eq!(submit_solution.nonce, solution.nonce, "Nonce mismatch");
                assert_eq!(
                    submit_solution.version, solution.version,
                    "Version mismatch"
                );
            } else {
                panic!("Expected SubmitSolution frame, got something else");
            }
        } else {
            panic!("No frame received, expected SubmitSolution frame");
        }
    }
}
