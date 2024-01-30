use crate::{
	abi::{to_ismp_event, EvmHost, PingModule},
	consts::{
		REQUEST_COMMITMENTS_SLOT, REQUEST_RECEIPTS_SLOT, RESPONSE_COMMITMENTS_SLOT,
		RESPONSE_RECEIPTS_SLOT,
	},
};
use ethabi::ethereum_types::{H256, U256};
use ethers::{
	core::k256::ecdsa::SigningKey,
	prelude::{k256::SecretKey, LocalWallet, MiddlewareBuilder, SignerMiddleware, Wallet},
	providers::{Middleware, Provider, Ws},
	signers::Signer,
};
use frame_support::crypto::ecdsa::ECDSAExt;
use ismp::{
	consensus::ConsensusStateId,
	events::Event,
	host::{Ethereum, StateMachine},
};
use reconnecting_jsonrpsee_ws_client::{Client, ExponentialBackoff, PingConfig};
use serde::{Deserialize, Serialize};
use sp_core::{bytes::from_hex, keccak_256, Pair, H160};
use std::{sync::Arc, time::Duration};
use tesseract_primitives::{IsmpHost, NonceProvider};

pub mod abi;
pub mod arbitrum;
pub mod consts;
mod host;
#[cfg(any(feature = "testing", test))]
pub mod mock;
pub mod optimism;
pub mod provider;
pub mod tx;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvmConfig {
	/// WS url for execution client
	pub execution_ws: String,
	/// State machine Identifier for this client on it's counterparties.
	pub state_machine: StateMachine,
	/// Consensus state id for the consensus client on counterparty chain
	pub consensus_state_id: String,
	/// Ismp Host contract address
	pub ismp_host: H160,
	/// Ismp Handler contract address
	pub handler: H160,
	/// Relayer account private key
	pub signer: String,
	/// Etherscan API keys
	pub etherscan_api_keys: String,
}

impl Default for EvmConfig {
	fn default() -> Self {
		Self {
			execution_ws: Default::default(),
			state_machine: StateMachine::Ethereum(Ethereum::ExecutionLayer),
			consensus_state_id: Default::default(),
			ismp_host: Default::default(),
			handler: Default::default(),
			signer: Default::default(),
			etherscan_api_keys: Default::default(),
		}
	}
}

/// Core EVM client.
pub struct EvmClient<I> {
	/// Ismp naive implementation
	pub host: Option<I>,
	/// Execution Rpc client
	pub client: Arc<Provider<Ws>>,
	/// Transaction signer
	pub signer: Arc<SignerMiddleware<Provider<Ws>, Wallet<SigningKey>>>,
	/// Public Key Address
	pub address: Vec<u8>,
	/// Consensus state Id
	pub consensus_state_id: ConsensusStateId,
	/// State machine Identifier for this client.
	state_machine: StateMachine,
	/// Latest state machine height.
	initial_height: u64,
	/// Ismp Host contract address
	ismp_host: H160,
	/// Ismp Handler contract address
	handler: H160,
	/// Config
	config: EvmConfig,
	/// Nonce Provider
	nonce_provider: Option<NonceProvider>,
	/// Jsonrpsee client for event susbscription, ethers does not expose a Send and Sync stream for
	/// susbcribing to contract logs
	pub rpc_client: Arc<Client>,
	/// etherscan api-keys, used for gas estimation
	pub etherscan_keys: String,
	/// EVM chain Id.
	pub chain_id: u64,
}

