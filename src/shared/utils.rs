use std::fmt::Display;

use sv1_api::utils::HexU32Be;
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;
use tracing::info;

#[derive(Debug)]
pub struct AbortOnDrop {
    abort_handle: AbortHandle,
}

impl AbortOnDrop {
    pub fn new<T: Send + 'static>(handle: JoinHandle<T>) -> Self {
        let abort_handle = handle.abort_handle();
        Self { abort_handle }
    }

    pub fn is_finished(&self) -> bool {
        self.abort_handle.is_finished()
    }
}

impl core::ops::Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.abort_handle.abort()
    }
}

impl<T: Send + 'static> From<JoinHandle<T>> for AbortOnDrop {
    fn from(value: JoinHandle<T>) -> Self {
        Self::new(value)
    }
}

/// Select a version rolling mask and min bit count based on the request from the miner.
/// It copy the behavior from SRI translator
pub fn sv1_rolling(configure: &sv1_api::client_to_server::Configure) -> (HexU32Be, HexU32Be) {
    // TODO 0x1FFFE000 should be configured
    // = 11111111111111110000000000000
    // this is a reasonable default as it allows all 16 version bits to be used
    // If the tproxy/pool needs to use some version bits this needs to be configurable
    // so upstreams can negotiate with downstreams. When that happens this should consider
    // the min_bit_count in the mining.configure message
    let version_rollin_mask = configure
        .version_rolling_mask()
        .map(|mask| HexU32Be(mask & 0x1FFFE000))
        .unwrap_or(HexU32Be(0));
    let version_rolling_min_bit = configure
        .version_rolling_min_bit_count()
        .unwrap_or(HexU32Be(0));
    (version_rollin_mask, version_rolling_min_bit)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct UserId(pub i64);
impl Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Attempts to get expected sv1 hashpower value (in TH/s) from env
/// if set, it is converted to TH and returned
/// if not, a default value is returned
pub fn get_expected_hashpower() -> f32 {
    const EXPECTED_SV1_HASHPOWER: f32 = 100_000_000_000_000.0;
    std::env::var("HASHPOWER_TH")
        .ok()
        .and_then(|expected_sv1_hashpower| expected_sv1_hashpower.parse::<f32>().ok()) // Ccnvert to f32
        .map(|th| {
            let value_in_hashes = th * 1_000_000_000_000.0;
            info!("Expected HASHPOWER_TH: {} TH", th);
            value_in_hashes
        }) // convert TH/s value to H/s
        .unwrap_or_else(|| {
            info!("Invalid or missing HASHPOWER_TH. Using default value of 100 TH.");
            EXPECTED_SV1_HASHPOWER
        })
}

/// Attempts to get min sv1 downstream hashrate value (in TH/s) from env
/// if set, it is converted to TH and returned
/// if not, a default value is returned
pub fn get_min_downstream_hashrate() -> f32 {
    const MIN_SV1_DOWSNTREAM_HASHRATE: f32 = 10_000_000_000_000.0;
    std::env::var("MIN_HASHRATE")
        .ok()
        .and_then(|expected_sv1_hashpower| expected_sv1_hashpower.parse::<f32>().ok()) // convert to f32
        .map(|th| th * 1_000_000_000_000.0) // convert TH/s to H/s
        .unwrap_or_else(|| {
            info!("Invalid or missing MIN_HASHRATE. Using default value of 10 TH.");
            MIN_SV1_DOWSNTREAM_HASHRATE
        })
}
