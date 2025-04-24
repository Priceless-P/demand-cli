use bitcoin::{Amount, TxOut};
use roles_logic_sv2::{utils::CoinbaseOutput, Error};
use serde::Deserialize;
use tracing::{error, info};

use crate::proxy_state::{ProxyState, TpState};

// Used when tp is down or connection was unsuccessful to retry connection.
pub async fn retry_connection(address: String) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
    loop {
        info!("TP Retrying connection....");
        interval.tick().await;
        if tokio::net::TcpStream::connect(address.clone())
            .await
            .is_ok()
        {
            info!("Successfully reconnected to TP: Restarting Proxy...");
            if crate::TP_ADDRESS
                .safe_lock(|tp| *tp = Some(address))
                .is_err()
            {
                error!("TP_ADDRESS Mutex failed");
                std::process::exit(1);
            };
            // This force the proxy to restart. If we use Up the proxy just ignore it.
            // So updating it to Down and setting the TP_ADDRESS to Some(address) will make the
            // proxy restart with TP, the the TpState will be set to Up.
            ProxyState::update_tp_state(TpState::Down);
            break;
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Output {
    output_script_type: String,
    output_script_value: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    coinbase_outputs: Vec<Output>,
    withhold: Option<bool>,
}
impl Config {
    pub fn withhold(self) -> Option<bool> {
        self.withhold
    }
}

impl TryFrom<&Output> for CoinbaseOutput {
    type Error = roles_logic_sv2::errors::Error;

    fn try_from(pool_output: &Output) -> Result<Self, Self::Error> {
        match pool_output.output_script_type.as_str() {
            "TEST" | "P2PK" | "P2PKH" | "P2WPKH" | "P2SH" | "P2WSH" | "P2TR" => {
                Ok(CoinbaseOutput {
                    output_script_type: pool_output.clone().output_script_type,
                    output_script_value: pool_output.clone().output_script_value,
                })
            }
            _ => Err(roles_logic_sv2::Error::UnknownOutputScriptType),
        }
    }
}

pub fn get_coinbase_output(config: &Config) -> Result<Vec<TxOut>, Error> {
    let mut result = Vec::new();
    for coinbase_output_pool in &config.coinbase_outputs {
        let coinbase_output: CoinbaseOutput = coinbase_output_pool.try_into()?;
        let output_script = coinbase_output.try_into()?;
        result.push(TxOut {
            value: Amount::from_sat(0),
            script_pubkey: output_script,
        });
    }
    match result.is_empty() {
        true => Err(Error::EmptyCoinbaseOutputs),
        _ => Ok(result),
    }
}

pub fn parse_tp_address() -> Option<(String, u16, String)> {
    let tp_address = match crate::TP_ADDRESS.safe_lock(|tp| tp.clone()) {
        Ok(tp_address) => tp_address
            .expect("Unreachable code, jdc is not instantiated when TP_ADDRESS not present"),
        Err(e) => {
            error!("TP_ADDRESS mutex corrupted: {e}");
            return None;
        }
    };

    let mut parts = tp_address.split(':');
    let ip_tp = parts.next().expect("The passed value for TP address is not valid. Terminating.... TP_ADDRESS should be in this format `127.0.0.1:8442`").to_string();
    let port_tp = parts.next().expect("The passed value for TP address is not valid. Terminating.... TP_ADDRESS should be in this format `127.0.0.1:8442`").parse::<u16>().expect("This operation should not fail because a valid port_tp should always be converted to U16");

    Some((ip_tp, port_tp, tp_address))
}
