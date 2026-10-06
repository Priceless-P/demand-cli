use super::{error, TestResult};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    time::Instant,
};

pub struct Sv1Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    pub notifications: Vec<Value>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SolvedSubmission {
    pub job: String,
    pub extranonce: String,
    pub ntime: String,
    pub nonce: String,
}

impl SolvedSubmission {
    pub async fn solve(
        subscription: &Value,
        notify: &Value,
        target: [u8; 32],
        deadline: Instant,
    ) -> TestResult<Self> {
        use bitcoin::{
            block::{Header, Version},
            consensus::deserialize,
            hashes::Hash,
            hex::FromHex,
            BlockHash, CompactTarget, Target, Transaction, TxMerkleNode,
        };
        let fields = &notify["params"];
        let string = |value: &Value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| error("invalid SV1 work field"))
        };
        let job = string(&fields[0])?;
        let size = subscription["result"][2]
            .as_u64()
            .ok_or_else(|| error("invalid subscription extranonce size"))?;
        if size > 32 {
            return Err(error("unsupported SV1 extranonce size"));
        }
        let extranonce = "00".repeat(size as usize);
        let bytes = format!(
            "{}{}{}{}",
            string(&fields[2])?,
            string(&subscription["result"][1])?,
            extranonce,
            string(&fields[3])?
        );
        let coinbase: Transaction = deserialize(&Vec::<u8>::from_hex(&bytes)?)?;
        let path = fields[4]
            .as_array()
            .ok_or_else(|| error("invalid merkle path"))?
            .iter()
            .map(|value| Ok(<[u8; 32]>::from_hex(&string(value)?)?))
            .collect::<TestResult<Vec<_>>>()?;
        let root = super::pool::shares::merkle_root(coinbase.compute_txid().to_byte_array(), &path);
        let mut previous = <[u8; 32]>::from_hex(&string(&fields[1])?)?;
        for word in previous.chunks_exact_mut(4) {
            word.reverse();
        }
        let ntime = string(&fields[7])?;
        let header = Header {
            version: Version::from_consensus(u32::from_str_radix(&string(&fields[5])?, 16)? as i32),
            prev_blockhash: BlockHash::from_byte_array(previous),
            merkle_root: TxMerkleNode::from_byte_array(root),
            time: u32::from_str_radix(&ntime, 16)?,
            bits: CompactTarget::from_consensus(u32::from_str_radix(&string(&fields[6])?, 16)?),
            nonce: 0,
        };
        let target = Target::from_le_bytes(target);
        let nonce = tokio::task::spawn_blocking(move || {
            // Serialize once and use SHA256's hardware backend for the scripted CPU search.
            // The pool still reconstructs and validates the resulting header independently.
            use sha2::{Digest, Sha256};
            let mut bytes = bitcoin::consensus::serialize(&header);
            for nonce in 0..u32::MAX {
                if nonce % 1024 == 0 && Instant::now() >= deadline {
                    return Err(error("scripted share solver timed out"));
                }
                bytes[76..80].copy_from_slice(&nonce.to_le_bytes());
                let hash = Sha256::digest(Sha256::digest(&bytes));
                if target.is_met_by(BlockHash::from_byte_array(hash.into())) {
                    return Ok(format!("{nonce:08x}"));
                }
            }
            Err(error("scripted share solver exhausted nonce window"))
        })
        .await??;
        Ok(Self {
            job,
            extranonce,
            ntime,
            nonce,
        })
    }

    pub async fn submit(
        &self,
        client: &mut Sv1Client,
        worker: &str,
        deadline: Instant,
    ) -> TestResult<Value> {
        client
            .submit(
                worker,
                &self.job,
                &self.extranonce,
                &self.ntime,
                &self.nonce,
                deadline,
            )
            .await
    }
}

impl Sv1Client {
    pub async fn connect(address: SocketAddr, deadline: Instant) -> TestResult<Self> {
        let stream = tokio::time::timeout_at(deadline, TcpStream::connect(address)).await??;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 0,
            notifications: vec![],
        })
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> TestResult<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(&json!({"id":id,"method":method,"params":params}).to_string())
            .await?;
        loop {
            let message = self.read(deadline).await?;
            if message["id"] == id {
                return Ok(message);
            }
            self.notifications.push(message);
        }
    }

    pub async fn authorize(&mut self, worker: &str, deadline: Instant) -> TestResult<Value> {
        let subscription = self
            .request("mining.subscribe", json!(["mining-e2e"]), deadline)
            .await?;
        let response = self
            .request("mining.authorize", json!([worker, "x"]), deadline)
            .await?;
        if response["result"] != true {
            return Err(error(format!("SV1 authorization failed: {response}")));
        }
        Ok(subscription)
    }

    pub async fn notify(&mut self, deadline: Instant) -> TestResult<Value> {
        if let Some(index) = self
            .notifications
            .iter()
            .position(|m| m["method"] == "mining.notify")
        {
            return Ok(self.notifications.remove(index));
        }
        loop {
            let message = self.read(deadline).await?;
            if message["method"] == "mining.notify" {
                return Ok(message);
            }
            self.notifications.push(message);
        }
    }

    pub async fn submit(
        &mut self,
        worker: &str,
        job: &str,
        extranonce: &str,
        ntime: &str,
        nonce: &str,
        deadline: Instant,
    ) -> TestResult<Value> {
        self.request(
            "mining.submit",
            json!([worker, job, extranonce, ntime, nonce]),
            deadline,
        )
        .await
    }

    pub async fn send_raw(&mut self, line: &str) -> TestResult {
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }

    pub async fn read(&mut self, deadline: Instant) -> TestResult<Value> {
        let mut line = String::new();
        let count = tokio::time::timeout_at(deadline, self.reader.read_line(&mut line)).await??;
        if count == 0 {
            return Err(error("SV1 peer disconnected"));
        }
        if line.len() > 65536 {
            return Err(error("SV1 response too large"));
        }
        Ok(serde_json::from_str(&line)?)
    }
}
