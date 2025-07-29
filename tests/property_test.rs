mod common;
use bitcoin::Amount;
use electrsd::corepc_node::Client as BitcoindClient;
use electrsd::corepc_node::Node as BitcoinD;
use electrsd::ElectrsD;
use electrum_client::ElectrumApi;
use ldk_node::payment::{PaymentDirection, PaymentKind, PaymentStatus};
use ldk_node::{
	Builder, CustomTlvRecord, Event, LightningBalance, Node, NodeError, PendingSweepBalance,
};
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use proptest::prelude::*;
use std::sync::Once;
use std::time::Duration;

use crate::common::{
	distribute_funds, expect_channel_pending_event, expect_channel_ready_event, expect_event,
	generate_blocks_and_wait, invalidate_blocks, premine_blocks, random_config,
	setup_bitcoind_and_electrsd_and_esplora, setup_node, setup_two_nodes, validate_txid_amount,
	wait_for_outpoint_spend, wait_for_tx, TestChainSource,
};

static INIT: Once = Once::new();
static mut TEST_ENV: Option<(BitcoinD, ElectrsD, ElectrsD)> = None;

fn setup_test_env() -> &'static (BitcoinD, ElectrsD, ElectrsD) {
	INIT.call_once(|| {
		let (bitcoind, electrsd, esplora) = setup_bitcoind_and_electrsd_and_esplora();
		let (bitcoind_client, electrsd_client) = (&bitcoind.client, &electrsd.client);

		// Initial pre-mining
		premine_blocks(bitcoind_client, electrsd_client);

		unsafe {
			TEST_ENV = Some((bitcoind, electrsd, esplora));
		}
	});
	unsafe { TEST_ENV.as_ref().unwrap() }
}

fn reorg<E: ElectrumApi>(
	bitcoind_client: &BitcoindClient, electrsd_client: &E, reorg_depth: usize,
) {
	let blockchain_info1 = bitcoind_client.get_blockchain_info().unwrap();

	// Invalidate blocks to simulate reorg
	invalidate_blocks(bitcoind_client, reorg_depth);
	// Generate new blocks to restore the chain
	generate_blocks_and_wait(bitcoind_client, electrsd_client, reorg_depth);

	let blockchain_info2 = bitcoind_client.get_blockchain_info().unwrap();
	assert_eq!(
		blockchain_info2.blocks, blockchain_info1.blocks,
		"Blockchain height should be restored after reorg"
	);

	println!(
		"Reorg completed: {} blocks invalidated, {} blocks generated",
		reorg_depth, reorg_depth
	);
}

