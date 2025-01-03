use itertools::Itertools;
use near_async::test_loop::data::TestLoopData;
use near_async::time::Duration;
use near_chain_configs::test_genesis::{TestGenesisBuilder, ValidatorsSpec};
use near_chain_configs::DEFAULT_GC_NUM_EPOCHS_TO_KEEP;
use near_client::Query;
use near_o11y::testonly::init_test_logger;
use near_primitives::epoch_manager::EpochConfigStore;
use near_primitives::shard_layout::ShardLayout;
use near_primitives::types::{
    AccountId, BlockHeightDelta, BlockId, BlockReference, Gas, ShardId, ShardIndex,
};
use near_primitives::version::{ProtocolFeature, PROTOCOL_VERSION};
use rand::seq::SliceRandom;
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::test_loop::builder::TestLoopBuilder;
use crate::test_loop::env::{TestData, TestLoopEnv};
use crate::test_loop::utils::receipts::{
    check_receipts_presence_after_resharding_block, check_receipts_presence_at_resharding_block,
    ReceiptKind,
};
use crate::test_loop::utils::sharding::{
    next_block_has_new_shard_layout, print_and_assert_shard_accounts,
};
use crate::test_loop::utils::transactions::{
    check_txs, create_account, delete_account, deploy_contract, get_anchor_hash, get_next_nonce,
    get_smallest_height_head, store_and_submit_tx, submit_tx,
};
use crate::test_loop::utils::trie_sanity::{
    check_state_shard_uid_mapping_after_resharding, TrieSanityCheck,
};
use crate::test_loop::utils::{get_node_data, retrieve_client_actor, LoopActionFn, ONE_NEAR, TGAS};
use assert_matches::assert_matches;
use near_crypto::Signer;
use near_parameters::{vm, RuntimeConfig, RuntimeConfigStore};
use near_primitives::test_utils::create_user_test_signer;
use near_primitives::transaction::SignedTransaction;
use near_primitives::views::{FinalExecutionStatus, QueryRequest};

#[derive(derive_builder::Builder)]
#[builder(pattern = "owned", build_fn(skip))]
#[allow(unused)]
struct TestReshardingParameters {
    base_shard_layout_version: u64,
    /// Number of accounts.
    num_accounts: u64,
    /// Number of clients.
    num_clients: u64,
    /// Number of block and chunk producers.
    num_producers: u64,
    /// Number of chunk validators.
    num_validators: u64,
    /// Number of RPC clients.
    num_rpcs: u64,
    /// Number of archival clients.
    num_archivals: u64,
    #[builder(setter(skip))]
    accounts: Vec<AccountId>,
    #[builder(setter(skip))]
    clients: Vec<AccountId>,
    #[builder(setter(skip))]
    producers: Vec<AccountId>,
    #[builder(setter(skip))]
    validators: Vec<AccountId>,
    #[builder(setter(skip))]
    rpcs: Vec<AccountId>,
    #[builder(setter(skip))]
    rpc_client_index: Option<usize>,
    #[builder(setter(skip))]
    archivals: Vec<AccountId>,
    initial_balance: u128,
    epoch_length: BlockHeightDelta,
    chunk_ranges_to_drop: HashMap<ShardIndex, std::ops::Range<i64>>,
    shuffle_shard_assignment_for_chunk_producers: bool,
    track_all_shards: bool,
    load_mem_tries_for_tracked_shards: bool,
    /// Custom behavior executed at every iteration of test loop.
    #[builder(setter(custom))]
    loop_actions: Vec<LoopActionFn>,
    // When enabling shard shuffling with a short epoch length, sometimes a node might not finish
    // catching up by the end of the epoch, and then misses a chunk. This can be fixed by using a longer
    // epoch length, but it's good to also check what happens with shorter ones.
    all_chunks_expected: bool,
    /// Optionally deploy the test contract
    /// (see nearcore/runtime/near-test-contracts/test-contract-rs/src/lib.rs) on the provided accounts.
    #[builder(setter(custom))]
    deploy_test_contract: Vec<AccountId>,
    /// Enable a stricter limit on outgoing gas to easily trigger congestion control.
    limit_outgoing_gas: bool,
    /// If non zero, split parent shard for flat state resharding will be delayed by an additional
    /// `BlockHeightDelta` number of blocks. Useful to simulate slower task completion.
    delay_flat_state_resharding: BlockHeightDelta,
    /// Make promise yield timeout much shorter than normal.
    short_yield_timeout: bool,
    // TODO(resharding) Remove this when negative refcounts are properly handled.
    /// Whether to allow negative refcount being a result of the database update.
    allow_negative_refcount: bool,
}

