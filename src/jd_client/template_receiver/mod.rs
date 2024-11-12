mod task_manager;
use crate::jd_client::mining_downstream::DownstreamMiningNode as Downstream;
use crate::shared::utils::AbortOnDrop;

use super::job_declarator::JobDeclarator;
use bitcoin::{consensus::Encodable, TxOut};
use codec_sv2::{HandshakeRole, Initiator, StandardEitherFrame, StandardSv2Frame};
use demand_sv2_connection::noise_connection_tokio::Connection;
use key_utils::Secp256k1PublicKey;
use roles_logic_sv2::{
    handlers::{template_distribution::ParseServerTemplateDistributionMessages, SendTo_},
    job_declaration_sv2::AllocateMiningJobTokenSuccess,
    parsers::{PoolMessages, TemplateDistribution},
    template_distribution_sv2::{
        CoinbaseOutputDataSize, NewTemplate, RequestTransactionData, SubmitSolution,
    },
    utils::Mutex,
};
use setup_connection::SetupConnectionHandler;
use std::{convert::TryInto, net::SocketAddr, sync::Arc};
use task_manager::TaskManager;
use tokio::sync::mpsc::{Receiver as TReceiver, Sender as TSender};
use tracing::{error, info, warn};

mod message_handler;
mod setup_connection;

pub type SendTo = SendTo_<roles_logic_sv2::parsers::TemplateDistribution<'static>, ()>;
pub type Message = PoolMessages<'static>;
pub type StdFrame = StandardSv2Frame<Message>;
pub type EitherFrame = StandardEitherFrame<Message>;

pub struct TemplateRx {
    sender: TSender<EitherFrame>,
    /// Allows the tp recv to communicate back to the main thread any status updates
    /// that would interest the main thread for error handling
    jd: Option<Arc<Mutex<super::job_declarator::JobDeclarator>>>,
    down: Arc<Mutex<Downstream>>,
    new_template_message: Option<NewTemplate<'static>>,
    miner_coinbase_output: Vec<u8>,
    test_only_do_not_send_solution_to_tp: bool,
}