impl<I> EvmClient<I>
where
	I: IsmpHost + Send + Sync,
{
	pub async fn new(host: Option<I>, config: EvmConfig) -> Result<Self, anyhow::Error> {
		let config_clone = config.clone();
		let bytes = from_hex(config.signer.as_str())?;
		let signer = sp_core::ecdsa::Pair::from_seed_slice(&bytes)?;
		let address = signer.public().to_eth_address().expect("Infallible").to_vec();

		let provider =
			Provider::<Ws>::connect_with_reconnects(config.execution_ws.clone(), 1000).await?;
		let client = Arc::new(provider.clone());
		let chain_id = client.get_chainid().await?.low_u64();
		let signer = LocalWallet::from(SecretKey::from_slice(signer.seed().as_slice())?)
			.with_chain_id(chain_id);
		let signer = Arc::new(provider.with_signer(signer));
		let consensus_state_id = {
			let mut consensus_state_id: ConsensusStateId = Default::default();
			consensus_state_id.copy_from_slice(config.consensus_state_id.as_bytes());
			consensus_state_id
		};
		let rpc_client = Client::builder()
			.retry_policy(ExponentialBackoff::from_millis(100))
			.enable_ws_ping(
				PingConfig::new()
					.ping_interval(Duration::from_secs(6))
					.inactive_limit(Duration::from_secs(30)),
			)
			.build(config.execution_ws)
			.await?;

		let latest_height = client.get_block_number().await?.as_u64();
		Ok(Self {
			host,
			client,
			signer,
			address,
			consensus_state_id,
			state_machine: config.state_machine,
			initial_height: latest_height,
			ismp_host: config.ismp_host,
			handler: config.handler,
			config: config_clone,
			nonce_provider: None,
			rpc_client: Arc::new(rpc_client),
			etherscan_keys: config.etherscan_api_keys,
			chain_id,
		})
	}

	pub async fn events(&self, from: u64, to: u64) -> Result<Vec<Event>, anyhow::Error> {
		let client = Arc::new(self.client.clone());
		let contract = EvmHost::new(self.ismp_host, client);
		let events = contract
			.events()
			.address(self.ismp_host.into())
			.from_block(from)
			.to_block(to)
			.query()
			.await?
			.into_iter()
			.filter_map(|ev| to_ismp_event(ev).ok())
			.collect::<_>();
		Ok(events)
	}

	/// Set the consensus state on the IsmpHost
	pub async fn set_consensus_state(&self, consensus_state: Vec<u8>) -> Result<(), anyhow::Error> {
		let contract = EvmHost::new(self.ismp_host, self.signer.clone());
		let call = contract.set_consensus_state(consensus_state.clone().into());

		// let gas = call.estimate_gas().await?; // todo: fix estimate gas
		// dbg!(gas);
		call.nonce(self.get_nonce().await?).gas(10_000_000).send().await?.await?;

		Ok(())
	}

	/// Dispatch a test request to the parachain.
	pub async fn dispatch_to_parachain(
		&self,
		address: H160,
		para_id: u32,
	) -> Result<(), anyhow::Error> {
		let contract = PingModule::new(address, self.signer.clone());
		let call = contract.dispatch_to_parachain(para_id.into());

		// let gas = call.estimate_gas().await?; // todo: fix estimate gas
		// dbg!(gas);
		call.nonce(self.get_nonce().await?).gas(10_000_000).send().await?.await?;

		Ok(())
	}

	pub fn request_commitment_key(&self, key: H256) -> H256 {
		// commitment is mapped to a  bool
		derive_map_key(key.0.to_vec(), REQUEST_COMMITMENTS_SLOT)
	}

	pub fn response_commitment_key(&self, key: H256) -> H256 {
		// commitment is mapped to a  bool
		derive_map_key(key.0.to_vec(), RESPONSE_COMMITMENTS_SLOT)
	}

	pub fn request_receipt_key(&self, key: H256) -> H256 {
		// commitment is mapped to a  bool
		derive_map_key(key.0.to_vec(), REQUEST_RECEIPTS_SLOT)
	}

	pub fn response_receipt_key(&self, key: H256) -> H256 {
		// commitment is mapped to a  bool
		derive_map_key(key.0.to_vec(), RESPONSE_RECEIPTS_SLOT)
	}

	pub async fn get_nonce(&self) -> Result<u64, anyhow::Error> {
		if let Some(nonce_provider) = self.nonce_provider.as_ref() {
			return Ok(nonce_provider.get_nonce().await)
		}
		Err(anyhow::anyhow!("Nonce provider not set on client"))
	}

	pub async fn read_nonce(&self) -> Result<u64, anyhow::Error> {
		if let Some(nonce_provider) = self.nonce_provider.as_ref() {
			return Ok(nonce_provider.read_nonce().await)
		}
		Err(anyhow::anyhow!("Nonce provider not set on client"))
	}
}

fn derive_map_key(mut key: Vec<u8>, slot: u64) -> H256 {
	let mut bytes = [0u8; 32];
	U256::from(slot as u64).to_big_endian(&mut bytes);
	key.extend_from_slice(&bytes);
	keccak_256(&key).into()
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct GasResult {
	pub safe_gas_price: String,
	pub usd_price: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GasResultEthereum {
	pub safe_gas_price: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct EthPriceResult {
	pub ethusd: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GasResponse {
	pub result: GasResult,
}

#[derive(Debug, Deserialize)]
pub struct GasResponseEthereum {
	pub result: GasResultEthereum,
}

#[derive(Debug, Deserialize)]
pub struct EthPriceResponse {
	pub result: EthPriceResult,
}