impl TestReshardingParametersBuilder {
    fn build(self) -> TestReshardingParameters {
        let epoch_length = self.epoch_length.unwrap_or(6);

        let num_accounts = self.num_accounts.unwrap_or(8);
        let num_clients = self.num_clients.unwrap_or(7);
        let num_producers = self.num_producers.unwrap_or(3);
        let num_validators = self.num_validators.unwrap_or(2);
        let num_rpcs = self.num_rpcs.unwrap_or(1);
        let num_archivals = self.num_archivals.unwrap_or(1);

        assert!(num_clients >= num_producers + num_validators + num_rpcs + num_archivals);

        // #12195 prevents number of BPs bigger than `epoch_length`.
        assert!(num_producers > 0 && num_producers <= epoch_length);

        let accounts = Self::compute_initial_accounts(num_accounts);

        // This piece of code creates `num_clients` from `accounts`. First client is at index 0 and
        // other clients are spaced in the accounts' space as evenly as possible.
        let clients_per_account = num_clients as f64 / accounts.len() as f64;
        let mut client_parts = 1.0 - clients_per_account;
        let clients: Vec<_> = accounts
            .iter()
            .filter(|_| {
                client_parts += clients_per_account;
                if client_parts >= 1.0 {
                    client_parts -= 1.0;
                    true
                } else {
                    false
                }
            })
            .cloned()
            .collect();

        // Split the clients into producers, validators, rpc and archivals node.
        let tmp = clients.clone();
        let (producers, tmp) = tmp.split_at(num_producers as usize);
        let producers = producers.to_vec();
        let (validators, tmp) = tmp.split_at(num_validators as usize);
        let validators = validators.to_vec();
        let (rpcs, tmp) = tmp.split_at(num_rpcs as usize);
        let rpcs = rpcs.to_vec();
        let rpc_client_index =
            rpcs.first().map(|_| num_producers as usize + num_validators as usize);
        let (archivals, _) = tmp.split_at(num_archivals as usize);
        let archivals = archivals.to_vec();

        println!("Clients setup:");
        println!("Producers: {producers:?}");
        println!("Validators: {validators:?}");
        println!("Rpcs: {rpcs:?}, first RPC node uses client at index: {rpc_client_index:?}");
        println!("Archivals: {archivals:?}");

        TestReshardingParameters {
            base_shard_layout_version: self.base_shard_layout_version.unwrap_or(2),
            num_accounts,
            num_clients,
            num_producers,
            num_validators,
            num_rpcs,
            num_archivals,
            accounts,
            clients,
            producers,
            validators,
            rpcs,
            rpc_client_index,
            archivals,
            initial_balance: self.initial_balance.unwrap_or(1_000_000 * ONE_NEAR),
            epoch_length,
            chunk_ranges_to_drop: self.chunk_ranges_to_drop.unwrap_or_default(),
            shuffle_shard_assignment_for_chunk_producers: self
                .shuffle_shard_assignment_for_chunk_producers
                .unwrap_or(false),
            track_all_shards: self.track_all_shards.unwrap_or(false),
            load_mem_tries_for_tracked_shards: self
                .load_mem_tries_for_tracked_shards
                .unwrap_or(true),
            loop_actions: self.loop_actions.unwrap_or_default(),
            all_chunks_expected: self.all_chunks_expected.unwrap_or(false),
            deploy_test_contract: self.deploy_test_contract.unwrap_or_default(),
            limit_outgoing_gas: self.limit_outgoing_gas.unwrap_or(false),
            delay_flat_state_resharding: self.delay_flat_state_resharding.unwrap_or(0),
            short_yield_timeout: self.short_yield_timeout.unwrap_or(false),
            allow_negative_refcount: self.allow_negative_refcount.unwrap_or(false),
        }
    }

    fn add_loop_action(mut self, loop_action: LoopActionFn) -> Self {
        self.loop_actions.get_or_insert_default().push(loop_action);
        self
    }

    fn deploy_test_contract(mut self, account_id: AccountId) -> Self {
        self.deploy_test_contract.get_or_insert_default().push(account_id);
        self
    }

    fn compute_initial_accounts(num_accounts: u64) -> Vec<AccountId> {
        (0..num_accounts)
            .map(|i| format!("account{}", i).parse().unwrap())
            .collect::<Vec<AccountId>>()
    }
}

// Returns a callable function that, when invoked inside a test loop iteration, can force the creation of a chain fork.
#[cfg(feature = "test_features")]
fn fork_before_resharding_block(double_signing: bool) -> LoopActionFn {
    use crate::test_loop::utils::retrieve_client_actor;
    use near_client::client_actor::AdvProduceBlockHeightSelection;

    let done = Cell::new(false);
    Box::new(
        move |node_datas: &[TestData],
              test_loop_data: &mut TestLoopData,
              client_account_id: AccountId| {
            // It must happen only for the first resharding block encountered.
            if done.get() {
                return;
            }
            let client_actor =
                retrieve_client_actor(node_datas, test_loop_data, &client_account_id);
            let tip = client_actor.client.chain.head().unwrap();

            // If there's a new shard layout force a chain fork.
            if next_block_has_new_shard_layout(client_actor.client.epoch_manager.as_ref(), &tip) {
                println!("creating chain fork at height {}", tip.height);
                let height_selection = if double_signing {
                    // In the double signing scenario we want a new block on top of prev block, with consecutive height.
                    AdvProduceBlockHeightSelection::NextHeightOnSelectedBlock {
                        base_block_height: tip.height - 1,
                    }
                } else {
                    // To avoid double signing skip already produced height.
                    AdvProduceBlockHeightSelection::SelectedHeightOnSelectedBlock {
                        produced_block_height: tip.height + 1,
                        base_block_height: tip.height - 1,
                    }
                };
                client_actor.adv_produce_blocks_on(3, true, height_selection);
                done.set(true);
            }
        },
    )
}

