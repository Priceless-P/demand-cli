use reqwest::Url;
use roles_logic_sv2::utils::Mutex;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, error};

use crate::{
    proxy_state::{DownstreamType, ProxyState},
    shared::error::Error,
};

const MONITORING_SERVER_URL: &str = "http://localhost:8787/api/share/save";
const BATCH_SIZE: u32 = 20; // Default batch size for sending shares

#[derive(serde::Serialize, Clone, Debug)]
pub struct SharesInfo {
    worker_name: String,
    difficulty: f32,
    job_id: i64,
    nonce: i64,
    ntime: i64,
}

impl SharesInfo {
    pub fn new(worker_name: String, difficulty: f32, job_id: i64, nonce: i64, ntime: i64) -> Self {
        SharesInfo {
            worker_name,
            difficulty,
            job_id,
            nonce,
            ntime,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SharesMonitor {
    pending_shares: Arc<Mutex<Vec<SharesInfo>>>,
    batch_size: u32,
}

impl SharesMonitor {
    pub fn new() -> Self {
        SharesMonitor {
            pending_shares: Arc::new(Mutex::new(Vec::new())),
            batch_size: BATCH_SIZE, // Default batch size
        }
    }

    /// Inserts a new share into the pending shares list.
    pub fn insert_share(&self, share: SharesInfo) {
        self.pending_shares
            .safe_lock(|shares| {
                shares.push(share);
            })
            .unwrap_or_else(|e| {
                error!("Failed to lock pending shares: {:?}", e);
                ProxyState::update_downstream_state(DownstreamType::TranslatorDownstream);
            });
    }

    /// Retrieves the list of pending shares.
    fn get_pending_shares(&self) -> Vec<SharesInfo> {
        self.pending_shares
            .safe_lock(|shares| shares.clone())
            .unwrap_or_else(|e| {
                error!("Failed to lock pending shares: {:?}", e);
                ProxyState::update_downstream_state(DownstreamType::TranslatorDownstream);
                Vec::new()
            })
    }

    /// Clears the list of pending shares.
    fn clear_pending_shares(&self) {
        self.pending_shares
            .safe_lock(|shares| {
                shares.clear();
            })
            .unwrap_or_else(|e| {
                error!("Failed to lock pending shares: {:?}", e);
                ProxyState::update_downstream_state(DownstreamType::TranslatorDownstream);
            });
    }

    /// Monitors the pending shares and sends them to the monitoring server in batches.
    pub async fn monitor(&self) -> Result<(), Error> {
        let api = MonitorAPI::new(MONITORING_SERVER_URL);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60)); // Check every 60 seconds
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
    fn new(url: &str) -> Self {
        let client = reqwest::Client::new();
        MonitorAPI {
            url: url.parse().expect("Invalid URL"),
            client,
        }
    }

    /// Sends a batch of shares to the monitoring server.
    async fn send_shares(&self, shares: Vec<SharesInfo>) -> Result<(), Error> {
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