impl TemplateRx {
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        address: SocketAddr,
        solution_receiver: TReceiver<SubmitSolution<'static>>,
        jd: Option<Arc<Mutex<super::job_declarator::JobDeclarator>>>,
        down: Arc<Mutex<Downstream>>,
        miner_coinbase_outputs: Vec<TxOut>,
        authority_public_key: Option<Secp256k1PublicKey>,
        test_only_do_not_send_solution_to_tp: bool,
    ) -> AbortOnDrop {
        let mut encoded_outputs = vec![];
        miner_coinbase_outputs
            .consensus_encode(&mut encoded_outputs)
            .expect("Invalid coinbase output in config");
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();

        let initiator = match authority_public_key {
            Some(pub_key) => Initiator::from_raw_k(pub_key.into_bytes()),
            None => Initiator::without_pk(),
        }
        .unwrap_or_else(|_| {
            error!("Impossible to connect to TP wait few second and retry");
            std::process::abort();
        });
        let (mut receiver, mut sender, _, _) =
            Connection::new(stream, HandshakeRole::Initiator(initiator))
                .await
                .unwrap_or_else(|_| {
                    error!("Impossible to connect to TP wait few second and retry");
                    std::process::abort();
                });

        info!("Template Receiver try to set up connection");
        SetupConnectionHandler::setup(&mut receiver, &mut sender, address)
            .await
            .unwrap_or_else(|_| {
                error!("Impossible to connect to TP wait few second and retry");
                std::process::abort();
            });
        info!("Template Receiver connection set up");

        let self_mutex = Arc::new(Mutex::new(Self {
            sender: sender.clone(),
            jd,
            down,
            new_template_message: None,
            miner_coinbase_output: encoded_outputs,
            test_only_do_not_send_solution_to_tp,
        }));

        let task_manager = TaskManager::initialize();
        let abortable = task_manager
            .safe_lock(|t| t.get_aborter())
            .unwrap()
            .unwrap();
        let on_new_solution_task =
            tokio::task::spawn(Self::on_new_solution(self_mutex.clone(), solution_receiver));
        TaskManager::add_on_new_solution(task_manager.clone(), on_new_solution_task.into())
            .await
            .unwrap();
        let main_task = Self::start_templates(self_mutex, receiver).await;
        TaskManager::add_main_task(task_manager, main_task)
            .await
            .unwrap();
        abortable
    }

    pub async fn send(self_: &Arc<Mutex<Self>>, sv2_frame: StdFrame) {
        let either_frame = sv2_frame.into();
        let sender_to_tp = self_.safe_lock(|self_| self_.sender.clone()).unwrap();
        match sender_to_tp.send(either_frame).await {
            Ok(_) => (),
            Err(e) => panic!("{:?}", e),
        }
    }

    pub async fn send_max_coinbase_size(self_mutex: &Arc<Mutex<Self>>, size: u32) {
        let coinbase_output_data_size = PoolMessages::TemplateDistribution(
            TemplateDistribution::CoinbaseOutputDataSize(CoinbaseOutputDataSize {
                coinbase_output_max_additional_size: size,
            }),
        );
        let frame: StdFrame = coinbase_output_data_size.try_into().unwrap();
        Self::send(self_mutex, frame).await;
    }

    pub async fn send_tx_data_request(
        self_mutex: &Arc<Mutex<Self>>,
        new_template: NewTemplate<'static>,
    ) {
        let tx_data_request = PoolMessages::TemplateDistribution(
            TemplateDistribution::RequestTransactionData(RequestTransactionData {
                template_id: new_template.template_id,
            }),
        );
        let frame: StdFrame = tx_data_request.try_into().unwrap();
        Self::send(self_mutex, frame).await;
    }

    async fn get_last_token(
        jd: Option<Arc<Mutex<JobDeclarator>>>,
        miner_coinbase_output: &[u8],
    ) -> AllocateMiningJobTokenSuccess<'static> {
        if let Some(jd) = jd {
            super::job_declarator::JobDeclarator::get_last_token(&jd).await
        } else {
            AllocateMiningJobTokenSuccess {
                request_id: 0,
                mining_job_token: vec![0; 32].try_into().unwrap(),
                coinbase_output_max_additional_size: 100,
                coinbase_output: miner_coinbase_output.to_vec().try_into().unwrap(),
                async_mining_allowed: true,
            }
        }
    }

    pub async fn start_templates(
        self_mutex: Arc<Mutex<Self>>,
        mut receiver: TReceiver<EitherFrame>,
    ) -> AbortOnDrop {
        let jd = self_mutex.safe_lock(|s| s.jd.clone()).unwrap();
        let down = self_mutex.safe_lock(|s| s.down.clone()).unwrap();
        let mut coinbase_output_max_additional_size_sent = false;
        let mut last_token = None;
        let miner_coinbase_output = self_mutex
            .safe_lock(|s| s.miner_coinbase_output.clone())
            .unwrap();
        let main_task = {
            let self_mutex = self_mutex.clone();
            tokio::task::spawn(async move {
                // Send CoinbaseOutputDataSize size to TP
                loop {
                    if last_token.is_none() {
                        let jd = self_mutex.safe_lock(|s| s.jd.clone()).unwrap();
                        last_token =
                            Some(Self::get_last_token(jd, &miner_coinbase_output[..]).await);
                    }
                    if !coinbase_output_max_additional_size_sent {
                        coinbase_output_max_additional_size_sent = true;
                        Self::send_max_coinbase_size(
                            &self_mutex,
                            last_token
                                .clone()
                                .unwrap()
                                .coinbase_output_max_additional_size,
                        )
                        .await;
                    }

                    let received = receiver.recv().await.expect("TP down");
                    //let mut frame: StdFrame =
                    //    handle_result!(tx_status.clone(), received.try_into());
                    let frame: Result<StdFrame, _> = received.try_into();
                    if let Ok(mut frame) = frame {
                        let message_type = frame.get_header().unwrap().msg_type();
                        let payload = frame.payload();

                        let next_message_to_send =
                            ParseServerTemplateDistributionMessages::handle_message_template_distribution(
                                self_mutex.clone(),
                                message_type,
                                payload,
                            );
                        match next_message_to_send {
                            Ok(SendTo::None(m)) => {
                                match m {
                                    // Send the new template along with the token to the JD so that JD can
                                    // declare the mining job
                                    Some(TemplateDistribution::NewTemplate(m)) => {
                                        // See coment on the definition of the global for memory
                                        // ordering
                                        super::IS_NEW_TEMPLATE_HANDLED
                                            .store(false, std::sync::atomic::Ordering::Release);
                                        Self::send_tx_data_request(&self_mutex, m.clone()).await;
                                        self_mutex
                                            .safe_lock(|t| t.new_template_message = Some(m.clone()))
                                            .unwrap();
                                        let token = last_token.clone().unwrap();
                                        let pool_output = token.coinbase_output.to_vec();
                                        Downstream::on_new_template(
                                            &down,
                                            m.clone(),
                                            &pool_output[..],
                                        )
                                        .await
                                        .unwrap();
                                    }
                                    Some(TemplateDistribution::SetNewPrevHash(m)) => {
                                        info!("Received SetNewPrevHash, waiting for IS_NEW_TEMPLATE_HANDLED");
                                        // See coment on the definition of the global for memory
                                        // ordering
                                        while !super::IS_NEW_TEMPLATE_HANDLED
                                            .load(std::sync::atomic::Ordering::Acquire)
                                        {
                                            tokio::task::yield_now().await;
                                        }
                                        info!("IS_NEW_TEMPLATE_HANDLED ok");
                                        if let Some(jd) = jd.as_ref() {
                                            super::job_declarator::JobDeclarator::on_set_new_prev_hash(
                                                jd.clone(),
                                                m.clone(),
                                            ).await;
                                        }
                                        Downstream::on_set_new_prev_hash(&down, m).await.unwrap();
                                    }

                                    Some(TemplateDistribution::RequestTransactionDataSuccess(
                                        m,
                                    )) => {
                                        // safe to unwrap because this message is received after the new
                                        // template message
                                        let transactions_data = m.transaction_list;
                                        let excess_data = m.excess_data;
                                        let m = self_mutex
                                            .safe_lock(|t| t.new_template_message.clone())
                                            .unwrap()
                                            .unwrap();
                                        let token = last_token.unwrap();
                                        last_token = None;
                                        let mining_token = token.mining_job_token.to_vec();
                                        let pool_coinbase_out = token.coinbase_output.to_vec();
                                        if let Some(jd) = jd.as_ref() {
                                            super::job_declarator::JobDeclarator::on_new_template(
                                                jd,
                                                m.clone(),
                                                mining_token,
                                                transactions_data,
                                                excess_data,
                                                pool_coinbase_out,
                                            )
                                            .await;
                                        }
                                    }
                                    Some(TemplateDistribution::RequestTransactionDataError(_)) => {
                                        warn!("The prev_hash of the template requested to Template Provider no longer points to the latest tip. Continuing work on the updated template.")
                                    }
                                    _ => {
                                        error!("{:?}", frame);
                                        error!("{:?}", frame.payload());
                                        error!("{:?}", frame.get_header());
                                        std::process::exit(1);
                                    }
                                }
                            }
                            Ok(m) => {
                                error!("{:?}", m);
                                error!("{:?}", frame);
                                error!("{:?}", frame.payload());
                                error!("{:?}", frame.get_header());
                                std::process::exit(1);
                            }
                            Err(e) => {
                                error!("{:?}", e);
                                error!("{:?}", frame);
                                error!("{:?}", frame.payload());
                                error!("{:?}", frame.get_header());
                                std::process::exit(1);
                            }
                        }
                    } else {
                        // TODO TODO TODO
                    }
                }
            })
        };
        main_task.into()
    }

    async fn on_new_solution(self_: Arc<Mutex<Self>>, mut rx: TReceiver<SubmitSolution<'static>>) {
        while let Some(solution) = rx.recv().await {
            if !self_
                .safe_lock(|s| s.test_only_do_not_send_solution_to_tp)
                .unwrap()
            {
                let sv2_frame: StdFrame = PoolMessages::TemplateDistribution(
                    TemplateDistribution::SubmitSolution(solution),
                )
                .try_into()
                .expect("Failed to convert solution to sv2 frame!");
                Self::send(&self_, sv2_frame).await
            }
        }
    }
}

