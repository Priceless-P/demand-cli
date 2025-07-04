use reqwest::Url;
use roles_logic_sv2::utils::Mutex;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, error};
const BATCH_SIZE: u32 = 20; // Default batch size for sending shares

use crate::{
    proxy_state::{DownstreamType, ProxyState},
    shared::error::Error,
};

fn monitoring_server_url() -> String {
    std::env::var("MONITORING_SERVER_URL")
        .ok()
        .unwrap_or("http://localhost:8787/api/share/save".to_string())
}

// #[derive(serde::Serialize, Debug)]
// pub struct AggregatedShares {
//     worker_name: String,
//     timeframe_duration: i64, // time difference between the first and last share in seconds
//     difficulties: f32,       // aggregated difficulty of all shares
//     accepted_count: i64,     // total number of accepted shares
//     rejected_count: Value,   // Json serailized map of rejection reasons and their counts
// }

#[derive(serde::Serialize, Clone, Debug)]
pub struct ShareInfo {
    worker_name: String,
    difficulty: Option<f32>,
    job_id: i64,
    rejection_reason: Option<RejectionReason>, // if None, the share was accepted
    timestamp: u64,
}

impl ShareInfo {
    pub fn new(
        worker_name: String,
        difficulty: Option<f32>,
        job_id: i64,
        rejection_reason: Option<RejectionReason>,
    ) -> Self {
        ShareInfo {
            worker_name,
            difficulty,
            job_id,
            rejection_reason,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SharesMonitor {
    pending_shares: Arc<Mutex<Vec<ShareInfo>>>,
    batch_size: u32,
}

impl SharesMonitor {
    pub fn new() -> Self {
        SharesMonitor {
            pending_shares: Arc::new(Mutex::new(Vec::new())),
            batch_size: BATCH_SIZE,
        }
    }

    /// Inserts a new share into the pending shares list.
    pub fn insert_share(&self, share: ShareInfo) {
        self.pending_shares
            .safe_lock(|event| {
                event.push(share);
            })
            .unwrap_or_else(|e| {
                error!("Failed to lock pending shares: {:?}", e);
                ProxyState::update_downstream_state(DownstreamType::TranslatorDownstream);
            });
    }

    /// Retrieves the list of pending shares.
    fn get_pending_shares(&self) -> Vec<ShareInfo> {
        self.pending_shares
            .safe_lock(|event| event.clone())
            .unwrap_or_else(|e| {
                error!("Failed to lock pending shares: {:?}", e);
                ProxyState::update_downstream_state(DownstreamType::TranslatorDownstream);
                Vec::new()
            })
    }

    /// Clears the list of pending shares.
    fn clear_pending_shares(&self) {
        self.pending_shares
            .safe_lock(|event| {
                event.clear();
            })
            .unwrap_or_else(|e| {
                error!("Failed to lock pending shares: {:?}", e);
                ProxyState::update_downstream_state(DownstreamType::TranslatorDownstream);
            });
    }

    /// Monitors the pending shares and sends them to the monitoring server in batches.
    pub async fn monitor(&self) -> Result<(), Error> {
        let api = MonitorAPI::new(monitoring_server_url());
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60)); // Check every 60 seconds
        interval.tick().await; // Skip the first tick to avoid unnecessary error log
        loop {
            interval.tick().await;
            let shares_to_send = self.get_pending_shares();
            if !shares_to_send.is_empty() {
                if shares_to_send.len() >= self.batch_size as usize {
                    api.send_shares(shares_to_send.clone()).await?;
                    debug!("Successfully sent Shares: {:?} to API", &shares_to_send);
                    self.clear_pending_shares(); // Clear after sending
                } else {
                    debug!(
                        "Current shares count ({}) is less than batch size ({}), waiting for more",
                        shares_to_send.len(),
                        self.batch_size
                    );
                }
            } else {
                error!("No pending shares to send");
            }
        }
    }
}

struct MonitorAPI {
    url: Url,
    client: reqwest::Client,
}

impl MonitorAPI {
    fn new(url: String) -> Self {
        let client = reqwest::Client::new();
        MonitorAPI {
            url: url.parse().expect("Invalid URL"),
            client,
        }
    }

    /// Sends a batch of shares to the monitoring server.
    async fn send_shares(&self, shares: Vec<ShareInfo>) -> Result<(), Error> {
        let token = crate::config::Configuration::token().expect("Token is not set");

        debug!("Sending batch of {} shares to API", shares.len());
        let response = self
            .client
            .post(self.url.clone())
            .json(&json!({ "shares": shares, "token": token }))
            .send()
            .await?;

        match response.error_for_status() {
            Ok(_) => Ok(()),
            Err(err) => {
                error!("Failed to send shares: {}", err);
                Err(err.into())
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub enum RejectionReason {
    JobIdNotFound,
    InvalidShare,
    InvalidJobIdFormat,
    DifficultyMismatch,
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectionReason::JobIdNotFound => write!(f, "Job ID not found"),
            RejectionReason::InvalidShare => write!(f, "Invalid share"),
            RejectionReason::InvalidJobIdFormat => write!(f, "Invalid job ID format"),
            RejectionReason::DifficultyMismatch => write!(f, "Difficulty mismatch"),
        }
    }
}
