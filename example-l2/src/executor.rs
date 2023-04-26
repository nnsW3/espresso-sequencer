use crate::prover::BatchProof;
use async_std::sync::RwLock;
use async_std::task::sleep;
use commit::Committable;
use contract_bindings::{
    example_rollup::{self, ExampleRollup},
    hot_shot::NewBlocksFilter,
    HotShot,
};
use ethers::prelude::*;
use hotshot_query_service::availability::BlockQueryData;
use std::sync::Arc;

use sequencer::{
    hotshot_commitment::{connect_l1, HotShotContractOptions},
    SeqTypes,
};
use sequencer_utils::{commitment_to_u256, contract_send, u256_to_commitment};

use crate::state::State;

type HotShotClient = surf_disco::Client<hotshot_query_service::Error>;

/// Runs the executor service, which is responsible for:
/// 1) Fetching blocks of ordered transactions from HotShot and applying them to the Rollup State.
/// 2) Submitting mock proofs to the Rollup Contract.
pub async fn run_executor(
    rollup_address: Address,
    opt: &HotShotContractOptions,
    state: Arc<RwLock<State>>,
) {
    let query_service_url = opt.query_service_url.join("availability").unwrap();
    let hotshot = HotShotClient::new(query_service_url.clone());
    hotshot.connect(None).await;

    // Connect to the layer one HotShot contract.
    let Some(l1) = connect_l1(opt)
    .await else {
        // TODO: Switch these over to panics
        tracing::error!("unable to connect to L1, hotshot commitment task exiting");
        return;
    };

    // Create a socket connection to the L1 to subscribe to contract events
    // This assumes that the L1 node supports both HTTP and Websocket connections
    let mut ws_url = opt.l1_provider.clone();
    ws_url.set_scheme("ws").unwrap();
    let socket_provider = match Provider::<Ws>::connect(ws_url).await {
        Ok(socket_provider) => socket_provider,
        Err(err) => {
            tracing::error!("Unable to make websocket connection to L1: {}", err);
            tracing::error!("Executor task will exit");
            return;
        }
    };

    let rollup_contract = ExampleRollup::new(rollup_address, l1.clone());
    let hotshot_contract = HotShot::new(opt.hotshot_address, Arc::new(socket_provider));
    let filter = hotshot_contract.new_blocks_filter();
    let mut stream = match filter.subscribe().await {
        Ok(stream) => stream,
        Err(err) => {
            tracing::error!("Unable to subscribe to L1 log stream: {}", err);
            tracing::error!("Executor task will exit");
            return;
        }
    };

    while let Some(event) = stream.next().await {
        let (first_block, num_blocks) = match event {
            Ok(NewBlocksFilter {
                first_block_number,
                num_blocks,
            }) => (first_block_number, num_blocks.as_u64()),
            Err(err) => {
                tracing::error!("Error in HotShot block stream, retrying: {err}");
                continue;
            }
        };

        // Execute new blocks, generating proofs.
        let mut proofs = vec![];
        let mut state = state.write().await;
        for i in 0..num_blocks {
            let commitment = match hotshot_contract.commitments(first_block + i).call().await {
                // TODO: Replace these with typed errors
                Ok(commitment) => commitment,
                Err(err) => {
                    tracing::error!("Unable to read commitment from contract: {}", err);
                    tracing::error!("Executor task will exit");
                    return;
                }
            };
            let block_commitment = match u256_to_commitment(commitment) {
                Ok(commitment) => commitment,
                Err(err) => {
                    tracing::error!("Unable to deserialize commitment: {}", err);
                    tracing::error!("Executor task will exit");
                    return;
                }
            };

            let block = match hotshot
                .get::<BlockQueryData<SeqTypes>>(&format!("block/{}", first_block + i))
                .send()
                .await
            {
                Ok(block) => block,
                Err(err) => {
                    tracing::error!("Unable to query block from hotshot client: {}", err);
                    tracing::error!("Executor task will exit");
                    return;
                }
            };

            if block.block().commit() != block_commitment {
                tracing::error!("Block commitment does not match hash of recieved block, the executor cannot continue");
                return;
            }

            proofs.push(state.execute_block(&block).await);
        }

        // Compute an aggregate proof.
        let proof = BatchProof::generate(&proofs);
        let state_comm = commitment_to_u256(state.commit());

        // Send the batch proof to L1.
        tracing::info!(
            "Sending batch proof of blocks {}-{} to L1: {:?}",
            first_block,
            first_block + num_blocks - 1,
            proof,
        );
        let proof = example_rollup::BatchProof::from(proof);
        while contract_send(rollup_contract.verify_blocks(num_blocks, state_comm, proof.clone()))
            .await
            .is_none()
        {
            tracing::warn!("Failed to submit proof to contract, retrying");
            sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

#[cfg(test)]
mod test {
    use crate::transaction::{SignedTransaction, Transaction};
    use crate::RollupVM;

    use super::*;
    use async_std::task::spawn;
    use contract_bindings::example_rollup::ExampleRollup;
    use contract_bindings::TestClients;
    use ethers::providers::{Middleware, Provider};
    use ethers::signers::{LocalWallet, Signer};
    use futures::future::ready;
    use futures::FutureExt;
    use hotshot_query_service::data_source::QueryData;
    use portpicker::pick_unused_port;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use sequencer::api::SequencerNode;
    use sequencer::hotshot_commitment::run_hotshot_commitment_task;
    use sequencer::testing::{init_hotshot_handles, wait_for_decide_on_handle};
    use sequencer::transaction::SequencerTransaction;
    use sequencer::Vm;
    use sequencer_utils::{commitment_to_u256, AnvilOptions};
    use std::path::Path;
    use std::time::Duration;
    use surf_disco::{Client, Url};
    use tempfile::TempDir;
    use tide_disco::error::ServerError;

    const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[async_std::test]
    async fn test_execute() {
        // Create mock rollup state
        let alice = LocalWallet::new(&mut ChaChaRng::seed_from_u64(0));
        let bob = LocalWallet::new(&mut ChaChaRng::seed_from_u64(1));
        let state = Arc::new(RwLock::new(State::from_initial_balances([(
            alice.address(),
            9999,
        )])));
        let initial_state = { state.read().await.commit() };

        // Start a test HotShot and Rollup contract
        let anvil = AnvilOptions::default().spawn().await;
        let mut provider = Provider::try_from(&anvil.url().to_string()).unwrap();
        provider.set_interval(Duration::from_millis(10));
        let chain_id = provider.get_chainid().await.unwrap().as_u64();
        let clients = TestClients::new(&provider, chain_id);
        let hotshot_contract = HotShot::deploy(clients.deployer.provider.clone(), ())
            .unwrap()
            .send()
            .await
            .unwrap();
        let rollup_contract = ExampleRollup::deploy(
            clients.deployer.provider.clone(),
            (
                hotshot_contract.address(),
                commitment_to_u256(initial_state),
            ),
        )
        .unwrap()
        .send()
        .await
        .unwrap();

        // Setup a WS connection to the rollup contract and subscribe to state updates
        let mut ws_url = anvil.url();
        ws_url.set_scheme("ws").unwrap();
        let socket_provider = Provider::<Ws>::connect(ws_url).await.unwrap();
        let state_update_filter = rollup_contract.state_update_filter().filter;
        let mut stream = socket_provider
            .subscribe_logs(&state_update_filter)
            .await
            .unwrap();

        // Start a test HotShot configuration
        let sequencer_port = pick_unused_port().unwrap();
        let nodes = init_hotshot_handles().await;
        let api_node = nodes[0].clone();
        let tmp_dir = TempDir::new().unwrap();
        let storage_path: &Path = &(tmp_dir.path().join("tmp_storage"));
        let init_handle = Box::new(move |_| (ready((api_node, 0)).boxed()));
        let query_data = QueryData::create(storage_path, ()).unwrap();
        let SequencerNode { .. } = sequencer::api::serve(query_data, init_handle, sequencer_port)
            .await
            .unwrap();
        for node in &nodes {
            node.start().await;
        }
        let sequencer_url: Url = format!("http://localhost:{sequencer_port}")
            .parse()
            .unwrap();

        // Submit transaction to sequencer
        let txn = Transaction {
            amount: 100,
            destination: bob.address(),
            nonce: 1,
        };
        let txn = SignedTransaction::new(txn, &alice).await;
        let txn = RollupVM.wrap(&txn);
        let client: Client<ServerError> = Client::new(sequencer_url.clone());
        client.connect(None).await;
        client
            .post::<()>("submit/submit")
            .body_json(&txn)
            .unwrap()
            .send()
            .await
            .unwrap();

        // Spawn hotshot commitment and executor tasks
        let hotshot_opt = HotShotContractOptions {
            l1_provider: anvil.url(),
            sequencer_mnemonic: TEST_MNEMONIC.to_string(),
            sequencer_account_index: clients.funded[0].index,
            hotshot_address: hotshot_contract.address(),
            l1_chain_id: None,
            query_service_url: sequencer_url,
        };
        let options = HotShotContractOptions {
            sequencer_account_index: clients.funded[1].index,
            ..hotshot_opt.clone()
        };
        let state_lock = state.clone();
        let rollup_address = rollup_contract.address();
        spawn(async move { run_hotshot_commitment_task(&hotshot_opt).await });
        spawn(async move { run_executor(rollup_address, &options, state_lock).await });

        // Wait for the rollup contract to process all state updates
        loop {
            // Wait for an event. This stream should not end until our events have been processed.
            stream.next().await.unwrap();
            let bob_balance = state.read().await.get_balance(&bob.address());
            if bob_balance == 100 {
                break;
            } else {
                tracing::info!("Bob's balance is {bob_balance}/100");
            }
        }

        // Ensure that the state commitments match.
        let state_comm = state.read().await.commit();
        let state_comm = commitment_to_u256(state_comm);
        let contract_state_comm = rollup_contract.state_commitment().call().await.unwrap();
        assert_eq!(state_comm, contract_state_comm);
    }

    #[async_std::test]
    async fn test_execute_batched_updates_to_slow_l1() {
        let num_txns = 10;

        // Create mock rollup state
        let alice = LocalWallet::new(&mut ChaChaRng::seed_from_u64(0));
        let bob = LocalWallet::new(&mut ChaChaRng::seed_from_u64(1));
        let state = Arc::new(RwLock::new(State::from_initial_balances([(
            alice.address(),
            9999,
        )])));
        let initial_state = { state.read().await.commit() };

        // Start a test HotShot and Rollup contract.
        let anvil = AnvilOptions::default()
            .block_time(Duration::from_secs(30))
            .spawn()
            .await;
        let mut provider = Provider::try_from(&anvil.url().to_string()).unwrap();
        provider.set_interval(Duration::from_millis(10));
        let chain_id = provider.get_chainid().await.unwrap().as_u64();
        let clients = TestClients::new(&provider, chain_id);
        let hotshot_contract = HotShot::deploy(clients.deployer.provider.clone(), ())
            .unwrap()
            .send()
            .await
            .unwrap();
        let rollup_contract = ExampleRollup::deploy(
            clients.deployer.provider.clone(),
            (
                hotshot_contract.address(),
                commitment_to_u256(initial_state),
            ),
        )
        .unwrap()
        .send()
        .await
        .unwrap();

        // Setup a WS connection to the rollup contract and subscribe to state updates
        let mut ws_url = anvil.url();
        ws_url.set_scheme("ws").unwrap();
        let socket_provider = Provider::<Ws>::connect(ws_url).await.unwrap();
        let state_update_filter = rollup_contract.state_update_filter().filter;
        let mut stream = socket_provider
            .subscribe_logs(&state_update_filter)
            .await
            .unwrap();

        // Start a test HotShot configuration
        let sequencer_port = pick_unused_port().unwrap();
        let nodes = init_hotshot_handles().await;
        let api_node = nodes[0].clone();
        let tmp_dir = TempDir::new().unwrap();
        let storage_path: &Path = &(tmp_dir.path().join("tmp_storage"));
        let init_handle = Box::new(move |_| (ready((api_node, 0)).boxed()));
        let query_data = QueryData::create(storage_path, ()).unwrap();
        let SequencerNode { .. } = sequencer::api::serve(query_data, init_handle, sequencer_port)
            .await
            .unwrap();
        for node in &nodes {
            node.start().await;
        }
        let sequencer_url: Url = format!("http://localhost:{sequencer_port}")
            .parse()
            .unwrap();

        // Spawn hotshot commitment and executor tasks
        let hotshot_opt = HotShotContractOptions {
            l1_provider: anvil.url(),
            sequencer_mnemonic: TEST_MNEMONIC.to_string(),
            sequencer_account_index: clients.funded[0].index,
            hotshot_address: hotshot_contract.address(),
            l1_chain_id: None,
            query_service_url: sequencer_url,
        };
        let options = HotShotContractOptions {
            sequencer_account_index: clients.funded[1].index,
            ..hotshot_opt.clone()
        };
        let state_lock = state.clone();
        let rollup_address = rollup_contract.address();
        spawn(async move { run_hotshot_commitment_task(&hotshot_opt).await });
        spawn(async move { run_executor(rollup_address, &options, state_lock).await });

        // Submit transactions to sequencer
        for nonce in 1..=num_txns {
            let txn = Transaction {
                amount: 1,
                destination: bob.address(),
                nonce,
            };
            let txn = SignedTransaction::new(txn, &alice).await;
            let txn = SequencerTransaction::from(RollupVM.wrap(&txn));
            nodes[0].submit_transaction(txn.clone()).await.unwrap();

            // Wait for the transaction to be sequenced, before we can sequence the next one.
            tracing::info!("Waiting for txn {nonce} to be sequenced");
            wait_for_decide_on_handle(nodes[0].clone(), txn)
                .await
                .unwrap();
        }

        // Wait for the rollup contract to process all state updates
        loop {
            // Wait for an event. This stream should not end until our events have been processed.
            stream.next().await.unwrap();
            let bob_balance = state.read().await.get_balance(&bob.address());
            if bob_balance == num_txns {
                break;
            } else {
                tracing::info!("Bob's balance is {bob_balance}/{num_txns}");
            }
        }

        // Ensure the state commitments match.
        let state_comm = state.read().await.commit();
        let state_comm = commitment_to_u256(state_comm);
        let contract_state_comm = rollup_contract.state_commitment().call().await.unwrap();
        assert_eq!(state_comm, contract_state_comm);
    }
}
