use ::irys_database::{tables::IrysTables, BlockIndex, Initialized};
use actix::{Actor, System, SystemRegistry};
use actix::{Arbiter, SystemService};
use alloy_eips::BlockNumberOrTag;
use irys_actors::reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor};
use irys_actors::{
    block_discovery::BlockDiscoveryActor,
    block_index_service::{BlockIndexReadGuard, BlockIndexService, GetBlockIndexGuardMessage},
    block_producer::BlockProducerActor,
    block_tree_service::{BlockTreeService, GetBlockTreeGuardMessage},
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    chunk_migration_service::ChunkMigrationService,
    epoch_service::{
        EpochServiceActor, EpochServiceConfig, GetGenesisStorageModulesMessage,
        GetLedgersGuardMessage, GetPartitionAssignmentsGuardMessage,
    },
    mempool_service::MempoolService,
    mining::PartitionMiningActor,
    packing::{PackingActor, PackingRequest},
    validation_service::ValidationService,
    vdf_service::{GetVdfStateMessage, VdfService, VdfStepsReadGuard},
    ActorAddresses, BlockFinalizedMessage,
};
use irys_api_server::{run_server, ApiState};
use irys_config::{decode_hex, IrysNodeConfig, STORAGE_SUBMODULES_CONFIG};
use irys_database::database;
use irys_packing::{PackingType, PACKING_TYPE};
use irys_reth_node_bridge::adapter::node::RethNodeContext;
pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};

