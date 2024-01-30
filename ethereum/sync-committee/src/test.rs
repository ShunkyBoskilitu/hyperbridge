use crate::{HostConfig, SyncCommitteeHost};
use codec::Decode;
use futures::StreamExt;
use ismp::host::{Ethereum, StateMachine};
use ismp_sync_committee::{
	arbitrum::verify_arbitrum_payload, optimism::verify_optimism_payload, types::BeaconClientUpdate,
};
use sync_committee_primitives::constants::sepolia::Sepolia;
use tesseract_evm::{
	arbitrum::client::{ArbHost, HostConfig as ArbHostConfig},
	mock::Host,
	optimism::client::{HostConfig as OpHostConfig, OpHost},
	EvmClient, EvmConfig,
};
use tesseract_primitives::{mocks::MockHost, IsmpHost};

#[tokio::test]
async fn check_consensus_notification() -> anyhow::Result<()> {
	dotenv::dotenv().ok();
	let op_orl = std::env::var("OP_URL").expect("OP_URL must be set.");
	let arb_orl = std::env::var("ARB_URL").expect("OP_URL must be set.");
	let base_orl = std::env::var("BASE_URL").expect("BASE_URL must be set.");
	let geth_url = std::env::var("GETH_URL").expect("GETH_URL must be set.");
	let beacon_url = std::env::var("BEACON_URL").expect("BEACON_URL must be set.");
	let chain_a = MockHost::new(
		ismp_sync_committee::types::ConsensusState {
			frozen_height: Default::default(),
			light_client_state: Default::default(),
			ismp_contract_addresses: Default::default(),
			l2_oracle_address: Default::default(),
			rollup_core_address: Default::default(),
		},
		0,
		StateMachine::Polygon,
	);

	let chain_b = {
		let config = EvmConfig {
			execution_ws: geth_url.clone(),
			state_machine: StateMachine::Ethereum(Ethereum::ExecutionLayer),
			consensus_state_id: "SYNC".to_string(),
			ismp_host: Default::default(),
			handler: Default::default(),
			signer: "2e0834786285daccd064ca17f1654f67b4aef298acbb82cef9ec422fb4975622".to_string(),
			etherscan_api_keys: Default::default(),
		};

		let host = HostConfig { beacon_http_url: beacon_url, consensus_update_frequency: 180 };
		let arb_host = {
			let config = EvmConfig {
				execution_ws: arb_orl,
				state_machine: StateMachine::Ethereum(Ethereum::Arbitrum),
				consensus_state_id: "SYNC".to_string(),
				ismp_host: Default::default(),
				handler: Default::default(),
				signer: "2e0834786285daccd064ca17f1654f67b4aef298acbb82cef9ec422fb4975622"
					.to_string(),
				etherscan_api_keys: Default::default(),
			};

			ArbHost::new(
				&ArbHostConfig {
					beacon_execution_ws: geth_url.clone(),
					rollup_core: sp_core::H160::from(hex_literal::hex!(
						"45e5cAea8768F42B385A366D3551Ad1e0cbFAb17"
					)),
				},
				&config,
			)
			.await?
		};

		let op_host = {
			let config = EvmConfig {
				execution_ws: op_orl,
				state_machine: StateMachine::Ethereum(Ethereum::Optimism),
				consensus_state_id: "SYNC".to_string(),
				ismp_host: Default::default(),
				handler: Default::default(),
				signer: "2e0834786285daccd064ca17f1654f67b4aef298acbb82cef9ec422fb4975622"
					.to_string(),
				etherscan_api_keys: Default::default(),
			};

			OpHost::new(
				&OpHostConfig {
					beacon_execution_ws: geth_url.clone(),
					l2_oracle: sp_core::H160::from(hex_literal::hex!(
						"E6Dfba0953616Bacab0c9A8ecb3a9BBa77FC15c0"
					)),
					message_parser: sp_core::H160::from(hex_literal::hex!(
						"4200000000000000000000000000000000000016"
					)),
				},
				&config,
			)
			.await?
		};

		let base_host = {
			let config = EvmConfig {
				execution_ws: base_orl,
				state_machine: StateMachine::Ethereum(Ethereum::Optimism),
				consensus_state_id: "SYNC".to_string(),
				ismp_host: Default::default(),
				handler: Default::default(),
				signer: "2e0834786285daccd064ca17f1654f67b4aef298acbb82cef9ec422fb4975622"
					.to_string(),
				etherscan_api_keys: Default::default(),
			};

			OpHost::new(
				&OpHostConfig {
					beacon_execution_ws: geth_url,
					l2_oracle: sp_core::H160::from(hex_literal::hex!(
						"2A35891ff30313CcFa6CE88dcf3858bb075A2298"
					)),
					message_parser: sp_core::H160::from(hex_literal::hex!(
						"4200000000000000000000000000000000000016"
					)),
				},
				&config,
			)
			.await?
		};

		let mut host = SyncCommitteeHost::<Sepolia>::new(&host, &config).await?;
		host.set_arb_host(arb_host);
		host.set_op_host(op_host);
		host.set_base_host(base_host);
		EvmClient::new(Some(host), config).await?
	};

	let mut consensus_stream = chain_b.consensus_notification(chain_a.clone()).await.unwrap();

	while let Some(res) = consensus_stream.next().await {
		println!("Received new event");
		match res {
			Ok(res) => {
				let BeaconClientUpdate { mut op_stack_payload, consensus_update, arbitrum_payload } =
					BeaconClientUpdate::decode(&mut &res.consensus_proof[..]).unwrap();
				(*chain_a.consensus_state.lock().unwrap()).light_client_state.finalized_header =
					consensus_update.finalized_header;
				(*chain_a.consensus_state.lock().unwrap())
					.light_client_state
					.latest_finalized_epoch = consensus_update.finality_proof.epoch;
				(*chain_a.latest_height.lock().unwrap()) =
					consensus_update.execution_payload.block_number;
				dbg!(consensus_update.execution_payload.block_number);
				let state_root = consensus_update.execution_payload.state_root;

				let op_stack = [
					(
						StateMachine::Ethereum(Ethereum::Base),
						hex_literal::hex!("2A35891ff30313CcFa6CE88dcf3858bb075A2298"),
					),
					(
						StateMachine::Ethereum(Ethereum::Optimism),
						hex_literal::hex!("E6Dfba0953616Bacab0c9A8ecb3a9BBa77FC15c0"),
					),
				];

				for (state_machine_id, l2_oracle) in op_stack {
					println!("Verifying {state_machine_id:?} payload proof");
					if let Some(payload) = op_stack_payload.remove(&state_machine_id) {
						let _state = verify_optimism_payload::<Host>(
							payload,
							&state_root[..],
							l2_oracle.into(),
							Default::default(),
						)
						.unwrap();
					}
				}

				if let Some(arbitrum_payload) = arbitrum_payload {
					println!("Verifying arbitrum payload proof");
					let _state = verify_arbitrum_payload::<Host>(
						arbitrum_payload,
						&state_root[..],
						hex_literal::hex!("45e5cAea8768F42B385A366D3551Ad1e0cbFAb17").into(),
						Default::default(),
					)
					.unwrap();
				}

				println!("Finished payload proof verification");
			},
			Err(err) => {
				println!("Failed to fetch light client update {err:?}")
			},
		}
	}
	Ok(())
}
