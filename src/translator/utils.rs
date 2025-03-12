use std::{
    collections::VecDeque,
    sync::{atomic::AtomicBool, Arc},
};

use crate::translator::error::Error;
use bitcoin::hashes::{sha256d, Hash};
use lazy_static::lazy_static;
use roles_logic_sv2::{mining_sv2::Target, utils::Mutex};
use sv1_api::{client_to_server, server_to_client::Notify};
use tracing::error;

use super::downstream::Downstream;
lazy_static! {
    pub static ref SHARE_TIMESTAMPS: Arc<Mutex<VecDeque<tokio::time::Instant>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(70)));
    pub static ref IS_RATE_LIMITED: AtomicBool = AtomicBool::new(false);
}

/// Checks if a share can be sent upstream based on a rate limit of 70 shares per minute.
/// Returns `true` if the share can be sent, `false` if the limit is exceeded.
pub async fn check_share_rate_limit() {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        let now = tokio::time::Instant::now();
        let count = SHARE_TIMESTAMPS
            .safe_lock(|timestamps| {
                while let Some(&front) = timestamps.front() {
                    if now.duration_since(front).as_secs() >= 60 {
                        timestamps.pop_front();
                    } else {
                        break;
                    }
                }
                timestamps.len()
            })
            .unwrap_or_else(|e| {
                error!("Failed to lock SHARE_TIMESTAMPS: {:?}", e);
                0
            });

        IS_RATE_LIMITED.store(count >= 70, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Checks if a share can be sent by checking if rate is limited
pub fn allow_submit_share() -> crate::translator::error::ProxyResult<'static, bool> {
    // Check if rate-limited
    let is_rate_limited = IS_RATE_LIMITED.load(std::sync::atomic::Ordering::SeqCst);

    if is_rate_limited {
        return Ok(false); // Rate limit exceeded, don’t send
    }

    SHARE_TIMESTAMPS
        .safe_lock(|timestamps| {
            timestamps.push_back(tokio::time::Instant::now());
        })
        .map_err(|e| {
            error!("Failed to lock SHARE_TIMESTAMPS: {:?}", e);
            Error::TranslatorDiffConfigMutexPoisoned
        })?;

    Ok(true) // Share can be sent
}

pub fn validate_share(
    request: &client_to_server::Submit<'static>,
    job: &Notify,
    difficulty: f32,
    extranonce1: Vec<u8>,
    version_rolling_mask: Option<sv1_api::utils::HexU32Be>,
) -> bool {
    // Check job ID match
    if request.job_id != job.job_id {
        error!("Share rejected: Job ID mismatch");
        return false;
    }

    let prev_hash_vec: Vec<u8> = job.prev_hash.clone().into();
    let prev_hash: [u8; 32] = prev_hash_vec.try_into().expect("PrevHash must be 32 bytes");

    let mut merkle_branch = Vec::new();
    for branch in &job.merkle_branch {
        merkle_branch.push(branch.0.to_vec());
    }

    let mut extranonce = Vec::new();
    extranonce.extend_from_slice(extranonce1.as_ref());
    extranonce.extend_from_slice(request.extra_nonce2.0.as_ref());
    let extranonce: &[u8] = extranonce.as_ref();

    let job_version = job.version.0;
    let request_version = request
        .version_bits
        .clone()
        .map(|vb| vb.0)
        .unwrap_or(job_version);
    let mask = version_rolling_mask
        .unwrap_or(sv1_api::utils::HexU32Be(0x1FFFE000_u32))
        .0;
    let version = (job_version & !mask) | (request_version & mask);

    let mut hash = roles_logic_sv2::utils::get_target(
        request.nonce.0,
        version,
        request.time.0,
        extranonce,
        job.coin_base1.as_ref(),
        job.coin_base2.as_ref(),
        bitcoin::BlockHash::from(sha256d::Hash::from_inner(prev_hash)),
        merkle_branch,
        job.bits.0,
    );

    hash.reverse(); //conver to little-endian

    let hash: Target = hash.into();
    let target = Downstream::difficulty_to_target(difficulty);
    let target: Target = target.into();
    hash <= target
}

// /// currently the pool only supports 16 bytes exactly for its channels
// /// to use but that may change
// pub fn proxy_extranonce1_len(
//     channel_extranonce2_size: usize,
//     downstream_extranonce2_len: usize,
// ) -> usize {
//     // full_extranonce_len - pool_extranonce1_len - miner_extranonce2 = tproxy_extranonce1_len
//     channel_extranonce2_size - downstream_extranonce2_len
// }