#[cfg(test)]
// Faile
// Reason: Sv2Frame is not yet serialized.
mod tests {
    use crate::jd_client::job_declarator::test::{create_template, get_new_prev_hash, setup_jd};

    use super::*;
    use crate::jd_client::DownstreamMiningNode;
    use bitcoin::TxOut;
    use codec_sv2::Sv2Frame;
    use roles_logic_sv2::parsers::{Mining, TemplateDistribution};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_start_templates_new_template_message() {
        tracing_subscriber::fmt::init();
        let (tx, rx) = mpsc::channel(10);
        let (downstream_sender, mut downstream_receiver) = mpsc::channel(10);

        let (jd_sender, _jd_receiver) = mpsc::channel(10);
        let (solution_sender, _solution_receiver) = mpsc::channel(10);
        let miner_coinbase_output = vec![TxOut::default()];
        let (jd, _jd_abortable, upstream) = setup_jd(jd_sender).await;

        // Initialize downstream
        let downstream = Arc::new(Mutex::new(DownstreamMiningNode::new(
            downstream_sender,
            Some(upstream),
            solution_sender,
            false,
            miner_coinbase_output,
            Some(jd.clone()),
        )));

        // Initialize TemplateRx
        let template_rx = TemplateRx {
            sender: tx.clone(),
            jd: Some(jd.clone()),
            down: downstream,
            new_template_message: None,
            miner_coinbase_output: vec![],
            test_only_do_not_send_solution_to_tp: true,
        };
        let template_rx = Arc::new(Mutex::new(template_rx));

        // Create new_prev_hash
        let new_template = create_template();
        let new_prev_hash = get_new_prev_hash(new_template.template_id);
        let frame = Sv2Frame::try_from(PoolMessages::TemplateDistribution(
            TemplateDistribution::SetNewPrevHash(new_prev_hash.clone()),
        ))
        .unwrap();

        // Call the start_templates fn
        let start_template_abortable = TemplateRx::start_templates(template_rx, rx).await;

        // Send the new_prev_hash frame
        tx.send(frame.into()).await.unwrap();
        let received = downstream_receiver.recv().await;
        warn!("{:?}", received.as_ref().expect("No data"));
        // Check if new_prev_hash message was handled as expected
        if let Some(received_message) = received {
            if let Mining::NewExtendedMiningJob(job) = received_message.into() {
                assert_eq!(job.merkle_path, new_template.merkle_path);
                assert_eq!(job.coinbase_tx_prefix, new_template.coinbase_tx_outputs);
            }
        }

        if start_template_abortable.is_finished() {
            drop(start_template_abortable);
        }
    }
}