fn execute_money_transfers(account_ids: Vec<AccountId>) -> LoopActionFn {
    const NUM_TRANSFERS_PER_BLOCK: usize = 20;

    let latest_height = Cell::new(0);
    let seed = rand::thread_rng().gen::<u64>();
    println!("Random seed: {}", seed);

    Box::new(
        move |node_datas: &[TestData],
              test_loop_data: &mut TestLoopData,
              client_account_id: AccountId| {
            let client_actor =
                retrieve_client_actor(node_datas, test_loop_data, &client_account_id);
            let tip = client_actor.client.chain.head().unwrap();

            // Run this action only once at every block height.
            if latest_height.get() == tip.height {
                return;
            }
            latest_height.set(tip.height);

            let mut slice = [0u8; 32];
            slice[0..8].copy_from_slice(&seed.to_le_bytes());
            slice[8..16].copy_from_slice(&tip.height.to_le_bytes());
            let mut rng: ChaCha20Rng = SeedableRng::from_seed(slice);

            for _ in 0..NUM_TRANSFERS_PER_BLOCK {
                let sender = account_ids.choose(&mut rng).unwrap().clone();
                let receiver = account_ids.choose(&mut rng).unwrap().clone();

                let clients = node_datas
                    .iter()
                    .map(|test_data| {
                        &test_loop_data.get(&test_data.client_sender.actor_handle()).client
                    })
                    .collect_vec();

                let anchor_hash = get_anchor_hash(&clients);
                let nonce = get_next_nonce(&test_loop_data, &node_datas, &sender);
                let amount = ONE_NEAR * rng.gen_range(1..=10);
                let tx = SignedTransaction::send_money(
                    nonce,
                    sender.clone(),
                    receiver.clone(),
                    &create_user_test_signer(&sender).into(),
                    amount,
                    anchor_hash,
                );
                submit_tx(&node_datas, &client_account_id, tx);
            }
        },
    )
}

/// Returns a loop action that invokes a costly method from a contract
/// `CALLS_PER_BLOCK_HEIGHT` times per block height.
///
/// The account invoking the contract is taken in sequential order from `signed_ids`.
///
/// The account receiving the contract call is taken in sequential order from `receiver_ids`.
fn call_burn_gas_contract(
    signer_ids: Vec<AccountId>,
    receiver_ids: Vec<AccountId>,
    gas_burnt_per_call: Gas,
) -> LoopActionFn {
    // Must be less than epoch length, otherwise transactions won't be checked.
    const TX_CHECK_BLOCKS_AFTER_RESHARDING: u64 = 5;
    const CALLS_PER_BLOCK_HEIGHT: usize = 5;

    let resharding_height = Cell::new(None);
    let nonce = Cell::new(102);
    let txs = Cell::new(vec![]);
    let latest_height = Cell::new(0);

    Box::new(
        move |node_datas: &[TestData],
              test_loop_data: &mut TestLoopData,
              client_account_id: AccountId| {
            let client_actor =
                retrieve_client_actor(node_datas, test_loop_data, &client_account_id);
            let tip = client_actor.client.chain.head().unwrap();

            // Run this action only once at every block height.
            if latest_height.get() == tip.height {
                return;
            }
            latest_height.set(tip.height);

            // After resharding: wait some blocks and check that all txs have been executed correctly.
            if let Some(height) = resharding_height.get() {
                if tip.height > height + TX_CHECK_BLOCKS_AFTER_RESHARDING {
                    for (tx, tx_height) in txs.take() {
                        let tx_outcome =
                            client_actor.client.chain.get_partial_transaction_result(&tx);
                        let status = tx_outcome.as_ref().map(|o| o.status.clone());
                        let status = status.unwrap();
                        tracing::debug!(target: "test", ?tx_height, ?tx, ?status, "transaction status");
                        assert_matches!(status, FinalExecutionStatus::SuccessValue(_));
                    }
                }
            } else {
                if next_block_has_new_shard_layout(client_actor.client.epoch_manager.as_ref(), &tip)
                {
                    tracing::debug!(target: "test", height=tip.height, "resharding height set");
                    resharding_height.set(Some(tip.height));
                }
            }
            // Before resharding and one block after: call the test contract a few times per block.
            // The objective is to pile up receipts (e.g. delayed).
            if tip.height <= resharding_height.get().unwrap_or(1000) + 1 {
                for i in 0..CALLS_PER_BLOCK_HEIGHT {
                    // Note that if the number of signers and receivers is the
                    // same then the traffic will always flow the same way. It
                    // would be nice to randomize it a bit.
                    let signer_id = &signer_ids[i % signer_ids.len()];
                    let receiver_id = &receiver_ids[i % receiver_ids.len()];
                    let signer: Signer = create_user_test_signer(signer_id).into();
                    nonce.set(nonce.get() + 1);
                    let method_name = "burn_gas_raw".to_owned();
                    let burn_gas: u64 = gas_burnt_per_call;
                    let args = burn_gas.to_le_bytes().to_vec();
                    let tx = SignedTransaction::call(
                        nonce.get(),
                        signer_id.clone(),
                        receiver_id.clone(),
                        &signer,
                        1,
                        method_name,
                        args,
                        gas_burnt_per_call + 10 * TGAS,
                        tip.last_block_hash,
                    );
                    store_and_submit_tx(
                        &node_datas,
                        &client_account_id,
                        &txs,
                        &signer_id,
                        &receiver_id,
                        tip.height,
                        tx,
                    );
                }
            }
        },
    )
}