proptest! {
	#![proptest_config(ProptestConfig::with_cases(1))]
	#[test]
	fn prop_handle_reorgs(
		reorg_depth in 1..=6usize,
		allow_0conf in prop::bool::ANY,
		anchor_channels in prop::bool::ANY,
		anchors_trusted_no_reserve in prop::bool::ANY,
		force_close in prop::bool::ANY,
		reorg_point in 0..=3u8 // 0: before channel, 1: after opening, 2: after payments, 3: after closing
	) {
		println!(
			"Reorg depth: {}, Allow 0-conf: {}, Anchor channels: {}, Anchors trusted no reserve: {}, Force close: {}, Reorg point: {}",
			reorg_depth, allow_0conf, anchor_channels, anchors_trusted_no_reserve, force_close, reorg_point
		);

		let (bitcoind, electrsd, esplora) = setup_test_env();
		let chain_source_bitcoind = TestChainSource::BitcoindRpcSync(&bitcoind);
		let chain_source_electrsd = TestChainSource::Electrum(&electrsd);
		let chain_source_esplora = TestChainSource::Esplora(&esplora);

		let (bitcoind_client, electrsd_client) = (&bitcoind.client, &electrsd.client);

		let (node_hub, node_bitcoind) = setup_two_nodes(
			&chain_source_bitcoind,
			allow_0conf,
			anchor_channels,
			anchors_trusted_no_reserve,
		);

		let mut config_generic = random_config(anchor_channels);
		if allow_0conf {
			config_generic.node_config.trusted_peers_0conf.push(node_bitcoind.node_id());
		}
		if anchor_channels && anchors_trusted_no_reserve {
			config_generic
				.node_config
				.anchor_channels_config
				.as_mut()
				.unwrap()
				.trusted_peers_no_reserve
				.push(node_bitcoind.node_id());
		}
		let node_electrsd = setup_node(&chain_source_electrsd, config_generic, None);

		let mut config_generic = random_config(anchor_channels);
		if allow_0conf {
			config_generic.node_config.trusted_peers_0conf.push(node_bitcoind.node_id());
		}
		if anchor_channels && anchors_trusted_no_reserve {
			config_generic
				.node_config
				.anchor_channels_config
				.as_mut()
				.unwrap()
				.trusted_peers_no_reserve
				.push(node_hub.node_id());
		}
		let node_esplora = setup_node(&chain_source_esplora, config_generic, None);

		macro_rules! println_balances_wallets {
			() => {
				println!("\nNode hub balances: {:?}", node_hub.list_balances());
				println!("\nNode bitcoind balances: {:?}", node_bitcoind.list_balances());
				println!("\nNode electrsd balances: {:?}", node_electrsd.list_balances());
				println!("\nNode esplora balances: {:?}", node_esplora.list_balances());
			};
		}

		macro_rules! sync_wallets {
			() => {
				node_hub.sync_wallets().unwrap();
				node_bitcoind.sync_wallets().unwrap();
				node_electrsd.sync_wallets().unwrap();
				node_esplora.sync_wallets().unwrap();
			};
		}


		let addr_hub = node_hub.onchain_payment().new_address().unwrap();
		let addr_bitcoind = node_bitcoind.onchain_payment().new_address().unwrap();
		let addr_electrsd = node_electrsd.onchain_payment().new_address().unwrap();
		let addr_esplora = node_esplora.onchain_payment().new_address().unwrap();

		let premine_amount_sat = if anchor_channels { 2_125_000 } else { 2_100_000 };

		// Distribute funds
		let result = distribute_funds(
			bitcoind_client,
			electrsd_client,
			vec![addr_hub.clone(), addr_bitcoind.clone(), addr_electrsd.clone(), addr_esplora.clone()],
			Amount::from_sat(premine_amount_sat),
		);

		macro_rules! valid_node_amount {
			($node:ident) => {
				assert_eq!(
					$node.list_balances().spendable_onchain_balance_sats,
					premine_amount_sat,
				);
				assert_eq!(
					$node
						.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
							&& matches!(p.kind, PaymentKind::Onchain { .. }))
						.len(),
					1
				);
				assert_eq!($node.next_event(), None);
			};
		}
		macro_rules! valid_node_amount_all {
			() => {
				println!("Validating node hub");
				valid_node_amount!(node_hub);
				println!("Validating node bitcoind");
				valid_node_amount!(node_bitcoind);
				println!("Validating node electrsd");
				valid_node_amount!(node_electrsd);
				println!("Validating node esplora");
				valid_node_amount!(node_esplora);
			};
		}

		sync_wallets!();
		valid_node_amount_all!();

		// // Reorg before channel opening
		if reorg_point == 0  || true {
			// reorg(bitcoind_client, electrsd_client, 1);
			reorg(bitcoind_client, electrsd_client, 6);
			sync_wallets!();
			let mut is_sync_wallets = false;

			macro_rules! check_balance_node {
				($node:ident, $addr:ident) => {
					loop {
						let list_balances = $node.list_balances();
						// If there is a balance and it is not ready to spend, it means that the transaction has not been confirmed.
						if list_balances.spendable_onchain_balance_sats != premine_amount_sat &&  list_balances.total_onchain_balance_sats == premine_amount_sat {
							generate_blocks_and_wait(bitcoind_client, electrsd_client, 1);
							is_sync_wallets = true;
						// If there is no balance left, it means the transaction has been removed from the block and mempool.
						}else if list_balances.total_onchain_balance_sats == 0 {
							distribute_funds(
								bitcoind_client,
								electrsd_client,
								vec![$addr.clone()],
								Amount::from_sat(premine_amount_sat),
							);
							is_sync_wallets = true;
							break
						} else { break };
						$node.sync_wallets().unwrap();
					}
				};
			}
			println!("\nChecking balance of node_hub");
			check_balance_node!(node_hub, addr_hub);
			println!("\nChecking balance of node_bitcoind");
			check_balance_node!(node_bitcoind, addr_bitcoind);
			println!("\nChecking balance of node_electrsd");
			check_balance_node!(node_electrsd, addr_electrsd);
			println!("\nChecking balance of node_esplora");
			check_balance_node!(node_esplora, addr_esplora);

			if is_sync_wallets {
				sync_wallets!();
			}
			println_balances_wallets!();
			valid_node_amount_all!();
		}

		// Open channel between node_hub and node_bitcoind
		let funding_amount_sat = 2_080_000;
		let push_msat = (funding_amount_sat / 2) * 1000;
		node_hub
			.open_announced_channel(
				node_bitcoind.node_id(),
				node_bitcoind.listening_addresses().unwrap().first().unwrap().clone(),
				funding_amount_sat,
				Some(push_msat),
				None,
			)
			.unwrap();

		assert_eq!(
			node_hub.list_peers().first().unwrap().node_id,
			node_bitcoind.node_id(),
			"Node hub should have node_bitcoind as peer"
		);
		assert!(
			node_hub.list_peers().first().unwrap().is_persisted,
			"Node hub peer should be persisted"
		);
		let funding_txo = expect_channel_pending_event!(node_hub, node_bitcoind.node_id());
		let funding_txo_2 = expect_channel_pending_event!(node_bitcoind, node_hub.node_id());
		assert_eq!(
			funding_txo.txid, funding_txo_2.txid,
			"Funding transaction IDs should match"
		);
		println!("\nFunding transaction: {:?}", funding_txo);
		validate_txid_amount(
			bitcoind_client,
			electrsd_client,
			funding_txo.txid,
		);

		sync_wallets!();
		println_balances_wallets!();

		wait_for_tx(electrsd_client, funding_txo.txid);
		println!(
			"\nFunding transaction {} confirmed",
			funding_txo.txid
		);

		if !allow_0conf {
			generate_blocks_and_wait(bitcoind_client, electrsd_client, 6);
		}

		node_hub.sync_wallets().unwrap();
		node_bitcoind.sync_wallets().unwrap();
		println!("\npassed here 2");

		let user_channel_id = expect_channel_ready_event!(node_hub, node_bitcoind.node_id());
		expect_channel_ready_event!(node_bitcoind, node_hub.node_id());
		if reorg_point == 1 {
			reorg(bitcoind_client, electrsd_client, reorg_depth);
		}

		println!("\npassed here 2");

		// Verify balances and payments after channel opening
		let node_a_anchor_reserve_sat = if anchor_channels { 25_000 } else { 0 };
		let node_b_anchor_reserve_sat = if anchor_channels && anchors_trusted_no_reserve {
			0
		} else {
			25_000
		};
		let onchain_fee_buffer_sat = 5_000;
		let node_hub_upper_bound_sat =
			premine_amount_sat - node_a_anchor_reserve_sat - funding_amount_sat;
		let node_hub_lower_bound_sat = node_hub_upper_bound_sat - onchain_fee_buffer_sat;
		// assert!(
		// 	node_hub.list_balances().spendable_onchain_balance_sats < node_hub_upper_bound_sat,
		// 	"Node hub balance too high after channel opening"
		// );
		// assert!(
		// 	node_hub.list_balances().spendable_onchain_balance_sats > node_hub_lower_bound_sat,
		// 	"Node hub balance too low after channel opening"
		// );
		// assert_eq!(
		// 	node_hub.list_balances().total_anchor_channels_reserve_sats,
		// 	node_a_anchor_reserve_sat,
		// 	"Node hub anchor reserve incorrect"
		// );
		// assert!(
		// 	node_bitcoind.list_balances().spendable_onchain_balance_sats
		// 		<= premine_amount_sat - node_b_anchor_reserve_sat,
		// 	"Node bitcoind balance too high after channel opening"
		// );
		// assert!(
		// 	node_bitcoind.list_balances().spendable_onchain_balance_sats
		// 		> premine_amount_sat - node_b_anchor_reserve_sat - onchain_fee_buffer_sat,
		// 	"Node bitcoind balance too low after channel opening"
		// );
		// assert_eq!(
		// 	node_bitcoind.list_balances().total_anchor_channels_reserve_sats,
		// 	node_b_anchor_reserve_sat,
		// 	"Node bitcoind anchor reserve incorrect"
		// );
		// assert_eq!(
		// 	node_hub
		// 		.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound
		// 			&& matches!(p.kind, PaymentKind::Onchain { .. }))
		// 		.len(),
		// 	1,
		// 	"Node hub should have one outbound on-chain payment for funding"
		// );

		// Perform payments (Bolt11, under/overpayment, zero-amount, manual, keysend)
		let invoice_description =
			Bolt11InvoiceDescription::Direct(Description::new("test".to_string()).unwrap());

		// Bolt11 payment
		println!("\nBolt11 payment");
		let invoice_amount_1_msat = 2_500_000;
		let invoice = node_bitcoind
			.bolt11_payment()
			.receive(invoice_amount_1_msat, &invoice_description.clone().into(), 9217)
			.unwrap();
		let payment_id_1 = node_bitcoind
			.bolt11_payment()
			.receive(invoice_amount_1_msat, &invoice_description.clone().into(), 9217)
			.unwrap();
		let payment_id = node_hub.bolt11_payment().send(&invoice, None).unwrap();
		assert_eq!(
			node_hub.bolt11_payment().send(&invoice, None),
			Err(NodeError::DuplicatePayment),
			"Duplicate payment should fail"
		);
		expect_event!(node_hub, PaymentSuccessful);
		expect_event!(node_bitcoind, PaymentReceived);
		assert_eq!(
			node_hub.payment(&payment_id).unwrap().status,
			PaymentStatus::Succeeded,
			"Bolt11 payment should be successful"
		);
		assert_eq!(
			node_bitcoind.payment(&payment_id).unwrap().status,
			PaymentStatus::Succeeded,
			"Bolt11 payment should be successful"
		);
		assert_eq!(
			node_hub.payment(&payment_id).unwrap().amount_msat,
			Some(invoice_amount_1_msat),
			"Bolt11 payment amount incorrect"
		);
		assert_eq!(
			node_bitcoind.payment(&payment_id).unwrap().amount_msat,
			Some(invoice_amount_1_msat),
			"Bolt11 payment amount incorrect"
		);
		assert!(
			matches!(node_hub.payment(&payment_id).unwrap().kind, PaymentKind::Bolt11 { .. }),
			"Bolt11 payment kind incorrect"
		);

		println!("\npassed here 3");

		// Reorg after payments
		if reorg_point == 2 {
			reorg(bitcoind_client, electrsd_client, reorg_depth);

			sync_wallets!();

			// Verify that the channel remains active
			assert!(
				!node_hub.list_channels().is_empty(),
				"Channel should remain open after reorg"
			);
			assert!(
				!node_bitcoind.list_channels().is_empty(),
				"Channel should remain open after reorg"
			);
		}

		// Close channel
		let channel_closed = if !node_hub.list_channels().is_empty() {
			println!("\nClosing channel (force: {})", force_close);
			if force_close {
				std::thread::sleep(Duration::from_secs(1));
				node_hub
					.force_close_channel(&user_channel_id, node_bitcoind.node_id(), None)
					.unwrap();
			} else {
				node_hub
					.close_channel(&user_channel_id, node_bitcoind.node_id())
					.unwrap();
			}
			expect_event!(node_hub, ChannelClosed);
			expect_event!(node_bitcoind, ChannelClosed);

			wait_for_outpoint_spend(electrsd_client, funding_txo);

			generate_blocks_and_wait(bitcoind_client, electrsd_client, 1);
			node_hub.sync_wallets().unwrap();
			node_bitcoind.sync_wallets().unwrap();
			true
		} else {
			false
		};

		// Reorg after channel closing
		if reorg_point == 3 && channel_closed {
			let blockchain_info1 = bitcoind_client.get_blockchain_info().unwrap();
			reorg(bitcoind_client, electrsd_client, reorg_depth);
			let blockchain_info2 = bitcoind_client.get_blockchain_info().unwrap();
			assert_eq!(
				blockchain_info2.blocks,
				blockchain_info1.blocks,
				"Blockchain height should be restored after reorg"
			);
			sync_wallets!();

			// Verify that the channel remains closed
			assert!(
				node_hub.list_channels().is_empty(),
				"Channel should remain closed after reorg"
			);
			assert!(
				node_bitcoind.list_channels().is_empty(),
				"Channel should remain closed after reorg"
			);

			// Verify that payments remain valid
			assert_eq!(
				node_hub.payment(&payment_id).unwrap().status,
				PaymentStatus::Succeeded,
				"Bolt11 payment should remain successful after reorg"
			);
		}

		// Verify balances after closing
		if force_close && channel_closed {
			// Verify pending balances from forced closing
			assert_eq!(
				node_hub.list_balances().lightning_balances.len(),
				1,
				"Expected one lightning balance for node_hub"
			);
			assert_eq!(
				node_bitcoind.list_balances().lightning_balances.len(),
				1,
				"Expected one lightning balance for node_bitcoind"
			);

			// Process node_hub balances
			match node_hub.list_balances().lightning_balances[0] {
				LightningBalance::ClaimableAwaitingConfirmations {
					counterparty_node_id,
					confirmation_height,
					..
				} => {
					assert_eq!(
						counterparty_node_id,
						node_bitcoind.node_id(),
						"Node hub counterparty incorrect"
					);
					let cur_height = node_hub.status().current_best_block.height;
					let blocks_to_go = confirmation_height - cur_height;
					generate_blocks_and_wait(bitcoind_client, electrsd_client, blocks_to_go as usize);
					node_hub.sync_wallets().unwrap();
					node_bitcoind.sync_wallets().unwrap();
				},
				_ => panic!("Unexpected balance state for node_hub!"),
			}

			assert!(
				node_hub.list_balances().lightning_balances.is_empty(),
				"Node hub lightning balances should be empty after confirmations"
			);
			assert_eq!(
				node_hub.list_balances().pending_balances_from_channel_closures.len(),
				1,
				"Expected one pending balance for node_hub"
			);
			match node_hub.list_balances().pending_balances_from_channel_closures[0] {
				PendingSweepBalance::BroadcastAwaitingConfirmation { .. } => {},
				_ => panic!("Unexpected pending balance state for node_hub!"),
			}
			generate_blocks_and_wait(bitcoind_client, electrsd_client, 1);
			node_hub.sync_wallets().unwrap();
			node_bitcoind.sync_wallets().unwrap();

			assert!(
				node_hub.list_balances().lightning_balances.is_empty(),
				"Node hub lightning balances should remain empty"
			);
			assert_eq!(
				node_hub.list_balances().pending_balances_from_channel_closures.len(),
				1,
				"Expected one pending balance for node_hub"
			);
			match node_hub.list_balances().pending_balances_from_channel_closures[0] {
				PendingSweepBalance::AwaitingThresholdConfirmations { .. } => {},
				_ => panic!("Unexpected pending balance state for node_hub!"),
			}
			generate_blocks_and_wait(bitcoind_client, electrsd_client, 5);
			node_hub.sync_wallets().unwrap();
			node_bitcoind.sync_wallets().unwrap();

			// Process node_bitcoind balances
			match node_bitcoind.list_balances().lightning_balances[0] {
				LightningBalance::ClaimableAwaitingConfirmations {
					counterparty_node_id,
					confirmation_height,
					..
				} => {
					assert_eq!(
						counterparty_node_id,
						node_hub.node_id(),
						"Node bitcoind counterparty incorrect"
					);
					let cur_height = node_bitcoind.status().current_best_block.height;
					let blocks_to_go = confirmation_height - cur_height;
					generate_blocks_and_wait(bitcoind_client, electrsd_client, blocks_to_go as usize);
					node_hub.sync_wallets().unwrap();
					node_bitcoind.sync_wallets().unwrap();
				},
				_ => panic!("Unexpected balance state for node_bitcoind!"),
			}

			assert!(
				node_bitcoind.list_balances().lightning_balances.is_empty(),
				"Node bitcoind lightning balances should be empty after confirmations"
			);
			assert_eq!(
				node_bitcoind.list_balances().pending_balances_from_channel_closures.len(),
				1,
				"Expected one pending balance for node_bitcoind"
			);
			match node_bitcoind.list_balances().pending_balances_from_channel_closures[0] {
				PendingSweepBalance::BroadcastAwaitingConfirmation { .. } => {},
				_ => panic!("Unexpected pending balance state for node_bitcoind!"),
			}
			generate_blocks_and_wait(bitcoind_client, electrsd_client, 1);
			node_hub.sync_wallets().unwrap();
			node_bitcoind.sync_wallets().unwrap();

			assert!(
				node_bitcoind.list_balances().lightning_balances.is_empty(),
				"Node bitcoind lightning balances should remain empty"
			);
			assert_eq!(
				node_bitcoind.list_balances().pending_balances_from_channel_closures.len(),
				1,
				"Expected one pending balance for node_bitcoind"
			);
			match node_bitcoind.list_balances().pending_balances_from_channel_closures[0] {
				PendingSweepBalance::AwaitingThresholdConfirmations { .. } => {},
				_ => panic!("Unexpected pending balance state for node_bitcoind!"),
			}
			generate_blocks_and_wait(bitcoind_client, electrsd_client, 5);
			node_hub.sync_wallets().unwrap();
			node_bitcoind.sync_wallets().unwrap();
		}

		// Verify final balances
		if channel_closed {
			let sum_of_all_payments_sat = (push_msat
				+ invoice_amount_1_msat)
				/ 1000;
			let node_hub_upper_bound_sat =
				(premine_amount_sat - funding_amount_sat) + (funding_amount_sat - sum_of_all_payments_sat);
			let node_hub_lower_bound_sat = node_hub_upper_bound_sat - onchain_fee_buffer_sat;
			let node_bitcoind_upper_bound_sat = premine_amount_sat + sum_of_all_payments_sat;
			let node_bitcoind_lower_bound_sat = node_bitcoind_upper_bound_sat - onchain_fee_buffer_sat;

			// assert!(
			// 	node_hub.list_balances().spendable_onchain_balance_sats > node_hub_lower_bound_sat,
			// 	"Node hub balance too low after closing"
			// );
			// assert!(
			// 	node_hub.list_balances().spendable_onchain_balance_sats < node_hub_upper_bound_sat,
			// 	"Node hub balance too high after closing"
			// );
			// assert!(
			// 	node_bitcoind.list_balances().spendable_onchain_balance_sats > node_bitcoind_lower_bound_sat,
			// 	"Node bitcoind balance too low after closing"
			// );
			// assert!(
			// 	node_bitcoind.list_balances().spendable_onchain_balance_sats <= node_bitcoind_upper_bound_sat,
			// 	"Node bitcoind balance too high after closing"
			// );
			// assert_eq!(
			// 	node_hub.list_balances().total_anchor_channels_reserve_sats,
			// 	0,
			// 	"Node hub anchor reserve should be zero after closing"
			// );
			// assert_eq!(
			// 	node_bitcoind.list_balances().total_anchor_channels_reserve_sats,
			// 	0,
			// 	"Node bitcoind anchor reserve should be zero after closing"
			// );

			// // Verify on-chain payments after closure
			// assert_eq!(
			// 	node_hub
			// 		.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
			// 			&& matches!(p.kind, PaymentKind::Onchain { .. }))
			// 		.len(),
			// 	2,
			// 	"Node hub should have two inbound on-chain payments after closure"
			// );
			// assert_eq!(
			// 	node_bitcoind
			// 		.list_payments_with_filter(|p| p.direction == PaymentDirection::Inbound
			// 			&& matches!(p.kind, PaymentKind::Onchain { .. }))
			// 		.len(),
			// 	2,
			// 	"Node bitcoind should have two inbound on-chain payments after closure"
			// );
		}

		// Check all events handled
		assert_eq!(node_hub.next_event(), None, "Node hub should have no pending events");
		assert_eq!(node_bitcoind.next_event(), None, "Node bitcoind should have no pending events");

		node_hub.stop().unwrap();
		println!("\nNode hub stopped");
		node_bitcoind.stop().unwrap();
		println!("\nNode bitcoind stopped");
		node_electrsd.stop().unwrap();
		node_esplora.stop().unwrap();
	}
}
