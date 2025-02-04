use std::any::Any;
use std::collections::BTreeMap;
use std::time::Duration;

use frost_signer::config::{Config, Error as ConfigError};
use frost_signer::{
    net::{Error as HttpNetError, Message, NetListen},
    signing_round::{
        DkgBegin, DkgPublicShare, MessageTypes, NonceRequest, NonceResponse, Signable,
        SignatureShareRequest,
    },
    util::{parse_public_key, parse_public_keys},
};
use hashbrown::HashSet;
use tracing::{debug, info, warn};
use wtfrost::{
    bip340::{Error as Bip340Error, SchnorrProof},
    common::{PolyCommitment, PublicNonce, Signature, SignatureShare},
    compute,
    errors::AggregatorError,
    v1, Point, Scalar,
};

use serde::{Deserialize, Serialize};

pub const DEVNET_COORDINATOR_ID: usize = 0;
pub const DEVNET_COORDINATOR_DKG_ID: u64 = 0; //TODO: Remove, this is a correlation id

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    NetworkError(#[from] HttpNetError),
    #[error("No aggregate public key")]
    NoAggregatePublicKey,
    #[error("Aggregator failed to sign: {0}")]
    Aggregator(#[from] AggregatorError),
    #[error("BIP-340 error")]
    Bip340(Bip340Error),
    #[error("SchnorrProof failed to verify")]
    SchnorrProofFailed,
    #[error("Operation timed out")]
    Timeout,
    #[error("{0}")]
    ConfigError(#[from] ConfigError),
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    Dkg,
    Sign { msg: Vec<u8> },
    DkgSign { msg: Vec<u8> },
    GetAggregatePublicKey,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Coordinator<Network: NetListen> {
    id: u32, // Used for relay coordination
    current_dkg_id: u64,
    current_dkg_public_id: u64,
    current_sign_id: u64,
    current_sign_nonce_id: u64,
    total_signers: usize, // Assuming the signers cover all id:s in {1, 2, ..., total_signers}
    total_keys: usize,
    threshold: usize,
    network: Network,
    dkg_public_shares: BTreeMap<u32, DkgPublicShare>,
    public_nonces: BTreeMap<u32, NonceResponse>,
    signature_shares: BTreeMap<u32, Vec<SignatureShare>>,
    aggregate_public_key: Point,
    network_private_key: Scalar,
    signer_public_keys: Vec<String>,
    key_public_keys: Vec<String>,
    coordinator_public_key: String,
}

impl<Network: NetListen> Coordinator<Network> {
    pub fn new(id: usize, dkg_id: u64, config: &Config, network: Network) -> Self {
        let network_private_key = Scalar::try_from(config.network_private_key.as_str())
            .expect("failed to parse network_private_key from config");

        Self {
            id: id as u32,
            current_dkg_id: dkg_id,
            current_dkg_public_id: 1,
            current_sign_id: 1,
            current_sign_nonce_id: 1,
            total_signers: config.total_signers,
            total_keys: config.total_keys,
            threshold: config.keys_threshold,
            network,
            dkg_public_shares: Default::default(),
            public_nonces: Default::default(),
            aggregate_public_key: Point::default(),
            signature_shares: Default::default(),
            network_private_key,
            signer_public_keys: config.signer_public_keys.clone(),
            key_public_keys: config.key_public_keys.clone(),
            coordinator_public_key: config.coordinator_public_key.clone(),
        }
    }
}

impl<Network: NetListen> Coordinator<Network>
where
    Error: From<Network::Error>,
{
    pub fn run(&mut self, command: &Command) -> Result<(), Error> {
        match command {
            Command::Dkg => {
                self.run_distributed_key_generation()?;
                Ok(())
            }
            Command::Sign { msg } => {
                self.sign_message(msg)?;
                Ok(())
            }
            Command::DkgSign { msg } => {
                info!("sign msg: {:?}", msg);
                self.run_distributed_key_generation()?;
                self.sign_message(msg)?;
                Ok(())
            }
            Command::GetAggregatePublicKey => {
                let key = self.get_aggregate_public_key()?;
                info!("aggregate public key {}", key);
                Ok(())
            }
        }
    }

    pub fn run_distributed_key_generation(&mut self) -> Result<Point, Error> {
        self.start_public_shares()?;
        let public_key = self.wait_for_public_shares()?;
        self.start_private_shares()?;
        self.wait_for_dkg_end()?;
        Ok(public_key)
    }

    fn start_public_shares(&mut self) -> Result<(), Error> {
        self.dkg_public_shares.clear();
        self.current_dkg_id += 1;
        info!("Starting DKG round #{}", self.current_dkg_id);
        info!(
            "DKG Round #{}: Starting Public Share Distribution",
            self.current_dkg_id
        );
        let dkg_begin = DkgBegin {
            dkg_id: self.current_dkg_id,
        };

        let dkg_begin_message = Message {
            sig: dkg_begin.sign(&self.network_private_key).expect(""),
            msg: MessageTypes::DkgBegin(dkg_begin),
        };
        self.network.send_message(dkg_begin_message)?;
        Ok(())
    }

    fn start_private_shares(&mut self) -> Result<(), Error> {
        info!(
            "DKG Round #{}: Starting Private Share Distribution",
            self.current_dkg_id
        );
        let dkg_begin = DkgBegin {
            dkg_id: self.current_dkg_id,
        };
        let dkg_private_begin_msg = Message {
            sig: dkg_begin.sign(&self.network_private_key).expect(""),
            msg: MessageTypes::DkgPrivateBegin(dkg_begin),
        };
        self.network.send_message(dkg_private_begin_msg)?;
        Ok(())
    }

    fn collect_nonces(&mut self) -> Result<(), Error> {
        self.public_nonces.clear();

        let nonce_request = NonceRequest {
            dkg_id: self.current_dkg_id,
            sign_id: self.current_sign_id,
            sign_nonce_id: self.current_sign_nonce_id,
        };

        let nonce_request_message = Message {
            sig: nonce_request
                .sign(&self.network_private_key)
                .expect("Failed to sign NonceRequest"),
            msg: MessageTypes::NonceRequest(nonce_request),
        };

        debug!(
            "dkg_id #{} sign_id #{} sign_nonce_id #{}. NonceRequest sent",
            self.current_dkg_id, self.current_sign_id, self.current_sign_nonce_id
        );
        self.network.send_message(nonce_request_message)?;

        loop {
            match self.wait_for_next_message()?.msg {
                MessageTypes::NonceRequest(_) => {}
                MessageTypes::NonceResponse(nonce_response) => {
                    let signer_id = nonce_response.signer_id;
                    self.public_nonces.insert(signer_id, nonce_response);
                    debug!(
                        "NonceResponse from signer #{:?}. Got {} nonce responses of threshold {}",
                        signer_id,
                        self.public_nonces.len(),
                        self.threshold,
                    );
                }
                msg => {
                    warn!("NonceLoop Got unexpected message {:?})", msg.type_id());
                }
            }

            if self.public_nonces.len() == self.total_signers {
                debug!("Nonce threshold of {} met.", self.threshold);
                break;
            }
        }
        Ok(())
    }

    #[allow(non_snake_case)]
    fn compute_aggregate_nonce(&mut self, msg: &[u8]) -> Result<Point, Error> {
        info!("Computing aggregate nonce...");
        self.collect_nonces()?;
        // XXX this needs to be key_ids for v1 and signer_ids for v2
        let party_ids = self
            .public_nonces
            .values()
            .flat_map(|pn| {
                pn.key_ids
                    .iter()
                    .map(|id| *id as usize)
                    .collect::<Vec<usize>>()
            })
            .collect::<Vec<usize>>();
        let nonces = self
            .public_nonces
            .values()
            .flat_map(|pn| pn.nonces.clone())
            .collect::<Vec<PublicNonce>>();
        let (_, R) = compute::intermediate(msg, &party_ids, &nonces);
        Ok(R)
    }

    fn request_signature_shares(
        &self,
        nonce_responses: &[NonceResponse],
        msg: &[u8],
    ) -> Result<(), Error> {
        let signature_share_request = SignatureShareRequest {
            dkg_id: self.current_dkg_id,
            sign_id: self.current_sign_id,
            correlation_id: 0,
            nonce_responses: nonce_responses.to_vec(),
            message: msg.to_vec(),
        };

        info!(
            "Sending SignShareRequest dkg_id #{} sign_id #{} to signers",
            signature_share_request.dkg_id, signature_share_request.sign_id
        );

        let signature_share_request_message = Message {
            sig: signature_share_request
                .sign(&self.network_private_key)
                .expect("Failed to sign SignShareRequest"),
            msg: MessageTypes::SignShareRequest(signature_share_request),
        };

        self.network.send_message(signature_share_request_message)?;

        Ok(())
    }

    fn collect_signature_shares(&mut self) -> Result<(), Error> {
        // get the parties who responded with a nonce
        let mut signers: HashSet<u32> = HashSet::from_iter(self.public_nonces.keys().cloned());
        while !signers.is_empty() {
            match self.wait_for_next_message()?.msg {
                MessageTypes::SignShareResponse(response) => {
                    if let Some(_party_id) = signers.take(&response.signer_id) {
                        info!(
                            "Insert signature shares for signer_id {}",
                            &response.signer_id
                        );
                        self.signature_shares
                            .insert(response.signer_id, response.signature_shares.clone());
                    }
                    debug!(
                        "signature shares for {} received.  left to receive: {:?}",
                        response.signer_id, signers
                    );
                }
                MessageTypes::SignShareRequest(_) => {}
                msg => {
                    warn!("SigShare loop got unexpected msg {:?}", msg.type_id());
                }
            }
        }
        Ok(())
    }

    #[allow(non_snake_case)]
    pub fn sign_message(&mut self, msg: &[u8]) -> Result<(Signature, SchnorrProof), Error> {
        debug!("Attempting to Sign Message");
        if self.aggregate_public_key == Point::default() {
            return Err(Error::NoAggregatePublicKey);
        }

        //Continually compute a new aggregate nonce until we have a valid even R
        loop {
            let R = self.compute_aggregate_nonce(msg)?;
            if R.has_even_y() {
                debug!("Success: R has even y coord: {}", &R);
                break;
            } else {
                warn!("Failure: R does not have even y coord: {}", R);
            }
        }

        // make an array of dkg public share polys for SignatureAggregator
        debug!(
            "collecting commitments from 1..{} in {:?}",
            self.total_keys,
            self.dkg_public_shares.keys().collect::<Vec<&u32>>()
        );
        let polys: Vec<PolyCommitment> = self
            .dkg_public_shares
            .values()
            .map(|ps| ps.public_share.clone())
            .collect();

        debug!(
            "SignatureAggregator::new total_keys: {} threshold: {} commitments: {}",
            self.total_keys,
            self.threshold,
            polys.len()
        );

        let mut aggregator = v1::SignatureAggregator::new(self.total_keys, self.threshold, polys)?;

        let nonce_responses: Vec<NonceResponse> = self.public_nonces.values().cloned().collect();

        // request signature shares
        self.request_signature_shares(&nonce_responses, msg)?;
        self.collect_signature_shares()?;

        let nonces = nonce_responses
            .iter()
            .flat_map(|nr| nr.nonces.clone())
            .collect::<Vec<PublicNonce>>();
        let shares = &self
            .public_nonces
            .iter()
            .flat_map(|(i, _)| self.signature_shares[i].clone())
            .collect::<Vec<SignatureShare>>();

        info!(
            "aggregator.sign({:?}, {:?}, {:?})",
            msg,
            nonces.len(),
            shares.len()
        );

        let sig = aggregator.sign(msg, &nonces, shares)?;

        info!("Signature ({}, {})", sig.R, sig.z);

        let proof = SchnorrProof::new(&sig).map_err(Error::Bip340)?;

        info!("SchnorrProof ({}, {})", proof.r, proof.s);

        if !proof.verify(&self.aggregate_public_key.x(), msg) {
            warn!("SchnorrProof failed to verify!");
            return Err(Error::SchnorrProofFailed);
        }

        Ok((sig, proof))
    }

    fn calculate_aggregate_public_key(&mut self) -> Result<Point, Error> {
        self.aggregate_public_key = self
            .dkg_public_shares
            .iter()
            .fold(Point::default(), |s, (_, dps)| s + dps.public_share.A[0]);
        Ok(self.aggregate_public_key)
    }

    pub fn get_aggregate_public_key(&self) -> Result<Point, Error> {
        if self.aggregate_public_key == Point::default() {
            Err(Error::NoAggregatePublicKey)
        } else {
            Ok(self.aggregate_public_key)
        }
    }

    fn wait_for_public_shares(&mut self) -> Result<Point, Error> {
        let mut ids_to_await: HashSet<usize> = (1..=self.total_signers).collect();

        info!(
            "DKG Round #{}: waiting for Dkg Public Shares from signers {:?}",
            self.current_dkg_id, ids_to_await
        );

        loop {
            if ids_to_await.is_empty() {
                let key = self.calculate_aggregate_public_key()?;
                // check to see if aggregate public key has even y
                if key.has_even_y() {
                    debug!("Aggregate public key has even y coord!");
                    info!("Aggregate public key: {}", key);
                    self.aggregate_public_key = key;
                    return Ok(key);
                } else {
                    warn!("DKG Round #{} Failed: Aggregate public key does not have even y coord, re-running dkg.", self.current_dkg_id);
                    ids_to_await = (1..=self.total_signers).collect();
                    self.start_public_shares()?;
                }
            }

            match self.wait_for_next_message()?.msg {
                MessageTypes::DkgPublicEnd(dkg_end_msg) => {
                    ids_to_await.remove(&dkg_end_msg.signer_id);
                    debug!(
                        "DKG_Public_End round #{} from signer #{}. Waiting on {:?}",
                        dkg_end_msg.dkg_id, dkg_end_msg.signer_id, ids_to_await
                    );
                }
                MessageTypes::DkgPublicShare(dkg_public_share) => {
                    self.dkg_public_shares
                        .insert(dkg_public_share.party_id, dkg_public_share.clone());

                    debug!(
                        "DKG round #{} DkgPublicShare from party #{}",
                        dkg_public_share.dkg_id, dkg_public_share.party_id
                    );
                }
                _ => {}
            }
        }
    }

    fn wait_for_dkg_end(&mut self) -> Result<(), Error> {
        let mut ids_to_await: HashSet<usize> = (1..=self.total_signers).collect();
        info!(
            "DKG Round #{}: waiting for Dkg End from signers {:?}",
            self.current_dkg_id, ids_to_await
        );
        while !ids_to_await.is_empty() {
            if let MessageTypes::DkgEnd(dkg_end_msg) = self.wait_for_next_message()?.msg {
                ids_to_await.remove(&dkg_end_msg.signer_id);
                debug!(
                    "DKG_End round #{} from signer #{}. Waiting on {:?}",
                    dkg_end_msg.dkg_id, dkg_end_msg.signer_id, ids_to_await
                );
            }
        }
        Ok(())
    }

    fn wait_for_next_message(&mut self) -> Result<Message, Error> {
        let signer_public_keys = parse_public_keys(&self.key_public_keys);
        let key_public_keys = parse_public_keys(&self.key_public_keys);
        let coordinator_public_key = parse_public_key(&self.coordinator_public_key);

        let get_next_message = || {
            self.network.poll(self.id);
            match self
                .network
                .next_message()
                .ok_or_else(|| "No message yet".to_owned())
                .map_err(backoff::Error::transient)
            {
                Ok(m) => {
                    match &m.msg {
                        MessageTypes::DkgBegin(msg) | MessageTypes::DkgPrivateBegin(msg) => {
                            assert!(msg.verify(&m.sig, &coordinator_public_key))
                        }
                        MessageTypes::DkgEnd(msg) | MessageTypes::DkgPublicEnd(msg) => {
                            assert!(msg.verify(&m.sig, &signer_public_keys[msg.signer_id - 1]))
                        }
                        MessageTypes::DkgPublicShare(msg) => {
                            assert!(msg.verify(&m.sig, &key_public_keys[msg.party_id as usize - 1]))
                        }
                        MessageTypes::DkgPrivateShares(msg) => {
                            assert!(msg.verify(&m.sig, &key_public_keys[msg.key_id as usize]))
                        }
                        MessageTypes::DkgQuery(msg) => {
                            assert!(msg.verify(&m.sig, &coordinator_public_key))
                        }
                        MessageTypes::DkgQueryResponse(msg) => {
                            let key_id = msg.public_share.id.id.get_u32();
                            assert!(msg.verify(&m.sig, &key_public_keys[key_id as usize - 1]))
                        }
                        MessageTypes::NonceRequest(msg) => {
                            assert!(msg.verify(&m.sig, &coordinator_public_key))
                        }
                        MessageTypes::NonceResponse(msg) => {
                            assert!(msg.verify(&m.sig, &signer_public_keys[msg.signer_id as usize]))
                        }
                        MessageTypes::SignShareRequest(msg) => {
                            assert!(msg.verify(&m.sig, &coordinator_public_key))
                        }
                        MessageTypes::SignShareResponse(msg) => {
                            assert!(msg.verify(&m.sig, &signer_public_keys[msg.signer_id as usize]))
                        }
                    }
                    Ok(m)
                }
                Err(e) => Err(e),
            }
        };

        let notify = |_err, dur| {
            debug!("No message. Next poll in {:?}", dur);
        };

        let backoff_timer = backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(2))
            .with_max_interval(Duration::from_millis(128))
            .build();
        backoff::retry_notify(backoff_timer, get_next_message, notify).map_err(|_| Error::Timeout)
    }
}