/// Sends a promise-yield transaction before resharding. Then, if `call_resume` is `true` also sends
/// a yield-resume transaction after resharding, otherwise it lets the promise-yield go into timeout.
///
/// Each `signer_id` sends transaction to the corresponding `receiver_id`.
///
/// A few blocks after resharding all transactions outcomes are checked for successful execution.
fn call_promise_yield(
    call_resume: bool,
    signer_ids: Vec<AccountId>,
    receiver_ids: Vec<AccountId>,
) -> LoopActionFn {
    let resharding_height: Cell<Option<u64>> = Cell::new(None);
    let txs = Cell::new(vec![]);
    let latest_height = Cell::new(0);
    let promise_txs_sent = Cell::new(false);
    let nonce = Cell::new(102);
    let yield_payload = vec![];

    Box::new(
        move |node_datas: &[TestData],
              test_loop_data: &mut TestLoopData,
              client_account_id: AccountId| {
            let client_actor =
                retrieve_client_actor(node_datas, test_loop_data, &client_account_id);
            let tip = client_actor.client.chain.head().unwrap();

            // Run this action only once at every block height.
            if latest_height.get() == tip.height {
                return;
            }
            latest_height.set(tip.height);

            // The operation to be done depends on the current block height in relation to the
            // resharding height.
            match (resharding_height.get(), latest_height.get()) {
                // Resharding happened in the previous block.
                // Maybe send the resume transaction.
                (Some(resharding), latest) if latest == resharding + 1 && call_resume => {
                    for (signer_id, receiver_id) in
                        signer_ids.clone().into_iter().zip(receiver_ids.clone().into_iter())
                    {
                        let signer: Signer = create_user_test_signer(&signer_id).into();
                        nonce.set(nonce.get() + 1);
                        let tx = SignedTransaction::call(
                            nonce.get(),
                            signer_id.clone(),
                            receiver_id.clone(),
                            &signer,
                            1,
                            "call_yield_resume_read_data_id_from_storage".to_string(),
                            yield_payload.clone(),
                            300 * TGAS,
                            tip.last_block_hash,
                        );
                        store_and_submit_tx(
                            &node_datas,
                            &client_account_id,
                            &txs,
                            &signer_id,
                            &receiver_id,
                            tip.height,
                            tx,
                        );
                    }
                }
                // Resharding happened a few blocks in the past.
                // Check transactions' outcomes.
                (Some(resharding), latest) if latest == resharding + 4 => {
                    let txs = txs.take();
                    assert_ne!(txs.len(), 0);
                    for (tx, tx_height) in txs {
                        let tx_outcome =
                            client_actor.client.chain.get_partial_transaction_result(&tx);
                        let status = tx_outcome.as_ref().map(|o| o.status.clone());
                        let status = status.unwrap();
                        tracing::debug!(target: "test", ?tx_height, ?tx, ?status, "transaction status");
                        assert_matches!(status, FinalExecutionStatus::SuccessValue(_));
                    }
                }
                (Some(_resharding), _latest) => {}
                // Resharding didn't happen in the past.
                (None, _) => {
                    // Check if resharding will happen in this block.
                    if next_block_has_new_shard_layout(
                        client_actor.client.epoch_manager.as_ref(),
                        &tip,
                    ) {
                        tracing::debug!(target: "test", height=tip.height, "resharding height set");
                        resharding_height.set(Some(tip.height));
                        return;
                    }
                    // Before resharding, send a set of promise transactions, just once.
                    if promise_txs_sent.get() {
                        return;
                    }
                    for (signer_id, receiver_id) in
                        signer_ids.clone().into_iter().zip(receiver_ids.clone().into_iter())
                    {
                        let signer: Signer = create_user_test_signer(&signer_id).into();
                        nonce.set(nonce.get() + 1);
                        let tx = SignedTransaction::call(
                            nonce.get(),
                            signer_id.clone(),
                            receiver_id.clone(),
                            &signer,
                            0,
                            "call_yield_create_return_promise".to_string(),
                            yield_payload.clone(),
                            300 * TGAS,
                            tip.last_block_hash,
                        );
                        store_and_submit_tx(
                            &node_datas,
                            &client_account_id,
                            &txs,
                            &signer_id,
                            &receiver_id,
                            tip.height,
                            tx,
                        );
                    }
                    promise_txs_sent.set(true);
                }
            }
        },
    )
}

fn get_base_shard_layout(version: u64) -> ShardLayout {
    let boundary_accounts = vec!["account1".parse().unwrap(), "account3".parse().unwrap()];
    match version {
        1 => {
            let shards_split_map = vec![vec![ShardId::new(0), ShardId::new(1), ShardId::new(2)]];
            #[allow(deprecated)]
            ShardLayout::v1(boundary_accounts, Some(shards_split_map), 3)
        }
        2 => {
            let shard_ids = vec![ShardId::new(5), ShardId::new(3), ShardId::new(6)];
            let shards_split_map = [(ShardId::new(0), shard_ids.clone())].into_iter().collect();
            let shards_split_map = Some(shards_split_map);
            ShardLayout::v2(boundary_accounts, shard_ids, shards_split_map)
        }
        _ => panic!("Unsupported shard layout version {}", version),
    }
}

// After resharding and gc-period, assert the deleted `account_id`
// is still accessible through archival node view client (if available),
// and it is not accessible through a regular, RPC node.
fn check_deleted_account_availability(
    env: &mut TestLoopEnv,
    archival_id: &Option<&AccountId>,
    rpc_id: &AccountId,
    account_id: AccountId,
    height: u64,
) {
    let rpc_node_data = get_node_data(&env.datas, &rpc_id);
    let rpc_view_client_handle = rpc_node_data.view_client_sender.actor_handle();

    let block_reference = BlockReference::BlockId(BlockId::Height(height));
    let request = QueryRequest::ViewAccount { account_id };
    let msg = Query::new(block_reference, request);

    let rpc_node_result = {
        let view_client = env.test_loop.data.get_mut(&rpc_view_client_handle);
        near_async::messaging::Handler::handle(view_client, msg.clone())
    };
    assert!(!rpc_node_result.is_ok());

    if let Some(archival_id) = archival_id {
        let archival_node_data = get_node_data(&env.datas, &archival_id);
        let archival_view_client_handle = archival_node_data.view_client_sender.actor_handle();
        let archival_node_result = {
            let view_client = env.test_loop.data.get_mut(&archival_view_client_handle);
            near_async::messaging::Handler::handle(view_client, msg)
        };
        assert!(archival_node_result.is_ok());
    }
}

