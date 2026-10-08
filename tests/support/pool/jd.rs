use super::{
    jobs::{pool_output, EXTRANONCE_LEN},
    templates::TemplateStore,
    transport::PeerId,
};
use crate::support::{error, TestResult};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256d, Hash},
    Transaction,
};
use roles_logic_sv2::{
    job_creator::extended_job_from_custom_job,
    job_declaration_sv2::{DeclareMiningJob, ProvideMissingTransactionsSuccess},
    mining_sv2::{NewExtendedMiningJob, SetCustomMiningJob},
};
use std::collections::{HashMap, HashSet};

pub struct DeclarationState {
    pub peer: PeerId,
    pub identity: String,
    pub message: DeclareMiningJob<'static>,
    pub missing: Vec<u16>,
    pub signed_token: Option<Vec<u8>>,
}

#[derive(Default)]
pub struct TokenRegistry {
    next: u64,
    allocated: HashMap<Vec<u8>, (PeerId, String)>,
    pub declarations: HashMap<(PeerId, u32), DeclarationState>,
    signed: HashMap<Vec<u8>, (PeerId, u32)>,
}

impl TokenRegistry {
    pub fn allocate(&mut self, peer: PeerId, identity: &str) -> Vec<u8> {
        self.next += 1;
        let token = format!("allocated-{}-{}", peer.0, self.next).into_bytes();
        self.allocated
            .insert(token.clone(), (peer, identity.to_string()));
        token
    }

    pub fn declare(
        &mut self,
        peer: PeerId,
        identity: &str,
        message: DeclareMiningJob<'static>,
        store: &TemplateStore,
        force_missing: bool,
    ) -> TestResult<Vec<u16>> {
        let token = message.mining_job_token.to_vec();
        if self.allocated.get(&token) != Some(&(peer, identity.to_string())) {
            return Err(error("invalid-mining-job-token"));
        }
        if self.declarations.contains_key(&(peer, message.request_id)) {
            return Err(error("duplicate-request-id"));
        }
        let coinbase: Transaction = deserialize(
            &[
                message.coinbase_prefix.as_ref(),
                &[0; EXTRANONCE_LEN as usize],
                message.coinbase_suffix.as_ref(),
            ]
            .concat(),
        )?;
        if !coinbase.is_coinbase() || !pays_pool(&coinbase) {
            return Err(error("invalid-declaration-coinbase"));
        }
        let mut unique = HashSet::new();
        let txs = message.tx_list.to_vec();
        let mut missing = vec![];
        for (index, id) in txs.into_iter().enumerate() {
            let id: [u8; 32] = id.to_vec().try_into().expect("U256");
            if !unique.insert(id) {
                return Err(error("duplicate-transaction"));
            }
            if force_missing || !store.transactions.contains_key(&id) {
                missing.push(index as u16);
            }
        }
        self.allocated.remove(&token);
        self.declarations.insert(
            (peer, message.request_id),
            DeclarationState {
                peer,
                identity: identity.to_string(),
                message,
                missing: missing.clone(),
                signed_token: None,
            },
        );
        Ok(missing)
    }

    pub fn provide(
        &mut self,
        peer: PeerId,
        message: ProvideMissingTransactionsSuccess<'static>,
        store: &mut TemplateStore,
    ) -> TestResult {
        let declaration = self
            .declarations
            .get_mut(&(peer, message.request_id))
            .ok_or_else(|| error("unknown-declaration"))?;
        let txs = message.transaction_list.into_inner();
        if txs.len() != declaration.missing.len() {
            return Err(error("missing-transaction-count"));
        }
        let ids = declaration.message.tx_list.to_vec();
        for (position, bytes) in declaration.missing.iter().zip(txs) {
            let tx: Transaction = deserialize(bytes.as_ref())?;
            let expected: [u8; 32] = ids[*position as usize].to_vec().try_into().expect("U256");
            if tx.is_coinbase() || tx.compute_wtxid().to_byte_array() != expected {
                return Err(error("missing-transaction-hash"));
            }
            store.transactions.insert(expected, bytes.to_vec());
        }
        declaration.missing.clear();
        Ok(())
    }

