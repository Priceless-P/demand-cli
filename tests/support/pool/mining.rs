use super::control::ChannelKey;
use roles_logic_sv2::mining_sv2::OpenExtendedMiningChannelSuccess;

#[derive(Clone, Debug)]
pub struct ChannelState {
    pub key: ChannelKey,
    pub target: [u8; 32],
    pub extranonce_prefix: Vec<u8>,
    pub extranonce_size: u16,
}

impl ChannelState {
    pub fn opened(key: ChannelKey, message: &OpenExtendedMiningChannelSuccess<'_>) -> Self {
        Self {
            key,
            target: message.target.to_vec().try_into().expect("U256"),
            extranonce_prefix: message.extranonce_prefix.to_vec(),
            extranonce_size: message.extranonce_size,
        }
    }
}

#[derive(Default)]
pub struct MiningSession {
    pub channels: std::collections::HashMap<ChannelKey, ChannelState>,
}