/// Base setup to check sanity of Resharding V3.
fn test_resharding_v3_base(params: TestReshardingParameters) {
    if !ProtocolFeature::SimpleNightshadeV4.enabled(PROTOCOL_VERSION) {
        return;
    }

    init_test_logger();
    let mut builder = TestLoopBuilder::new();

    // Adjust the resharding configuration to make the tests faster.
    builder = builder.config_modifier(|config, _| {
        let mut resharding_config = config.resharding_config.get();
        resharding_config.batch_delay = Duration::milliseconds(1);
        config.resharding_config.update(resharding_config);
    });

    // Prepare shard split configuration.
    let base_epoch_config_store = EpochConfigStore::for_chain_id("mainnet", None).unwrap();
    let base_protocol_version = ProtocolFeature::SimpleNightshadeV4.protocol_version() - 1;
    let mut base_epoch_config =
        base_epoch_config_store.get_config(base_protocol_version).as_ref().clone();
    base_epoch_config.num_block_producer_seats = params.num_producers;
    base_epoch_config.num_chunk_producer_seats = params.num_producers;
    base_epoch_config.num_chunk_validator_seats = params.num_producers + params.num_validators;
    base_epoch_config.shuffle_shard_assignment_for_chunk_producers =
        params.shuffle_shard_assignment_for_chunk_producers;
    if !params.chunk_ranges_to_drop.is_empty() {
        base_epoch_config.block_producer_kickout_threshold = 0;
        base_epoch_config.chunk_producer_kickout_threshold = 0;
        base_epoch_config.chunk_validator_only_kickout_threshold = 0;
    }

    let base_shard_layout = get_base_shard_layout(params.base_shard_layout_version);
    base_epoch_config.shard_layout = base_shard_layout.clone();

    let new_boundary_account = "account6".parse().unwrap();
    let parent_shard_uid = base_shard_layout.account_id_to_shard_uid(&new_boundary_account);
    let mut epoch_config = base_epoch_config.clone();
    epoch_config.shard_layout =
        ShardLayout::derive_shard_layout(&base_shard_layout, new_boundary_account.clone());
    tracing::info!(target: "test", ?base_shard_layout, new_shard_layout=?epoch_config.shard_layout, "shard layout");

    let expected_num_shards = epoch_config.shard_layout.num_shards();
    let epoch_config_store = EpochConfigStore::test(BTreeMap::from_iter(vec![
        (base_protocol_version, Arc::new(base_epoch_config)),
        (base_protocol_version + 1, Arc::new(epoch_config)),
    ]));

    let genesis = TestGenesisBuilder::new()
        .genesis_time_from_clock(&builder.clock())
        .shard_layout(base_shard_layout)
        .protocol_version(base_protocol_version)
        .epoch_length(params.epoch_length)
        .validators_spec(ValidatorsSpec::desired_roles(
            &params.producers.iter().map(|account_id| account_id.as_str()).collect_vec(),
            &params.validators.iter().map(|account_id| account_id.as_str()).collect_vec(),
        ))
        .add_user_accounts_simple(&params.accounts, params.initial_balance)
        .build();

    if params.track_all_shards {
        builder = builder.track_all_shards();
    }

    if params.allow_negative_refcount {
        builder = builder.allow_negative_refcount();
    }

    if params.limit_outgoing_gas || params.short_yield_timeout {
        let mut runtime_config = RuntimeConfig::test();
        if params.limit_outgoing_gas {
            runtime_config.congestion_control_config.max_outgoing_gas = 100 * TGAS;
            runtime_config.congestion_control_config.min_outgoing_gas = 100 * TGAS;
        }
        if params.short_yield_timeout {
            let mut wasm_config = vm::Config::clone(&runtime_config.wasm_config);
            // Assuming the promise yield is sent at h=9 and resharding happens at h=13, let's set
            // the timeout to trigger at h=14.
            wasm_config.limit_config.yield_timeout_length_in_blocks = 5;
            runtime_config.wasm_config = Arc::new(wasm_config);
        }
        let runtime_config_store = RuntimeConfigStore::with_one_config(runtime_config);
        builder = builder.runtime_config_store(runtime_config_store);
    }

    let archival_id = params.archivals.iter().next();
    // Try to use an RPC client, if available. Otherwise fallback to the client with the lowest index.
    let client_index = params.rpc_client_index.unwrap_or(0);
    let client_account_id = params.rpcs.get(0).unwrap_or_else(|| &params.clients[0]).clone();

    let mut env = builder
        .genesis(genesis)
        .epoch_config_store(epoch_config_store)
        .clients(params.clients)
        .archival_clients(params.archivals.iter().cloned().collect())
        .load_mem_tries_for_tracked_shards(params.load_mem_tries_for_tracked_shards)
        .drop_protocol_upgrade_chunks(
            base_protocol_version + 1,
            params.chunk_ranges_to_drop.clone(),
        )
        .build();

    let mut test_setup_transactions = vec![];
    for contract_id in &params.deploy_test_contract {
        let deploy_contract_tx = deploy_contract(
            &mut env.test_loop,
            &env.datas,
            &client_account_id,
            contract_id,
            near_test_contracts::rs_contract().into(),
            1,
        );
        test_setup_transactions.push(deploy_contract_tx);
    }

    // Create an account that is:
    // 1) Subaccount of a future resharding boundary account.
    // 2) Temporary, because we will remove it after resharding.
    // The goal is to test removing some state and see if it is kept on archival node.
    // The secondary goal is to catch potential bugs due to the above two conditions making it a special case.
    let temporary_account =
        format!("{}.{}", new_boundary_account, new_boundary_account).parse().unwrap();
    let create_account_tx = create_account(
        &mut env,
        &client_account_id,
        &new_boundary_account,
        &temporary_account,
        10 * ONE_NEAR,
        2,
    );
    test_setup_transactions.push(create_account_tx);

    // Wait for the test setup transactions to settle and ensure they all succeeded.
    env.test_loop.run_for(Duration::seconds(2));
    check_txs(&env.test_loop, &env.datas, &client_account_id, &test_setup_transactions);

    let client_handles =
        env.datas.iter().map(|data| data.client_sender.actor_handle()).collect_vec();

    #[cfg(feature = "test_features")]
    {
        if params.delay_flat_state_resharding > 0 {
            client_handles.iter().for_each(|handle| {
                let client = &mut env.test_loop.data.get_mut(handle).client;
                client.chain.resharding_manager.flat_storage_resharder.adv_task_delay_by_blocks =
                    params.delay_flat_state_resharding;
            });
        }
    }

    let clients =
        client_handles.iter().map(|handle| &env.test_loop.data.get(handle).client).collect_vec();
    let mut trie_sanity_check =
        TrieSanityCheck::new(&clients, params.load_mem_tries_for_tracked_shards);

    let latest_block_height = std::cell::Cell::new(0u64);
    let success_condition = |test_loop_data: &mut TestLoopData| -> bool {
        params
            .loop_actions
            .iter()
            .for_each(|action| action(&env.datas, test_loop_data, client_account_id.clone()));

        let clients =
            client_handles.iter().map(|handle| &test_loop_data.get(handle).client).collect_vec();
        let client = clients[client_index];

        let tip = get_smallest_height_head(&clients);

        // Check that all chunks are included.
        let block_header = client.chain.get_block_header(&tip.last_block_hash).unwrap();
        if latest_block_height.get() < tip.height {
            if latest_block_height.get() == 0 {
                println!("State before resharding:");
                print_and_assert_shard_accounts(&clients, &tip);
            }
            trie_sanity_check.assert_state_sanity(&clients, expected_num_shards);
            latest_block_height.set(tip.height);
            if params.all_chunks_expected && params.chunk_ranges_to_drop.is_empty() {
                assert!(block_header.chunk_mask().iter().all(|chunk_bit| *chunk_bit));
            }
        }

        // Return true if we passed an epoch with increased number of shards.
        let epoch_height =
            client.epoch_manager.get_epoch_height_from_prev_block(&tip.prev_block_hash).unwrap();
        assert!(epoch_height < 6);
        let prev_epoch_id =
            client.epoch_manager.get_prev_epoch_id_from_prev_block(&tip.prev_block_hash).unwrap();
        let epoch_config = client.epoch_manager.get_epoch_config(&prev_epoch_id).unwrap();
        if epoch_config.shard_layout.num_shards() != expected_num_shards {
            return false;
        }

        println!("State after resharding:");
        print_and_assert_shard_accounts(&clients, &tip);
        check_state_shard_uid_mapping_after_resharding(&client, parent_shard_uid);
        return true;
    };

    env.test_loop.run_until(
        success_condition,
        // Give enough time to produce ~7 epochs.
        Duration::seconds((7 * params.epoch_length) as i64),
    );
    let client = &env.test_loop.data.get(&client_handles[client_index]).client;
    trie_sanity_check.check_epochs(client);
    let height_after_resharding = latest_block_height.get();

    // Delete `temporary_account`.
    delete_account(&mut env, &client_account_id, &temporary_account, &client_account_id);
    // Wait for garbage collection to kick in.
    env.test_loop
        .run_for(Duration::seconds((DEFAULT_GC_NUM_EPOCHS_TO_KEEP * params.epoch_length) as i64));
    // Check that the deleted account is still accessible at archival node, but not at a regular node.
    check_deleted_account_availability(
        &mut env,
        &archival_id,
        &client_account_id,
        temporary_account,
        height_after_resharding,
    );

    env.shutdown_and_drain_remaining_events(Duration::seconds(20));
}