    pub fn approve(&mut self, peer: PeerId, request_id: u32) -> TestResult<Vec<u8>> {
        let state = self
            .declarations
            .get_mut(&(peer, request_id))
            .ok_or_else(|| error("unknown-declaration"))?;
        if !state.missing.is_empty() || state.signed_token.is_some() {
            return Err(error("declaration-not-ready"));
        }
        self.next += 1;
        let token = format!("signed-{}-{}", peer.0, self.next).into_bytes();
        state.signed_token = Some(token.clone());
        self.signed.insert(token.clone(), (peer, request_id));
        Ok(token)
    }

    pub fn custom(
        &self,
        identity: &str,
        message: &SetCustomMiningJob<'static>,
        store: &TemplateStore,
    ) -> TestResult<NewExtendedMiningJob<'static>> {
        let key = self
            .signed
            .get(message.token.as_ref())
            .ok_or_else(|| error("undeclared-token"))?;
        let state = &self.declarations[key];
        if state.identity != identity
            || state.peer != key.0
            || state.message.version != message.version
        {
            return Err(error("custom-job-identity-or-version"));
        }
        // Validate encoded outputs before the locked job constructor, which indexes output[0].
        use bitcoin::consensus::Decodable;
        let mut outputs = message.coinbase_tx_outputs.as_ref();
        if outputs.is_empty() {
            return Err(error("custom-job-empty-outputs"));
        }
        while !outputs.is_empty() {
            bitcoin::TxOut::consensus_decode(&mut outputs)?;
        }
        let job = extended_job_from_custom_job(message, 0, EXTRANONCE_LEN)
            .map_err(|e| error(format!("custom coinbase: {e:?}")))?;
        if job.coinbase_tx_prefix != state.message.coinbase_prefix
            || job.coinbase_tx_suffix != state.message.coinbase_suffix
        {
            return Err(error("custom-job-coinbase-mismatch"));
        }
        let txids = state
            .message
            .tx_list
            .to_vec()
            .into_iter()
            .map(|id| {
                let id: [u8; 32] = id.to_vec().try_into().expect("U256");
                let bytes = store
                    .transactions
                    .get(&id)
                    .ok_or_else(|| error("custom-job-unknown-transaction"))?;
                let tx: Transaction = deserialize(bytes)?;
                Ok(tx.compute_txid().to_byte_array())
            })
            .collect::<TestResult<Vec<_>>>()?;
        let path = coinbase_merkle_path(&txids);
        let advertised = message
            .merkle_path
            .to_vec()
            .into_iter()
            .map(|id| id.to_vec())
            .collect::<Vec<_>>();
        if advertised != path.iter().map(|h| h.to_vec()).collect::<Vec<_>>() {
            return Err(error("custom-job-merkle-mismatch"));
        }
        let coinbase: Transaction = deserialize(
            &[
                job.coinbase_tx_prefix.as_ref(),
                &[0; EXTRANONCE_LEN as usize],
                job.coinbase_tx_suffix.as_ref(),
            ]
            .concat(),
        )?;
        if !pays_pool(&coinbase) {
            return Err(error("custom-job-payout-missing"));
        }
        Ok(job)
    }

    pub fn coinbase_output() -> Vec<u8> {
        serialize(&pool_output())
    }
}

fn pays_pool(coinbase: &Transaction) -> bool {
    // This test pool receives the whole reward; commitment outputs carry no value.
    let script = pool_output().script_pubkey;
    coinbase
        .output
        .iter()
        .any(|out| out.script_pubkey == script && out.value > bitcoin::Amount::ZERO)
        && coinbase
            .output
            .iter()
            .all(|out| out.value == bitcoin::Amount::ZERO || out.script_pubkey == script)
}

pub fn coinbase_merkle_path(txids: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let mut layer = vec![[0; 32]];
    layer.extend_from_slice(txids);
    let mut path = vec![];
    while layer.len() > 1 {
        path.push(layer[1]);
        if layer.len() % 2 == 1 {
            layer.push(*layer.last().expect("nonempty"));
        }
        layer = layer
            .chunks_exact(2)
            .map(|pair| {
                sha256d::Hash::hash(&[pair[0].as_slice(), pair[1].as_slice()].concat())
                    .to_byte_array()
            })
            .collect();
    }
    path
}
