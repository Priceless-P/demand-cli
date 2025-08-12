use clap::Parser;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
};
use tracing::{debug, error, info, warn};

use crate::{
    shared::error::Error, HashUnit, DEFAULT_SV1_HASHPOWER, PRODUCTION_URL, STAGING_URL,
    TESTNET3_URL,
};
lazy_static! {
    pub static ref CONFIG: Configuration = Configuration::load_config();
}
#[derive(Parser)]
struct Args {
    #[clap(long)]
    staging: bool,
    #[clap(long)]
    testnet3: bool,
    #[clap(long)]
    local: bool,
    #[clap(long = "d", short = 'd', value_parser = parse_hashrate)]
    downstream_hashrate: Option<f32>,
    #[clap(long = "loglevel", short = 'l')]
    loglevel: Option<String>,
    #[clap(long = "nc", short = 'n')]
    noise_connection_log: Option<String>,
    #[clap(long = "sv1_loglevel")]
    sv1_loglevel: bool,
    #[clap(long = "delay")]
    delay: Option<u64>,
    #[clap(long = "interval", short = 'i')]
    adjustment_interval: Option<u64>,
    #[clap(long)]
    token: Option<String>,
    #[clap(long)]
    tp_address: Option<String>,
    #[clap(long)]
    listening_addr: Option<String>,
    #[clap(long = "config", short = 'c')]
    config_file: Option<PathBuf>,
    #[clap(long = "api-server-port", short = 's')]
    api_server_port: Option<String>,
    #[clap(long, short = 'm')]
    monitor: bool,
    #[clap(long, short = 'u')]
    auto_update: bool,
    #[clap(long, short = 'r', value_enum)]
    region: Option<Region>,
}

#[derive(Serialize, Deserialize)]
struct ConfigFile {
    token: Option<String>,
    tp_address: Option<String>,
    interval: Option<u64>,
    delay: Option<u64>,
    downstream_hashrate: Option<String>,
    loglevel: Option<String>,
    nc_loglevel: Option<String>,
    sv1_log: Option<bool>,
    staging: Option<bool>,
    local: Option<bool>,
    testnet3: Option<bool>,
    listening_addr: Option<String>,
    api_server_port: Option<String>,
    monitor: Option<bool>,
    auto_update: Option<bool>,
    region: Option<String>,
}

impl ConfigFile {
    pub fn default() -> Self {
        ConfigFile {
            token: None,
            tp_address: None,
            interval: None,
            delay: None,
            downstream_hashrate: None,
            loglevel: None,
            nc_loglevel: None,
            sv1_log: None,
            staging: None,
            testnet3: None,
            local: None,
            listening_addr: None,
            api_server_port: None,
            monitor: None,
            auto_update: None,
            region: None,
        }
    }
}

pub struct Configuration {
    token: Option<String>,
    tp_address: Option<String>,
    interval: u64,
    delay: u64,
    downstream_hashrate: f32,
    loglevel: String,
    nc_loglevel: String,
    sv1_log: bool,
    staging: bool,
    testnet3: bool,
    local: bool,
    listening_addr: Option<String>,
    api_server_port: String,
    monitor: bool,
    auto_update: bool,
    region: Option<Region>,
}
impl Configuration {
    pub fn token() -> Option<String> {
        CONFIG.token.clone()
    }

    pub fn tp_address() -> Option<String> {
        CONFIG.tp_address.clone()
    }

    pub async fn pool_address() -> Option<Vec<SocketAddr>> {
        match fetch_pool_urls().await {
            Ok(addresses) => Some(addresses),
            Err(e) => {
                error!("Failed to fetch pool addresses: {}", e);
                None
            }
        }
    }

    pub fn adjustment_interval() -> u64 {
        CONFIG.interval
    }

    pub fn delay() -> u64 {
        CONFIG.delay
    }

    pub fn downstream_hashrate() -> f32 {
        CONFIG.downstream_hashrate
    }

    pub fn downstream_listening_addr() -> Option<String> {
        CONFIG.listening_addr.clone()
    }

    pub fn api_server_port() -> String {
        CONFIG.api_server_port.clone()
    }

