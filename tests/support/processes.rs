use super::{
    artifacts::{binary_identity, Artifacts},
    error,
    pool::{
        control::{EventMatcher, PoolCommand},
        PoolConfig, RunningPool,
    },
    TestResult,
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    time::Instant,
};

pub fn free_address() -> TestResult<SocketAddr> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

pub fn external_binary(variable: &str) -> TestResult<PathBuf> {
    let path = std::env::var_os(variable).ok_or_else(|| {
        error(format!(
            "mining-e2e requires {variable}=/absolute/path/to/binary; see tests/README.md"
        ))
    })?;
    let path = fs::canonicalize(&path).map_err(|e| error(format!("{variable}: {e}")))?;
    if !path.is_file() {
        return Err(error(format!(
            "{variable} is not a binary file: {}",
            path.display()
        )));
    }
    Ok(path)
}

pub fn controlled_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        ("AUTO_UPDATE".into(), "false".into()),
        ("HEADFUL".into(), "false".into()),
        ("MONITOR".into(), "false".into()),
        ("RUST_BACKTRACE".into(), "1".into()),
    ]);
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".into(), path);
    }
    env
}

pub struct ManagedProcess {
    child: Option<Child>,
    pub log: PathBuf,
}

impl ManagedProcess {
    pub fn spawn(
        binary: &Path,
        args: &[String],
        env: &BTreeMap<String, String>,
        artifacts: &Artifacts,
        name: &str,
    ) -> TestResult<Self> {
        let working = artifacts.path.join(name);
        fs::create_dir_all(&working)?;
        let log = artifacts.path.join(format!("{name}.log"));
        let stdout = File::create(&log)?;
        let stderr = stdout.try_clone()?;
        artifacts.json(
            &format!("{name}-process.json"),
            &serde_json::json!({
                "binary":binary_identity(binary)?,"args":args,"env":env,"cwd":working,
            }),
        )?;
        let child = Command::new(binary)
            .args(args)
            .env_clear()
            .envs(env)
            .current_dir(working)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| error(format!("start {name} ({}): {e}", binary.display())))?;
        Ok(Self {
            child: Some(child),
            log,
        })
    }

    pub fn ensure_running(&mut self) -> TestResult {
        if let Some(status) = self
            .child
            .as_mut()
            .ok_or_else(|| error("process already stopped"))?
            .try_wait()?
        {
            return Err(error(format!(
                "process exited with {status}; log {}:\n{}",
                self.log.display(),
                fs::read_to_string(&self.log).unwrap_or_default()
            )));
        }
        Ok(())
    }

    pub async fn wait_for_log(&mut self, needle: &str, deadline: Instant) -> TestResult<String> {
        self.wait_for_log_after(needle, 0, deadline).await
    }

    pub async fn wait_for_log_after(
        &mut self,
        needle: &str,
        offset: usize,
        deadline: Instant,
    ) -> TestResult<String> {
        loop {
            let log = fs::read_to_string(&self.log)?;
            if log.get(offset..).is_some_and(|new| new.contains(needle)) {
                return Ok(log);
            }
            self.ensure_running()?;
            if Instant::now() >= deadline {
                return Err(error(format!(
                    "timed out waiting for {needle:?} in {}",
                    self.log.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn wait(&mut self, deadline: Instant) -> TestResult<std::process::ExitStatus> {
        Ok(tokio::time::timeout_at(
            deadline,
            self.child
                .as_mut()
                .ok_or_else(|| error("process stopped"))?
                .wait(),
        )
        .await??)
    }

    pub async fn shutdown(&mut self) -> TestResult {
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                child.start_kill()?;
            }
            tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
        }
        Ok(())
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

pub struct TpProcess {
    pub process: ManagedProcess,
    pub sv2: SocketAddr,
    pub rpc: SocketAddr,
}

impl TpProcess {
    pub async fn start(
        binary: &Path,
        artifacts: &Artifacts,
        seed: u64,
        time_multiplier: f64,
        network_hashpower: f64,
    ) -> TestResult<Self> {
        let rpc = free_address()?;
        let mut sv2 = free_address()?;
        while sv2 == rpc {
            sv2 = free_address()?;
        }
        let args = vec![
            "--rpc-listen-addr".into(),
            rpc.to_string(),
            "--sv2-listen-addr".into(),
            sv2.to_string(),
            "--seed".into(),
            seed.to_string(),
            "--time-multiplier".into(),
            time_multiplier.to_string(),
            "--network-hashpower".into(),
            network_hashpower.to_string(),
            "--max-stored-templates".into(),
            "64".into(),
        ];
        let process = ManagedProcess::spawn(binary, &args, &controlled_env(), artifacts, "tp")?;
        let mut tp = Self { process, sv2, rpc };
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            tp.process.ensure_running()?;
            let ready = client.post(format!("http://{rpc}")).basic_auth("username",Some("password"))
                .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]})).send().await;
            if let Ok(response) = ready {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    let response = body
                        .as_array()
                        .and_then(|batch| batch.first())
                        .unwrap_or(&body);
                    if response["result"].is_u64() && TcpListener::bind(sv2).is_err() {
                        return Ok(tp);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(error(format!(
                    "TP RPC/SV2 readiness timed out; {}",
                    tp.process.log.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn chain_height(&self) -> TestResult<u64> {
        self.rpc("getblockcount", serde_json::json!([]))
            .await?
            .as_u64()
            .ok_or_else(|| error("invalid chain height response"))
    }

    async fn rpc(&self, method: &str, params: serde_json::Value) -> TestResult<serde_json::Value> {
        let body: serde_json::Value = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()?
            .post(format!("http://{}", self.rpc))
            .basic_auth("username", Some("password"))
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?
            .json()
            .await?;
        let response = body
            .as_array()
            .and_then(|batch| batch.first())
            .unwrap_or(&body);
        if !response["error"].is_null() || response.get("result").is_none() {
            return Err(error(format!("TP {method} failed: {body}")));
        }
        Ok(response["result"].clone())
    }

    /// Mine a consensus-valid empty block using the network bits announced in real work.
    /// Only the tip scenario uses an easy network difficulty; pool share difficulty is separate.
    pub async fn mine_tip(
        &self,
        work_header: &[u8],
        deadline: Instant,
    ) -> TestResult<bitcoin::Block> {
        use bitcoin::{
            absolute,
            consensus::{deserialize, serialize},
            hashes::Hash,
            hex::DisplayHex,
            script::Builder,
            transaction::Version,
            Amount, Block, OutPoint, Sequence, Target, Transaction, TxIn, TxMerkleNode, TxOut,
            Witness,
        };
        let height = self.chain_height().await? + 1;
        let coinbase = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: Builder::new()
                    .push_int(height.try_into()?)
                    .push_slice(b"mining-e2e")
                    .into_script(),
                sequence: Sequence::MAX,
                witness: Witness::default(),
            }],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: Builder::new().push_int(1).into_script(),
            }],
        };
        let mut header: bitcoin::block::Header = deserialize(work_header)?;
        header.merkle_root = TxMerkleNode::from_byte_array(coinbase.compute_txid().to_byte_array());
        header.time = header.time.saturating_add(1);
        let target = Target::from_compact(header.bits);
        let block = tokio::task::spawn_blocking(move || {
            for nonce in 0..5_000_000 {
                if nonce % 1024 == 0 && Instant::now() >= deadline {
                    return Err(error("tip block solver timed out"));
                }
                header.nonce = nonce;
                if target.is_met_by(header.block_hash()) {
                    return Ok(Block {
                        header,
                        txdata: vec![coinbase],
                    });
                }
            }
            Err(error("tip block solver exhausted nonce window"))
        })
        .await??;
        let response = self
            .rpc(
                "submitblock",
                serde_json::json!([serialize(&block).to_lower_hex_string()]),
            )
            .await?;
        if !response.is_null() {
            return Err(error(format!("TP rejected mined tip block: {response}")));
        }
        if self.chain_height().await? != height {
            return Err(error("TP did not advance after accepting mined tip block"));
        }
        Ok(block)
    }
}

pub struct ProxyProcess {
    pub process: ManagedProcess,
    pub sv1: SocketAddr,
    pub api: SocketAddr,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum ConfigurationSource {
    #[default]
    Cli,
    Environment,
    Toml,
}

impl ProxyProcess {
    pub fn start(
        artifacts: &Artifacts,
        pools: &[SocketAddr],
        tp: Option<SocketAddr>,
        token: &str,
        extra: &[String],
        source: ConfigurationSource,
    ) -> TestResult<Self> {
        let sv1 = free_address()?;
        let mut api = free_address()?;
        while api == sv1 {
            api = free_address()?;
        }
        let mut args = vec![
            "--local".into(),
            "--token".into(),
            token.into(),
            "--listening-addr".into(),
            sv1.to_string(),
            "--api-server-port".into(),
            api.port().to_string(),
            "--pool-address".into(),
            pools
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
            "-d".into(),
            "0.0000001T".into(),
            "--downstream-min-difficulty".into(),
            "0".into(),
            "--interval".into(),
            "0".into(),
            "--loglevel".into(),
            "debug".into(),
        ];
        if let Some(tp) = tp {
            args.extend(["--tp-address".into(), tp.to_string()]);
        }
        args.extend_from_slice(extra);
        let mut env = controlled_env();
        match source {
            ConfigurationSource::Cli => {}
            ConfigurationSource::Environment => {
                args.clear();
                env.extend([
                    ("LOCAL".into(), "true".into()),
                    ("TOKEN".into(), token.into()),
                    (
                        "POOL_ADDRESSES".into(),
                        pools
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                    ("LISTENING_ADDR".into(), sv1.to_string()),
                    ("API_SERVER_PORT".into(), api.port().to_string()),
                    ("DOWNSTREAM_HASHRATE".into(), "100000".into()),
                    ("DOWNSTREAM_MIN_DIFFICULTY".into(), "0".into()),
                    ("INTERVAL".into(), "0".into()),
                    ("LOGLEVEL".into(), "debug".into()),
                ]);
                if let Some(tp) = tp {
                    env.insert("TP_ADDRESS".into(), tp.to_string());
                }
            }
            ConfigurationSource::Toml => {
                let path = artifacts.path.join("proxy-config.toml");
                let config = serde_json::json!({"local":true,"token":token,"pool_addresses":pools.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "listening_addr":sv1.to_string(),"api_server_port":api.port().to_string(),"downstream_hashrate":"0.0000001T",
                    "downstream_min_difficulty":0.0,"interval":0,"loglevel":"debug","auto_update":false,"headful":false,"monitor":false});
                let mut config = config.as_object().expect("config object").clone();
                if let Some(tp) = tp {
                    config.insert("tp_address".into(), tp.to_string().into());
                }
                fs::write(&path, toml::to_string(&config)?)?;
                args = vec!["--config".into(), path.to_string_lossy().into()];
            }
        }
        let process = ManagedProcess::spawn(
            Path::new(env!("CARGO_BIN_EXE_dmnd-client")),
            &args,
            &env,
            artifacts,
            "proxy",
        )?;
        Ok(Self { process, sv1, api })
    }

    pub async fn ready(&mut self, deadline: Instant) -> TestResult {
        loop {
            self.process.ensure_running()?;
            if tokio::net::TcpStream::connect(self.sv1).await.is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(error(format!(
                    "proxy SV1 readiness timed out; {}",
                    self.process.log.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

pub struct CpuMinerProcess {
    pub process: ManagedProcess,
}

impl CpuMinerProcess {
    pub fn start(
        binary: &Path,
        artifacts: &Artifacts,
        sv1: SocketAddr,
        index: usize,
    ) -> TestResult<Self> {
        let args = vec![
            "-a".into(),
            "sha256d".into(),
            "-o".into(),
            format!("stratum+tcp://{sv1}"),
            "-u".into(),
            format!("e2e-worker-{index}"),
            "-p".into(),
            "x".into(),
            "-t".into(),
            "1".into(),
            "--no-longpoll".into(),
            "--retry-pause".into(),
            "1".into(),
        ];
        Ok(Self {
            process: ManagedProcess::spawn(
                binary,
                &args,
                &controlled_env(),
                artifacts,
                &format!("miner-{index}"),
            )?,
        })
    }
}

pub struct TestRig {
    pub artifacts: Artifacts,
    pub tp: Option<TpProcess>,
    pub pools: Vec<RunningPool>,
    pub proxy: Option<ProxyProcess>,
    pub miners: Vec<CpuMinerProcess>,
    pub retired_pools: Vec<super::pool::control::PoolSnapshot>,
    pub configuration_source: ConfigurationSource,
    pub network_hashpower: f64,
    miner_binary: PathBuf,
}

impl TestRig {
    pub fn new(name: &str) -> TestResult<Self> {
        // Check both before creating any child or listener. Selecting this suite never silently skips it.
        external_binary("TP_SIMULATOR_BIN")?;
        let miner_binary = external_binary("CPUMINER_BIN")?;
        Ok(Self {
            artifacts: Artifacts::new(name)?,
            tp: None,
            pools: vec![],
            proxy: None,
            miners: vec![],
            retired_pools: vec![],
            configuration_source: ConfigurationSource::Cli,
            network_hashpower: 1.0,
            miner_binary,
        })
    }

    pub async fn start(
        &mut self,
        jd: bool,
        pool_configs: Vec<PoolConfig>,
        miner_count: usize,
        token: &str,
        time_multiplier: f64,
    ) -> TestResult {
        let seed = 1;
        self.artifacts.json("configuration.json", &serde_json::json!({"jd":jd,"seed":seed,"time_multiplier":time_multiplier,"network_hashpower":self.network_hashpower,
            "miner_count":miner_count,"token":token,"pool_configs":format!("{pool_configs:?}"),"tp_artifact_version":std::env::var("TP_SIMULATOR_VERSION").ok()}))?;
        self.tp = Some(
            TpProcess::start(
                &external_binary("TP_SIMULATOR_BIN")?,
                &self.artifacts,
                seed,
                time_multiplier,
                self.network_hashpower,
            )
            .await?,
        );
        let tp = self.tp.as_ref().expect("TP").sv2;
        for mut config in pool_configs {
            config.tp_address = Some(tp);
            config.event_journal = Some(
                self.artifacts
                    .path
                    .join(format!("pool-{}-events.jsonl", self.pools.len())),
            );
            self.pools.push(RunningPool::start(config).await?);
            // Wait for real TP work before starting another connection. The supplied TP's
            // locked Noise helper races when multiple handshakes run at once.
            self.pools
                .last()
                .expect("pool")
                .handle()
                .wait_for(
                    EventMatcher::TipChanged,
                    Instant::now() + Duration::from_secs(15),
                )
                .await?;
        }
        let addresses = self.pools.iter().map(|p| p.address).collect::<Vec<_>>();
        self.proxy = Some(ProxyProcess::start(
            &self.artifacts,
            &addresses,
            jd.then_some(tp),
            token,
            &[],
            self.configuration_source,
        )?);
        if miner_count > 0 {
            self.proxy
                .as_mut()
                .expect("proxy")
                .ready(Instant::now() + Duration::from_secs(40))
                .await?;
            let address = self.proxy.as_ref().expect("proxy").sv1;
            for index in 0..miner_count {
                self.miners.push(CpuMinerProcess::start(
                    &self.miner_binary,
                    &self.artifacts,
                    address,
                    index,
                )?);
            }
        }
        Ok(())
    }

    pub async fn finish(mut self, mut result: TestResult) -> TestResult {
        let mut snapshots = std::mem::take(&mut self.retired_pools);
        for pool in &self.pools {
            match pool.handle().command(PoolCommand::Snapshot).await {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(e) => {
                    if result.is_ok() {
                        result = Err(e);
                    }
                }
            }
        }
        for miner in &mut self.miners {
            if let Err(e) = miner.process.shutdown().await {
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
        if let Some(proxy) = &mut self.proxy {
            if let Err(e) = proxy.process.shutdown().await {
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
        for pool in self.pools.drain(..) {
            if let Err(e) = pool.shutdown().await {
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
        if let Some(tp) = &mut self.tp {
            if let Err(e) = tp.process.shutdown().await {
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
        self.artifacts.finish(&result, &snapshots)?;
        result
    }
}