#[test]
fn test_resharding_v3() {
    test_resharding_v3_base(TestReshardingParametersBuilder::default().build());
}

#[test]
fn test_resharding_v3_track_all_shards() {
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .track_all_shards(true)
            .all_chunks_expected(true)
            .build(),
    );
}

#[test]
fn test_resharding_v3_drop_chunks_before() {
    let chunk_ranges_to_drop = HashMap::from([(1, -2..0)]);
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .chunk_ranges_to_drop(chunk_ranges_to_drop)
            .build(),
    );
}

#[test]
fn test_resharding_v3_drop_chunks_after() {
    let chunk_ranges_to_drop = HashMap::from([(2, 0..2)]);
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .chunk_ranges_to_drop(chunk_ranges_to_drop)
            .build(),
    );
}

#[test]
fn test_resharding_v3_drop_chunks_before_and_after() {
    let chunk_ranges_to_drop = HashMap::from([(0, -2..2)]);
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .chunk_ranges_to_drop(chunk_ranges_to_drop)
            .build(),
    );
}

#[test]
fn test_resharding_v3_drop_chunks_all() {
    let chunk_ranges_to_drop = HashMap::from([(0, -1..2), (1, -3..0), (2, 0..3), (3, 0..1)]);
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .chunk_ranges_to_drop(chunk_ranges_to_drop)
            .build(),
    );
}