    pub fn loglevel() -> &'static str {
        match CONFIG.loglevel.to_lowercase().as_str() {
            "trace" | "debug" | "info" | "warn" | "error" | "off" => &CONFIG.loglevel,
            _ => {
                eprintln!(
                    "Invalid log level '{}'. Defaulting to 'info'.",
                    CONFIG.loglevel
                );
                "info"
            }
        }
    }

    pub fn nc_loglevel() -> &'static str {
        match CONFIG.nc_loglevel.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" | "off" => &CONFIG.nc_loglevel,
            _ => {
                eprintln!(
                    "Invalid log level for noise_connection '{}' Defaulting to 'off'.",
                    &CONFIG.nc_loglevel
                );
                "off"
            }
        }
    }
    pub fn sv1_ingress_log() -> bool {
        CONFIG.sv1_log
    }

    pub fn staging() -> bool {
        CONFIG.staging
    }

    pub fn local() -> bool {
        CONFIG.local
    }

    pub fn testnet3() -> bool {
        CONFIG.testnet3
    }

    /// Returns the environment based on the configuration.
    /// Possible values: "staging", "local", "production".
    /// If no environment is set, it defaults to "production".
    pub fn environment() -> String {
        if CONFIG.staging {
            "staging".to_string()
        } else if CONFIG.local {
            "local".to_string()
        } else if CONFIG.testnet3 {
            "testnet3".to_string()
        } else {
            "production".to_string()
        }
    }

    pub fn monitor() -> bool {
        CONFIG.monitor
    }

    pub fn auto_update() -> bool {
        CONFIG.auto_update
    }

    fn region() -> Option<Region> {
        CONFIG.region.clone()
    }

    // Loads config from CLI, file, or env vars with precedence: CLI > file > env.
    fn load_config() -> Self {
        let args = Args::parse();
        let config_path: PathBuf = args.config_file.unwrap_or("config.toml".into());
        let config: ConfigFile = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|content| toml::from_str(&content).ok())
            .unwrap_or(ConfigFile::default());

        let token = args
            .token
            .or(config.token)
            .or_else(|| std::env::var("TOKEN").ok());
        debug!("User Token: {:?}", token);

        let tp_address = args
            .tp_address
            .or(config.tp_address)
            .or_else(|| std::env::var("TP_ADDRESS").ok());

        let interval = args
            .adjustment_interval
            .or(config.interval)
            .or_else(|| std::env::var("INTERVAL").ok().and_then(|s| s.parse().ok()))
            .unwrap_or(120_000);

        let delay = args
            .delay
            .or(config.delay)
            .or_else(|| std::env::var("DELAY").ok().and_then(|s| s.parse().ok()))
            .unwrap_or(0);

        let expected_hashrate = args
            .downstream_hashrate
            .or_else(|| {
                config
                    .downstream_hashrate
                    .as_deref()
                    .and_then(|d| parse_hashrate(d).ok())
            })
            .or_else(|| {
                std::env::var("DOWNSTREAM_HASHRATE")
                    .ok()
                    .and_then(|s| s.parse().ok())
            });
        let downstream_hashrate;
        if let Some(hashpower) = expected_hashrate {
            downstream_hashrate = hashpower;
            info!(
                "Using downstream hashrate: {}h/s",
                HashUnit::format_value(hashpower)
            );
        } else {
            downstream_hashrate = DEFAULT_SV1_HASHPOWER;
            warn!(
                "No downstream hashrate provided, using default value: {}h/s",
                HashUnit::format_value(DEFAULT_SV1_HASHPOWER)
            );
        }

        let listening_addr = args.listening_addr.or(config.listening_addr).or_else(|| {
            std::env::var("DOWNSTREAM_HASHRATE")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        let api_server_port = args
            .api_server_port
            .or(config.api_server_port)
            .or_else(|| {
                std::env::var("API_SERVER_PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or("3001".to_string());

        let loglevel = args
            .loglevel
            .or(config.loglevel)
            .or_else(|| std::env::var("LOGLEVEL").ok())
            .unwrap_or("info".to_string());

        let nc_loglevel = args
            .noise_connection_log
            .or(config.nc_loglevel)
            .or_else(|| std::env::var("NC_LOGLEVEL").ok())
            .unwrap_or("off".to_string());

        let sv1_log = args.sv1_loglevel
            || config.sv1_log.unwrap_or(false)
            || std::env::var("SV1_LOGLEVEL").is_ok();

        let staging =
            args.staging || config.staging.unwrap_or(false) || std::env::var("STAGING").is_ok();
        let testnet3 =
            args.testnet3 || config.testnet3.unwrap_or(false) || std::env::var("TESTNET3").is_ok();
        let local = args.local || config.local.unwrap_or(false) || std::env::var("LOCAL").is_ok();
        let monitor =
            args.monitor || config.monitor.unwrap_or(false) || std::env::var("MONITOR").is_ok();

        let auto_update = args.auto_update
            || config.auto_update.unwrap_or(true)
            || std::env::var("AUTO_UPDATE").is_ok();

        let region = args
            .region
            .or(config.region.and_then(|s| Region::from_str(s.as_ref())))
            .or_else(|| {
                std::env::var("REGION")
                    .ok()
                    .and_then(|s| Region::from_str(&s))
            });

        Configuration {
            token,
            tp_address,
            interval,
            delay,
            downstream_hashrate,
            loglevel,
            nc_loglevel,
            sv1_log,
            staging,
            testnet3,
            local,
            listening_addr,
            api_server_port,
            monitor,
            auto_update,
            region,
        }
    }
}

/// Parses a hashrate string (e.g., "10T", "2.5P", "500E") into an f32 value in h/s.
fn parse_hashrate(hashrate_str: &str) -> Result<f32, String> {
    let hashrate_str = hashrate_str.trim();
    if hashrate_str.is_empty() {
        return Err("Hashrate cannot be empty. Expected format: '<number><unit>' (e.g., '10T', '2.5P', '5E'".to_string());
    }

    let unit = hashrate_str.chars().last().unwrap_or(' ').to_string();
    let num = &hashrate_str[..hashrate_str.len().saturating_sub(1)];

    let num: f32 = num.parse().map_err(|_| {
        format!(
            "Invalid number '{}'. Expected format: '<number><unit>' (e.g., '10T', '2.5P', '5E')",
            num
        )
    })?;

    let multiplier = HashUnit::from_str(&unit)
        .map(|unit| unit.multiplier())
        .ok_or_else(|| format!(
            "Invalid unit '{}'. Expected 'T' (Terahash), 'P' (Petahash), or 'E' (Exahash). Example: '10T', '2.5P', '5E'",
            unit
        ))?;

    let hashrate = num * multiplier;

    if hashrate.is_infinite() || hashrate.is_nan() {
        return Err("Hashrate too large or invalid".to_string());
    }

    Ok(hashrate)
}

fn parse_address(addr: String) -> Option<SocketAddr> {
    match addr.to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(socket_addr) => Some(socket_addr),
            None => {
                error!("Failed to parse address: {}", addr);
                None
            }
        },
        Err(e) => {
            error!("Failed to parse address '{}': {}", addr, e);
            None
        }
    }
}

/// Fetches pool URLs from the server based on the environment.
async fn fetch_pool_urls() -> Result<Vec<SocketAddr>, Error> {
    if CONFIG.local {
        return Ok(vec![
            parse_address("127.0.0.1:20000".to_string()).expect("Invalid local address")
        ]);
    };

    let url = if CONFIG.staging {
        info!("Fetching pool URLs from staging server: {}", STAGING_URL);
        STAGING_URL
    } else if CONFIG.testnet3 {
        info!("Fetching pool URLs from testnet server: {}", TESTNET3_URL);
        TESTNET3_URL
    } else {
        info!(
            "Fetching pool URLs from production server: {}",
            PRODUCTION_URL
        );
        PRODUCTION_URL
    };
    let endpoint = format!("{}/api/pool/urls", url);
    let token = Configuration::token().expect("TOKEN is not set");

    let response = match reqwest::Client::new()
        .post(endpoint)
        .json(&json!({"token": token}))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to fetch pool urls: {}", e);
            return Err(Error::from(e));
        }
    };

    debug!("Response status: {}", response.status());
    let addresses = match response.json::<Vec<PoolAddress>>().await {
        Ok(mut addrs) => {
            // if region is set, put the pools with that region first
            if let Some(region) = Configuration::region() {
                let region = region.as_str();
                addrs.sort_by(|a, b| b.host.contains(region).cmp(&a.host.contains(region)));
                debug!("Sorted addresses by region '{}': {:?}", region, addrs);
                addrs
            } else {
                debug!("No region set, returning addresses as is: {:?}", addrs);
                addrs
            }
        }
        Err(e) => {
            error!("Failed to parse pool urls: {}", e);
            return Err(Error::from(e));
        }
    };

    // Parse the addresses into SocketAddr
    let socket_addrs: Vec<SocketAddr> = addresses
        .into_iter()
        .filter_map(|addr| {
            let address = format!("{}:{}", addr.host, addr.port);
            parse_address(address)
        }) // Filter out any None values
        .collect();
    debug!("Pool addresses: {:?}", socket_addrs);
    Ok(socket_addrs)
}

#[derive(Debug, Deserialize)]
struct PoolAddress {
    host: String,
    port: u16,
}

#[derive(Debug, Clone, clap::ValueEnum, Serialize, Deserialize)]
enum Region {
    UsEast,
    EuNorth,
}

impl Region {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "us-east" => Some(Region::UsEast),
            "eu-north" => Some(Region::EuNorth),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &str {
        match self {
            Region::UsEast => "us-east",
            Region::EuNorth => "eu-north",
        }
    }
}