use irys_storage::{
    initialize_storage_files,
    reth_provider::{IrysRethProvider, IrysRethProviderInner},
    ChunkProvider, ChunkType, StorageModule, StorageModuleVec,
};
use irys_types::{
    app_state::DatabaseProvider, calculate_initial_difficulty, irys::IrysSigner,
    vdf_config::VDFStepsConfig, StorageConfig, CHUNK_SIZE, CONFIG, H256,
};
use reth::rpc::eth::EthApiServer as _;
use reth::{
    builder::FullNode,
    chainspec::ChainSpec,
    core::irys_ext::NodeExitReason,
    tasks::{TaskExecutor, TaskManager},
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c};
use reth_db::{Database as _, HasName, HasTableType};
use std::sync::atomic::AtomicU64;
use std::{
    sync::{mpsc, Arc, OnceLock, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, error, info};

use tokio::{
    runtime::Handle,
    sync::oneshot::{self},
};

use crate::vdf::run_vdf;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;

pub async fn start() -> eyre::Result<IrysNodeCtx> {
    let config: IrysNodeConfig = IrysNodeConfig {
        mining_signer: IrysSigner::mainnet_from_slice(&decode_hex(CONFIG.mining_key).unwrap()),
        ..IrysNodeConfig::default()
    };

    let storage_config = StorageConfig {
        chunk_size: CONFIG.chunk_size,
        num_chunks_in_partition: CONFIG.num_chunks_in_partition,
        num_chunks_in_recall_range: CONFIG.num_chunks_in_recall_range,
        num_partitions_in_slot: CONFIG.num_partitions_per_slot,
        miner_address: config.mining_signer.address(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: CONFIG.entropy_packing_iterations,
        chunk_migration_depth: CONFIG.chunk_migration_depth, // Testnet / single node config
    };

    start_irys_node(config, storage_config).await
}

pub async fn start_for_testing(config: IrysNodeConfig) -> eyre::Result<IrysNodeCtx> {
    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: config.mining_signer.address(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: 1_000,
        chunk_migration_depth: 1, // Testnet / single node config
    };

    start_irys_node(config, storage_config).await
}

pub async fn start_for_testing_default(
    name: Option<&str>,
    keep: bool,
    miner_signer: IrysSigner,
    storage_config: StorageConfig,
) -> eyre::Result<IrysNodeCtx> {
    let config = IrysNodeConfig {
        base_directory: setup_tracing_and_temp_dir(name, keep).into_path(),
        mining_signer: miner_signer.clone(),
        ..Default::default()
    };

    let storage_config = StorageConfig {
        miner_address: miner_signer.address(), // just in case to keep the same miner address
        chunk_migration_depth: 1,              // Testnet / single node config
        ..storage_config
    };

    start_irys_node(config, storage_config).await
}

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    pub reth_handle: RethNodeProvider,
    pub actor_addresses: ActorAddresses,
    pub db: DatabaseProvider,
    pub config: Arc<IrysNodeConfig>,
    pub chunk_provider: Arc<ChunkProvider>,
    pub block_index_guard: BlockIndexReadGuard,
    pub vdf_steps_guard: VdfStepsReadGuard,
    pub vdf_config: VDFStepsConfig,
    pub storage_config: StorageConfig,
}

pub async fn start_irys_node(
    node_config: IrysNodeConfig,
    storage_config: StorageConfig,
) -> eyre::Result<IrysNodeCtx> {
    info!("Using directory {:?}", &node_config.base_directory);

    if PACKING_TYPE != PackingType::CPU && storage_config.chunk_size != CHUNK_SIZE {
        error!("GPU packing only supports chunk size {}!", CHUNK_SIZE)
    }

    let (reth_handle_sender, reth_handle_receiver) =
        oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();
    let (irys_node_handle_sender, irys_node_handle_receiver) = oneshot::channel::<IrysNodeCtx>();
    let (reth_chainspec, mut irys_genesis) = node_config.chainspec_builder.build();
    let arc_config = Arc::new(node_config);
    let mut difficulty_adjustment_config = CONFIG.clone().into();

    // TODO: Hard coding 3 for storage module count isn't great here,
    // eventually we'll want to relate this to the genesis config
    irys_genesis.diff =
        calculate_initial_difficulty(&difficulty_adjustment_config, &storage_config, 3).unwrap();

    difficulty_adjustment_config.target_block_time = 5;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    irys_genesis.timestamp = now.as_millis();
    irys_genesis.last_diff_timestamp = irys_genesis.timestamp;
    let arc_genesis = Arc::new(irys_genesis);

    let mut storage_modules: StorageModuleVec = Vec::new();

    let at_genesis;
    let latest_block_index;
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new({
        let mut idx = BlockIndex::default();
        if !CONFIG.persist_data_on_restart {
            debug!("Resetting block index");
            idx = idx.reset(&arc_config.clone())?
        } else {
            debug!("Not resetting block index");
        }
        let i = idx.init(arc_config.clone()).await.unwrap();

        at_genesis = i.get_item(0).is_none();
        if at_genesis {
            debug!("At genesis!")
        } else {
            debug!("Not at genesis!")
        }
        latest_block_index = i.get_latest_item().cloned();

        i
    }));

    let cloned_arc = arc_config.clone();

    // Spawn thread and runtime for actors
    let arc_config_copy = arc_config.clone();
    let irys_provider: IrysRethProvider = Arc::new(OnceLock::new());

    // clone as this gets `move`d into the thread
    let irys_provider_1 = irys_provider.clone();

    std::thread::Builder::new()
        .name("actor-main-thread".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let node_config = arc_config_copy.clone();
            System::new().block_on(async move {
                // the RethNodeHandle doesn't *need* to be Arc, but it will reduce the copy cost
                let reth_node = RethNodeProvider(Arc::new(reth_handle_receiver.await.unwrap()));
                let db = DatabaseProvider(reth_node.provider.database.db.clone());
                let vdf_config = VDFStepsConfig::default();

                let latest_block = latest_block_index
                    .map(|b| {
                        database::block_header_by_hash(&db.tx().unwrap(), &b.block_hash)
                            .unwrap()
                            .unwrap()
                    })
                    .map(Arc::new)
                    .unwrap_or(arc_genesis.clone());

                // Initialize the epoch_service actor to handle partition ledger assignments
                let config = EpochServiceConfig {
                    storage_config: storage_config.clone(),
                    ..EpochServiceConfig::default()
                };

                let miner_address = node_config.mining_signer.address();
                debug!("Miner address {:?}", miner_address);

                let reth_service = RethServiceActor::new(reth_node.clone(), db.clone());
                let reth_arbiter = Arbiter::new();
                SystemRegistry::set(RethServiceActor::start_in_arbiter(
                    &reth_arbiter.handle(),
                    |_| reth_service,
                ));

                debug!(
                    "JESSEDEBUG setting head to block {} ({})",
                    &latest_block.evm_block_hash, &latest_block.height
                );

                {
                    let context = RethNodeContext::new(reth_node.clone().into())
                        .await
                        .map_err(|e| eyre::eyre!("Error connecting to Reth: {}", e))
                        .unwrap();

                    let latest = context
                        .rpc
                        .inner
                        .eth_api()
                        .block_by_number(BlockNumberOrTag::Latest, false)
                        .await;

                    let safe = context
                        .rpc
                        .inner
                        .eth_api()
                        .block_by_number(BlockNumberOrTag::Safe, false)
                        .await;

                    let finalized = context
                        .rpc
                        .inner
                        .eth_api()
                        .block_by_number(BlockNumberOrTag::Finalized, false)
                        .await;

                    debug!(
                        "JESSEDEBUG FCU S latest {:?}, safe {:?}, finalized {:?}",
                        &latest, &safe, &finalized
                    );

                    assert_eq!(
                        latest.unwrap().unwrap().header.number,
                        latest_block.height,
                        "CRITICAL FAILURE: Reth is out of sync with Irys block index!"
                    );
                }

                RethServiceActor::from_registry()
                    .send(ForkChoiceUpdateMessage {
                        head_hash: BlockHashType::Evm(latest_block.evm_block_hash),
                        confirmed_hash: Some(BlockHashType::Evm(latest_block.evm_block_hash)),
                        finalized_hash: None,
                    })
                    .await
                    .unwrap()
                    .unwrap();

                // Initialize the block_index actor and tell it about the genesis block
                let block_index_actor =
                    BlockIndexService::new(block_index.clone(), storage_config.clone());
                SystemRegistry::set(block_index_actor.start());
                let block_index_actor_addr = BlockIndexService::from_registry();

                if at_genesis {
                    let msg = BlockFinalizedMessage {
                        block_header: arc_genesis.clone(),
                        all_txs: Arc::new(vec![]),
                    };
                    db.update_eyre(|tx| irys_database::insert_block_header(tx, &arc_genesis))
                        .unwrap();
                    match block_index_actor_addr.send(msg).await {
                        Ok(_) => info!("Genesis block indexed"),
                        Err(_) => panic!("Failed to index genesis block"),
                    }
                }

                debug!("AT GENESIS {}", at_genesis);

                let mut epoch_service = EpochServiceActor::new(Some(config));
                epoch_service.initialize(&db).await;
                let epoch_service_actor_addr = epoch_service.start();

                // Retrieve ledger assignments
                let ledgers_guard = epoch_service_actor_addr
                    .send(GetLedgersGuardMessage)
                    .await
                    .unwrap();

                {
                    let ledgers = ledgers_guard.read();
                    debug!("ledgers: {:?}", ledgers);
                }

                let partition_assignments_guard = epoch_service_actor_addr
                    .send(GetPartitionAssignmentsGuardMessage)
                    .await
                    .unwrap();

                let block_index_guard = block_index_actor_addr
                    .send(GetBlockIndexGuardMessage)
                    .await
                    .unwrap();

                // Get the genesis storage modules and their assigned partitions
                let storage_module_infos = epoch_service_actor_addr
                    .send(GetGenesisStorageModulesMessage)
                    .await
                    .unwrap();

                // For Genesis we create the storage_modules and their files
                if at_genesis {
                    initialize_storage_files(
                        &arc_config.storage_module_dir(),
                        &storage_module_infos,
                        &STORAGE_SUBMODULES_CONFIG.with(|config| config.submodule_paths.clone()),
                    )
                    .unwrap();
                }

                // Create a list of storage modules wrapping the storage files
                for info in storage_module_infos {
                    let arc_module = Arc::new(
                        StorageModule::new(
                            &arc_config.storage_module_dir(),
                            &info,
                            storage_config.clone(),
                        )
                        // TODO: remove this unwrap
                        .unwrap(),
                    );
                    storage_modules.push(arc_module.clone());
                    // arc_module.pack_with_zeros();
                }

                let mempool_service = MempoolService::new(
                    db.clone(),
                    reth_node.task_executor.clone(),
                    node_config.mining_signer.clone(),
                    storage_config.clone(),
                    storage_modules.clone(),
                );
                let mempool_arbiter = Arbiter::new();
                SystemRegistry::set(MempoolService::start_in_arbiter(
                    &mempool_arbiter.handle(),
                    |_| mempool_service,
                ));
                let mempool_addr = MempoolService::from_registry();

                let chunk_migration_service = ChunkMigrationService::new(
                    block_index.clone(),
                    storage_config.clone(),
                    storage_modules.clone(),
                    db.clone(),
                );
                SystemRegistry::set(chunk_migration_service.start());

                let validation_service = ValidationService::new(
                    block_index_guard.clone(),
                    partition_assignments_guard.clone(),
                    storage_config.clone(),
                    vdf_config.clone(),
                );
                let validation_arbiter = Arbiter::new();
                SystemRegistry::set(ValidationService::start_in_arbiter(
                    &validation_arbiter.handle(),
                    |_| validation_service,
                ));

                let (_new_seed_tx, _new_seed_rx) = mpsc::channel::<H256>();

                let block_tree_service = BlockTreeService::new(
                    db.clone(),
                    block_index.clone(),
                    &miner_address,
                    block_index_guard.clone(),
                    storage_config.clone(),
                );
                let block_tree_arbiter = Arbiter::new();
                SystemRegistry::set(BlockTreeService::start_in_arbiter(
                    &block_tree_arbiter.handle(),
                    |_| block_tree_service,
                ));
                let block_tree_service = BlockTreeService::from_registry();

                let block_tree_guard = block_tree_service
                    .send(GetBlockTreeGuardMessage)
                    .await
                    .unwrap();

                let vdf_state = Arc::new(RwLock::new(VdfService::create_state(
                    Some(block_index_guard.clone()),
                    Some(db.clone()),
                )));

                let vdf_service_actor = VdfService::from_atomic_state(vdf_state);
                let vdf_service = vdf_service_actor.start();
                SystemRegistry::set(vdf_service.clone()); // register it as a service

                let vdf_steps_guard: VdfStepsReadGuard =
                    vdf_service.send(GetVdfStateMessage).await.unwrap();

                let (global_step_number, seed) = vdf_steps_guard.read().get_last_step_and_seed();
                info!("Starting at global step number: {}", global_step_number);

                let block_discovery_actor = BlockDiscoveryActor {
                    block_index_guard: block_index_guard.clone(),
                    partition_assignments_guard: partition_assignments_guard.clone(),
                    storage_config: storage_config.clone(),
                    difficulty_config: difficulty_adjustment_config,
                    db: db.clone(),
                    vdf_config: vdf_config.clone(),
                    vdf_steps_guard: vdf_steps_guard.clone(),
                };
                let block_discovery_arbiter = Arbiter::new();
                let block_discovery_addr = BlockDiscoveryActor::start_in_arbiter(
                    &block_discovery_arbiter.handle(),
                    |_| block_discovery_actor,
                );

                let block_producer_arbiter = Arbiter::new();
                let block_producer_actor = BlockProducerActor::new(
                    db.clone(),
                    mempool_addr.clone(),
                    block_discovery_addr.clone(),
                    epoch_service_actor_addr.clone(),
                    reth_node.clone(),
                    storage_config.clone(),
                    difficulty_adjustment_config,
                    vdf_config.clone(),
                    vdf_steps_guard.clone(),
                    block_tree_guard.clone(),
                );
                let block_producer_addr =
                    BlockProducerActor::start_in_arbiter(&block_producer_arbiter.handle(), |_| {
                        block_producer_actor
                    });

                let broadcast_arbiter = Arbiter::new();
                let broadcast_mining_service =
                    BroadcastMiningService::start_in_arbiter(&broadcast_arbiter.handle(), |_| {
                        BroadcastMiningService::default()
                    });
                SystemRegistry::set(broadcast_mining_service.clone());

                let mut part_actors = Vec::new();

                let atomic_global_step_number = Arc::new(AtomicU64::new(global_step_number));

                for sm in &storage_modules {
                    let partition_mining_actor = PartitionMiningActor::new(
                        miner_address,
                        db.clone(),
                        block_producer_addr.clone().recipient(),
                        sm.clone(),
                        false, // do not start mining automatically
                        vdf_steps_guard.clone(),
                        atomic_global_step_number.clone(),
                    );
                    let part_arbiter = Arbiter::new();
                    part_actors.push(PartitionMiningActor::start_in_arbiter(
                        &part_arbiter.handle(),
                        |_| partition_mining_actor,
                    ));
                }

                // Yield to let actors process their mailboxes (and subscribe to the mining_broadcaster)
                tokio::task::yield_now().await;

                let sm_ids = storage_modules.iter().map(|s| (*s).id).collect();

                let packing_actor_addr = PackingActor::new(
                    Handle::current(),
                    reth_node.task_executor.clone(),
                    sm_ids,
                    None,
                )
                .start();
                // request packing for uninitialized ranges
                for sm in &storage_modules {
                    let uninitialized = sm.get_intervals(ChunkType::Uninitialized);
                    debug!("ranges to pack: {:?}", &uninitialized);
                    let _ = uninitialized
                        .iter()
                        .map(|interval| {
                            packing_actor_addr.do_send(PackingRequest {
                                storage_module: sm.clone(),
                                chunk_range: (*interval).into(),
                            })
                        })
                        .collect::<Vec<()>>();
                }
                // let _ = wait_for_packing(packing_actor_addr.clone(), None).await;

                debug!("Packing complete");

                let part_actors_clone = part_actors.clone();

                // Let the partition actors know about the genesis difficulty
                broadcast_mining_service
                    .send(BroadcastDifficultyUpdate(latest_block.clone()))
                    .await
                    .unwrap();

                let (_new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();
                let (shutdown_tx, shutdown_rx) = mpsc::channel();

                let vdf_config2 = vdf_config.clone();
                let seed = seed.map_or(arc_genesis.vdf_limiter_info.output, |seed| seed.0);
                let vdf_reset_seed = latest_block.vdf_limiter_info.seed;

                info!(
                    "Starting VDF thread seed {:?} reset_seed {:?} step_number: {:?}",
                    seed, arc_genesis.vdf_limiter_info.seed, global_step_number
                );

                let vdf_thread_handler = std::thread::spawn(move || {
                    // Setup core affinity
                    let core_ids = core_affinity::get_core_ids().expect("Failed to get core IDs");
                    for core in core_ids {
                        let success = core_affinity::set_for_current(core);
                        if success {
                            info!("VDF thread pinned to core {:?}", core);
                            break;
                        }
                    }

                    run_vdf(
                        vdf_config2,
                        global_step_number,
                        seed,
                        vdf_reset_seed,
                        new_seed_rx,
                        shutdown_rx,
                        broadcast_mining_service.clone(),
                        vdf_service.clone(),
                        atomic_global_step_number.clone(),
                    )
                });

                let actor_addresses = ActorAddresses {
                    partitions: part_actors_clone,
                    block_producer: block_producer_addr,
                    packing: packing_actor_addr,
                    mempool: mempool_addr.clone(),
                    block_index: block_index_actor_addr,
                    epoch_service: epoch_service_actor_addr,
                };

                let chunk_provider =
                    ChunkProvider::new(storage_config.clone(), storage_modules.clone(), db.clone());
                let arc_chunk_provider = Arc::new(chunk_provider);
                // this OnceLock is due to the cyclic chain between Reth & the Irys node, where the IrysRethProvider requires both
                // this is "safe", as the OnceLock is always set before this start function returns
                irys_provider_1
                    .set(IrysRethProviderInner {
                        db: reth_node.provider.database.db.clone(),
                        chunk_provider: arc_chunk_provider.clone(),
                    })
                    .expect("Unable to set IrysRethProvider OnceLock");

                let _ = irys_node_handle_sender.send(IrysNodeCtx {
                    actor_addresses: actor_addresses.clone(),
                    reth_handle: reth_node.clone(),
                    db: db.clone(),
                    config: arc_config.clone(),
                    chunk_provider: arc_chunk_provider.clone(),
                    block_index_guard: block_index_guard.clone(),
                    vdf_steps_guard: vdf_steps_guard.clone(),
                    vdf_config: vdf_config.clone(),
                    storage_config: storage_config.clone(),
                });

                run_server(ApiState {
                    mempool: mempool_addr,
                    chunk_provider: arc_chunk_provider.clone(),
                    db,
                    reth_provider: Some(reth_node.clone()),
                    block_tree: Some(block_tree_guard.clone()),
                    block_index: Some(block_index_guard.clone()),
                })
                .await;

                // Send shutdown signal
                shutdown_tx.send(()).unwrap();

                // Wait for vdf thread to finish & save steps
                vdf_thread_handler.join().unwrap();
            });
        })?;

    // run reth in it's own thread w/ it's own tokio runtime
    // this is done as reth exhibits strange behaviour (notably channel dropping) when not in it's own context/when the exit future isn't been awaited

    std::thread::Builder::new().name("reth-thread".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let node_config= cloned_arc.clone();
            let tokio_runtime = /* Handle::current(); */ tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let mut task_manager = TaskManager::new(tokio_runtime.handle().clone());
            let exec: reth::tasks::TaskExecutor = task_manager.executor();

            tokio_runtime.block_on(run_to_completion_or_panic(
                &mut task_manager,
                run_until_ctrl_c(start_reth_node(exec, reth_chainspec, node_config, IrysTables::ALL, reth_handle_sender, irys_provider)),
            )).unwrap();
        })?;

    // wait for the full handle to be send over by the actix thread
    Ok(irys_node_handle_receiver.await?)
}

async fn start_reth_node<T: HasName + HasTableType>(
    exec: TaskExecutor,
    chainspec: ChainSpec,
    irys_config: Arc<IrysNodeConfig>,
    tables: &[T],
    sender: oneshot::Sender<FullNode<RethNode, RethNodeAddOns>>,
    irys_provider: IrysRethProvider,
) -> eyre::Result<NodeExitReason> {
    let node_handle = irys_reth_node_bridge::run_node(
        Arc::new(chainspec),
        exec,
        irys_config,
        tables,
        irys_provider,
    )
    .await?;
    sender
        .send(node_handle.node.clone())
        .expect("unable to send reth node handle");
    let exit_reason = node_handle.node_exit_future.await?;
    Ok(exit_reason)
}