#[test]
// TODO(resharding): fix nearcore and un-ignore this test
#[ignore]
#[cfg(feature = "test_features")]
fn test_resharding_v3_resharding_block_in_fork() {
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .num_clients(1)
            .num_producers(1)
            .num_validators(0)
            .num_rpcs(0)
            .num_archivals(0)
            .add_loop_action(fork_before_resharding_block(false))
            .build(),
    );
}

#[test]
// TODO(resharding): fix nearcore and un-ignore this test
// TODO(resharding): duplicate this test so that in one case resharding is performed on block
//                   B(height=13) and in another case resharding is performed on block B'(height=13)
#[ignore]
#[cfg(feature = "test_features")]
fn test_resharding_v3_double_sign_resharding_block() {
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .num_clients(1)
            .num_producers(1)
            .num_validators(0)
            .num_rpcs(0)
            .num_archivals(0)
            .add_loop_action(fork_before_resharding_block(true))
            .build(),
    );
}

#[test]
fn test_resharding_v3_shard_shuffling() {
    let params = TestReshardingParametersBuilder::default()
        .shuffle_shard_assignment_for_chunk_producers(true)
        .build();
    test_resharding_v3_base(params);
}

#[test]
fn test_resharding_v3_shard_shuffling_intense() {
    let chunk_ranges_to_drop = HashMap::from([(0, -1..2), (1, -3..0), (2, -3..3), (3, 0..1)]);
    let params = TestReshardingParametersBuilder::default()
        .num_accounts(8)
        .epoch_length(8)
        .shuffle_shard_assignment_for_chunk_producers(true)
        .chunk_ranges_to_drop(chunk_ranges_to_drop)
        .add_loop_action(execute_money_transfers(
            TestReshardingParametersBuilder::compute_initial_accounts(8),
        ))
        .build();
    test_resharding_v3_base(params);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_delayed_receipts_left_child() {
    let account: AccountId = "account4".parse().unwrap();
    let params = TestReshardingParametersBuilder::default()
        .deploy_test_contract(account.clone())
        .add_loop_action(call_burn_gas_contract(
            vec![account.clone()],
            vec![account.clone()],
            275 * TGAS,
        ))
        .add_loop_action(check_receipts_presence_at_resharding_block(
            vec![account],
            ReceiptKind::Delayed,
        ))
        .allow_negative_refcount(true)
        .build();
    test_resharding_v3_base(params);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_delayed_receipts_right_child() {
    let account: AccountId = "account6".parse().unwrap();
    let params = TestReshardingParametersBuilder::default()
        .deploy_test_contract(account.clone())
        .add_loop_action(call_burn_gas_contract(
            vec![account.clone()],
            vec![account.clone()],
            275 * TGAS,
        ))
        .add_loop_action(check_receipts_presence_at_resharding_block(
            vec![account],
            ReceiptKind::Delayed,
        ))
        .allow_negative_refcount(true)
        // TODO(resharding): test should work without changes to track_all_shards
        .track_all_shards(true)
        .build();
    test_resharding_v3_base(params);
}

fn test_resharding_v3_split_parent_buffered_receipts_base(base_shard_layout_version: u64) {
    let receiver_account: AccountId = "account0".parse().unwrap();
    let account_in_parent: AccountId = "account4".parse().unwrap();
    let account_in_left_child: AccountId = "account4".parse().unwrap();
    let account_in_right_child: AccountId = "account6".parse().unwrap();
    let params = TestReshardingParametersBuilder::default()
        .base_shard_layout_version(base_shard_layout_version)
        .deploy_test_contract(receiver_account.clone())
        .limit_outgoing_gas(true)
        .add_loop_action(call_burn_gas_contract(
            vec![account_in_left_child.clone(), account_in_right_child],
            vec![receiver_account],
            10 * TGAS,
        ))
        .add_loop_action(check_receipts_presence_at_resharding_block(
            vec![account_in_parent],
            ReceiptKind::Buffered,
        ))
        .add_loop_action(check_receipts_presence_after_resharding_block(
            vec![account_in_left_child],
            ReceiptKind::Buffered,
        ))
        // TODO(resharding): test should work without changes to track_all_shards
        .track_all_shards(true)
        .build();
    test_resharding_v3_base(params);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_split_parent_buffered_receipts_v1() {
    test_resharding_v3_split_parent_buffered_receipts_base(1);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_split_parent_buffered_receipts_v2() {
    test_resharding_v3_split_parent_buffered_receipts_base(2);
}

fn test_resharding_v3_buffered_receipts_towards_splitted_shard_base(
    base_shard_layout_version: u64,
) {
    let account_in_left_child: AccountId = "account4".parse().unwrap();
    let account_in_right_child: AccountId = "account6".parse().unwrap();
    let account_in_stable_shard: AccountId = "account1".parse().unwrap();

    let params = TestReshardingParametersBuilder::default()
        .base_shard_layout_version(base_shard_layout_version)
        .deploy_test_contract(account_in_left_child.clone())
        .deploy_test_contract(account_in_right_child.clone())
        .limit_outgoing_gas(true)
        .add_loop_action(call_burn_gas_contract(
            vec![account_in_stable_shard.clone()],
            vec![account_in_left_child, account_in_right_child],
            10 * TGAS,
        ))
        .add_loop_action(check_receipts_presence_at_resharding_block(
            vec![account_in_stable_shard.clone()],
            ReceiptKind::Buffered,
        ))
        .add_loop_action(check_receipts_presence_after_resharding_block(
            vec![account_in_stable_shard],
            ReceiptKind::Buffered,
        ))
        .build();
    test_resharding_v3_base(params);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_buffered_receipts_towards_splitted_shard_v1() {
    test_resharding_v3_buffered_receipts_towards_splitted_shard_base(1);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_buffered_receipts_towards_splitted_shard_v2() {
    test_resharding_v3_buffered_receipts_towards_splitted_shard_base(2);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_outgoing_receipts_towards_splitted_shard() {
    let receiver_account: AccountId = "account4".parse().unwrap();
    let account_1_in_stable_shard: AccountId = "account1".parse().unwrap();
    let account_2_in_stable_shard: AccountId = "account2".parse().unwrap();
    let params = TestReshardingParametersBuilder::default()
        .deploy_test_contract(receiver_account.clone())
        .add_loop_action(call_burn_gas_contract(
            vec![account_1_in_stable_shard, account_2_in_stable_shard],
            vec![receiver_account],
            5 * TGAS,
        ))
        .build();
    test_resharding_v3_base(params);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_outgoing_receipts_from_splitted_shard() {
    let receiver_account: AccountId = "account0".parse().unwrap();
    let account_in_left_child: AccountId = "account4".parse().unwrap();
    let account_in_right_child: AccountId = "account6".parse().unwrap();
    let params = TestReshardingParametersBuilder::default()
        .deploy_test_contract(receiver_account.clone())
        .add_loop_action(call_burn_gas_contract(
            vec![account_in_left_child, account_in_right_child],
            vec![receiver_account],
            5 * TGAS,
        ))
        // TODO(resharding): test should work without changes to track_all_shards
        .track_all_shards(true)
        .build();
    test_resharding_v3_base(params);
}

#[test]
fn test_resharding_v3_load_mem_trie_v1() {
    let params = TestReshardingParametersBuilder::default()
        .base_shard_layout_version(1)
        .load_mem_tries_for_tracked_shards(false)
        // TODO(resharding): should it work without tracking all shards?
        .track_all_shards(true)
        .build();
    test_resharding_v3_base(params);
}

#[test]
fn test_resharding_v3_load_mem_trie_v2() {
    let params = TestReshardingParametersBuilder::default()
        .base_shard_layout_version(2)
        .load_mem_tries_for_tracked_shards(false)
        // TODO(resharding): should it work without tracking all shards?
        .track_all_shards(true)
        .build();
    test_resharding_v3_base(params);
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_slower_post_processing_tasks() {
    // When there's a resharding task delay and single-shard tracking, the delay might be pushed out
    // even further because the resharding task might have to wait for the state snapshot to be made
    // before it can proceed, which might mean that flat storage won't be ready for the child shard for a whole epoch.
    // So we extend the epoch length a bit in this case.
    test_resharding_v3_base(
        TestReshardingParametersBuilder::default()
            .delay_flat_state_resharding(2)
            .epoch_length(13)
            .build(),
    );
}

#[test]
#[cfg_attr(not(feature = "test_features"), ignore)]
fn test_resharding_v3_shard_shuffling_slower_post_processing_tasks() {
    let params = TestReshardingParametersBuilder::default()
        .shuffle_shard_assignment_for_chunk_producers(true)
        .delay_flat_state_resharding(2)
        .epoch_length(13)
        .build();
    test_resharding_v3_base(params);
}

#[test]
fn test_resharding_v3_yield_resume() {
    let account_in_left_child: AccountId = "account4".parse().unwrap();
    let account_in_right_child: AccountId = "account6".parse().unwrap();
    let params = TestReshardingParametersBuilder::default()
        .deploy_test_contract(account_in_left_child.clone())
        .deploy_test_contract(account_in_right_child.clone())
        .add_loop_action(call_promise_yield(
            true,
            vec![account_in_left_child.clone(), account_in_right_child.clone()],
            vec![account_in_left_child.clone(), account_in_right_child.clone()],
        ))
        .add_loop_action(check_receipts_presence_at_resharding_block(
            vec![account_in_left_child.clone(), account_in_right_child.clone()],
            ReceiptKind::PromiseYield,
        ))
        .add_loop_action(check_receipts_presence_after_resharding_block(
            vec![account_in_left_child, account_in_right_child],
            ReceiptKind::PromiseYield,
        ))
        // TODO(resharding): test should work without changes to track_all_shards
        .track_all_shards(true)
        .build();
    test_resharding_v3_base(params);
}

#[test]
fn test_resharding_v3_yield_timeout() {
    let account_in_left_child: AccountId = "account4".parse().unwrap();
    let account_in_right_child: AccountId = "account6".parse().unwrap();
    let params = TestReshardingParametersBuilder::default()
        .deploy_test_contract(account_in_left_child.clone())
        .deploy_test_contract(account_in_right_child.clone())
        .short_yield_timeout(true)
        .add_loop_action(call_promise_yield(
            false,
            vec![account_in_left_child.clone(), account_in_right_child.clone()],
            vec![account_in_left_child.clone(), account_in_right_child.clone()],
        ))
        .add_loop_action(check_receipts_presence_at_resharding_block(
            vec![account_in_left_child.clone(), account_in_right_child.clone()],
            ReceiptKind::PromiseYield,
        ))
        .add_loop_action(check_receipts_presence_after_resharding_block(
            vec![account_in_left_child, account_in_right_child],
            ReceiptKind::PromiseYield,
        ))
        .allow_negative_refcount(true)
        .build();
    test_resharding_v3_base(params);
}
