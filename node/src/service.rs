//! Service implementation. Specialized wrapper over substrate service.

use crate::cli::Cli;
use acuity_runtime::opaque::Block;
use acuity_runtime::{self, RuntimeApi};
use codec::Encode;
use frame_benchmarking_cli::SUBSTRATE_REFERENCE_HARDWARE;
use frame_system_rpc_runtime_api::AccountNonceApi;
use futures::prelude::*;
use sc_client_api::{Backend, BlockBackend};
use sc_consensus_babe::{self, SlotProportion};
use sc_network::{event::Event, NetworkEventStream, NetworkService};
use sc_network_sync::{warp::WarpSyncParams, SyncingService};
use sc_service::{config::Configuration, error::Error as ServiceError, RpcHandlers, TaskManager};
use sc_statement_store::Store as StatementStore;
use sc_telemetry::{Telemetry, TelemetryWorker};
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_api::ProvideRuntimeApi;
use sp_core::crypto::Pair;
use sp_runtime::{generic, traits::Block as BlockT, SaturatedConversion};
use std::sync::Arc;

/// Host functions required for kitchensink runtime and Substrate node.
#[cfg(not(feature = "runtime-benchmarks"))]
pub type HostFunctions = (
    sp_io::SubstrateHostFunctions,
    sp_statement_store::runtime_api::HostFunctions,
);

/// Host functions required for kitchensink runtime and Substrate node.
#[cfg(feature = "runtime-benchmarks")]
pub type HostFunctions = (
    sp_io::SubstrateHostFunctions,
    sp_statement_store::runtime_api::HostFunctions,
    frame_benchmarking::benchmarking::HostFunctions,
);

/// A specialized `WasmExecutor` intended to use accross substrate node. It provides all required
/// HostFunctions.
pub type RuntimeExecutor = sc_executor::WasmExecutor<HostFunctions>;

/// The full client type definition.
pub type FullClient = sc_service::TFullClient<Block, RuntimeApi, RuntimeExecutor>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;
type FullGrandpaBlockImport =
    sc_consensus_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>;

/// The transaction pool type definition.
pub type TransactionPool = sc_transaction_pool::FullPool<Block, FullClient>;

/// The minimum period of blocks on which justifications will be
/// imported and generated.
const GRANDPA_JUSTIFICATION_PERIOD: u32 = 512;

/// Fetch the nonce of the given `account` from the chain state.
///
/// Note: Should only be used for tests.
pub fn fetch_nonce(client: &FullClient, account: sp_core::sr25519::Pair) -> u32 {
    let best_hash = client.chain_info().best_hash;
    client
        .runtime_api()
        .account_nonce(best_hash, account.public().into())
        .expect("Fetching account nonce works; qed")
}

/// Create a transaction using the given `call`.
///
/// The transaction will be signed by `sender`. If `nonce` is `None` it will be fetched from the
/// state of the best block.
///
/// Note: Should only be used for tests.
#[allow(dead_code)]
pub fn create_extrinsic(
    client: &FullClient,
    sender: sp_core::sr25519::Pair,
    function: impl Into<acuity_runtime::RuntimeCall>,
    nonce: Option<u32>,
) -> acuity_runtime::UncheckedExtrinsic {
    let function = function.into();
    let genesis_hash = client
        .block_hash(0)
        .ok()
        .flatten()
        .expect("Genesis block exists; qed");
    let best_hash = client.chain_info().best_hash;
    let best_block = client.chain_info().best_number;
    let nonce = nonce.unwrap_or_else(|| fetch_nonce(client, sender.clone()));

    let period = acuity_runtime::BlockHashCount::get()
        .checked_next_power_of_two()
        .map(|c| c / 2)
        .unwrap_or(2) as u64;
    let tip = 0;
    let extra: acuity_runtime::SignedExtra = (
        frame_system::CheckNonZeroSender::<acuity_runtime::Runtime>::new(),
        frame_system::CheckSpecVersion::<acuity_runtime::Runtime>::new(),
        frame_system::CheckTxVersion::<acuity_runtime::Runtime>::new(),
        frame_system::CheckGenesis::<acuity_runtime::Runtime>::new(),
        frame_system::CheckEra::<acuity_runtime::Runtime>::from(generic::Era::mortal(
            period,
            best_block.saturated_into(),
        )),
        frame_system::CheckNonce::<acuity_runtime::Runtime>::from(nonce),
        frame_system::CheckWeight::<acuity_runtime::Runtime>::new(),
        pallet_transaction_payment::ChargeTransactionPayment::<acuity_runtime::Runtime>::from(tip),
    );

    let raw_payload = acuity_runtime::SignedPayload::from_raw(
        function.clone(),
        extra.clone(),
        (
            (),
            acuity_runtime::VERSION.spec_version,
            acuity_runtime::VERSION.transaction_version,
            genesis_hash,
            best_hash,
            (),
            (),
            (),
        ),
    );
    let signature = raw_payload.using_encoded(|e| sender.sign(e));

    acuity_runtime::UncheckedExtrinsic::new_signed(
        function,
        sp_runtime::AccountId32::from(sender.public()).into(),
        acuity_runtime::Signature::Sr25519(signature),
        extra,
    )
}

/// Creates a new partial node.
pub fn new_partial(
    config: &Configuration,
    mixnet_config: Option<&sc_mixnet::Config>,
) -> Result<
    sc_service::PartialComponents<
        FullClient,
        FullBackend,
        FullSelectChain,
        sc_consensus::DefaultImportQueue<Block>,
        sc_transaction_pool::FullPool<Block, FullClient>,
        (
            impl Fn(
                crate::rpc::DenyUnsafe,
                sc_rpc::SubscriptionTaskExecutor,
            ) -> Result<jsonrpsee::RpcModule<()>, sc_service::Error>,
            (
                sc_consensus_babe::BabeBlockImport<Block, FullClient, FullGrandpaBlockImport>,
                sc_consensus_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
                sc_consensus_babe::BabeLink<Block>,
            ),
            sc_consensus_grandpa::SharedVoterState,
            Option<Telemetry>,
            Arc<StatementStore>,
            Option<sc_mixnet::ApiBackend>,
        ),
    >,
    ServiceError,
> {
    let telemetry = config
        .telemetry_endpoints
        .clone()
        .filter(|x| !x.is_empty())
        .map(|endpoints| -> Result<_, sc_telemetry::Error> {
            let worker = TelemetryWorker::new(16)?;
            let telemetry = worker.handle().new_telemetry(endpoints);
            Ok((worker, telemetry))
        })
        .transpose()?;

    let executor = sc_service::new_wasm_executor(&config);

    let (client, backend, keystore_container, task_manager) =
        sc_service::new_full_parts::<Block, RuntimeApi, _>(
            config,
            telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
            executor,
        )?;
    let client = Arc::new(client);

    let telemetry = telemetry.map(|(worker, telemetry)| {
        task_manager
            .spawn_handle()
            .spawn("telemetry", None, worker.run());
        telemetry
    });

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = sc_transaction_pool::BasicPool::new_full(
        config.transaction_pool.clone(),
        config.role.is_authority().into(),
        config.prometheus_registry(),
        task_manager.spawn_essential_handle(),
        client.clone(),
    );

    let (grandpa_block_import, grandpa_link) = sc_consensus_grandpa::block_import(
        client.clone(),
        GRANDPA_JUSTIFICATION_PERIOD,
        &(client.clone() as Arc<_>),
        select_chain.clone(),
        telemetry.as_ref().map(|x| x.handle()),
    )?;
    let justification_import = grandpa_block_import.clone();

    let (block_import, babe_link) = sc_consensus_babe::block_import(
        sc_consensus_babe::configuration(&*client)?,
        grandpa_block_import,
        client.clone(),
    )?;

    let slot_duration = babe_link.config().slot_duration();
    let (import_queue, babe_worker_handle) =
        sc_consensus_babe::import_queue(sc_consensus_babe::ImportQueueParams {
            link: babe_link.clone(),
            block_import: block_import.clone(),
            justification_import: Some(Box::new(justification_import)),
            client: client.clone(),
            select_chain: select_chain.clone(),
            create_inherent_data_providers: move |_, ()| async move {
                let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                let slot =
				sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
					*timestamp,
					slot_duration,
				);

                Ok((slot, timestamp))
            },
            spawner: &task_manager.spawn_essential_handle(),
            registry: config.prometheus_registry(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
            offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(transaction_pool.clone()),
        })?;

    let import_setup = (block_import, grandpa_link, babe_link);

    let statement_store = sc_statement_store::Store::new_shared(
        &config.data_path,
        Default::default(),
        client.clone(),
        keystore_container.local_keystore(),
        config.prometheus_registry(),
        &task_manager.spawn_handle(),
    )
    .map_err(|e| ServiceError::Other(format!("Statement store error: {:?}", e)))?;

    let (mixnet_api, mixnet_api_backend) = mixnet_config.map(sc_mixnet::Api::new).unzip();

    let (rpc_extensions_builder, rpc_setup) = {
        let (_, grandpa_link, _) = &import_setup;

        let justification_stream = grandpa_link.justification_stream();
        let shared_authority_set = grandpa_link.shared_authority_set().clone();
        let shared_voter_state = sc_consensus_grandpa::SharedVoterState::empty();
        let shared_voter_state2 = shared_voter_state.clone();

        let finality_proof_provider = sc_consensus_grandpa::FinalityProofProvider::new_for_service(
            backend.clone(),
            Some(shared_authority_set.clone()),
        );

        let client = client.clone();
        let pool = transaction_pool.clone();
        let select_chain = select_chain.clone();
        let keystore = keystore_container.keystore();
        let chain_spec = config.chain_spec.cloned_box();

        let rpc_backend = backend.clone();
        let rpc_statement_store = statement_store.clone();
        let rpc_extensions_builder = move |deny_unsafe, subscription_executor| {
            let deps = crate::rpc::FullDeps {
                client: client.clone(),
                pool: pool.clone(),
                select_chain: select_chain.clone(),
                chain_spec: chain_spec.cloned_box(),
                deny_unsafe,
                babe: crate::rpc::BabeDeps {
                    keystore: keystore.clone(),
                    babe_worker_handle: babe_worker_handle.clone(),
                },
                grandpa: crate::rpc::GrandpaDeps {
                    shared_voter_state: shared_voter_state.clone(),
                    shared_authority_set: shared_authority_set.clone(),
                    justification_stream: justification_stream.clone(),
                    subscription_executor,
                    finality_provider: finality_proof_provider.clone(),
                },
                statement_store: rpc_statement_store.clone(),
                backend: rpc_backend.clone(),
                mixnet_api: mixnet_api.as_ref().cloned(),
            };

            crate::rpc::create_full(deps).map_err(Into::into)
        };

        (rpc_extensions_builder, shared_voter_state2)
    };

    Ok(sc_service::PartialComponents {
        client,
        backend,
        task_manager,
        keystore_container,
        select_chain,
        import_queue,
        transaction_pool,
        other: (
            rpc_extensions_builder,
            import_setup,
            rpc_setup,
            telemetry,
            statement_store,
            mixnet_api_backend,
        ),
    })
}

/// Result of [`new_full_base`].
pub struct NewFullBase {
    /// The task manager of the node.
    pub task_manager: TaskManager,
    /// The client instance of the node.
    pub client: Arc<FullClient>,
    /// The networking service of the node.
    pub network: Arc<NetworkService<Block, <Block as BlockT>::Hash>>,
    /// The syncing service of the node.
    pub sync: Arc<SyncingService<Block>>,
    /// The transaction pool of the node.
    pub transaction_pool: Arc<TransactionPool>,
    /// The rpc handlers of the node.
    pub rpc_handlers: RpcHandlers,
}

/// Creates a full service from the configuration.
pub fn new_full_base(
    config: Configuration,
    mixnet_config: Option<sc_mixnet::Config>,
    disable_hardware_benchmarks: bool,
    with_startup_data: impl FnOnce(
        &sc_consensus_babe::BabeBlockImport<Block, FullClient, FullGrandpaBlockImport>,
        &sc_consensus_babe::BabeLink<Block>,
    ),
) -> Result<NewFullBase, ServiceError> {
    let hwbench = (!disable_hardware_benchmarks)
        .then_some(config.database.path().map(|database_path| {
            let _ = std::fs::create_dir_all(&database_path);
            sc_sysinfo::gather_hwbench(Some(database_path))
        }))
        .flatten();

    let sc_service::PartialComponents {
        client,
        backend,
        mut task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other:
            (rpc_builder, import_setup, rpc_setup, mut telemetry, statement_store, mixnet_api_backend),
    } = new_partial(&config, mixnet_config.as_ref())?;

    let shared_voter_state = rpc_setup;
    let auth_disc_publish_non_global_ips = config.network.allow_non_globals_in_dht;
    let mut net_config = sc_network::config::FullNetworkConfiguration::new(&config.network);
    let genesis_hash = client
        .block_hash(0)
        .ok()
        .flatten()
        .expect("Genesis block exists; qed");

    let grandpa_protocol_name =
        sc_consensus_grandpa::protocol_standard_name(&genesis_hash, &config.chain_spec);
    let (grandpa_protocol_config, grandpa_notification_service) =
        sc_consensus_grandpa::grandpa_peers_set_config(grandpa_protocol_name.clone());
    net_config.add_notification_protocol(grandpa_protocol_config);

    let (statement_handler_proto, statement_config) =
        sc_network_statement::StatementHandlerPrototype::new(
            genesis_hash,
            config.chain_spec.fork_id(),
        );
    net_config.add_notification_protocol(statement_config);

    let mixnet_protocol_name =
        sc_mixnet::protocol_name(genesis_hash.as_ref(), config.chain_spec.fork_id());
    let mixnet_notification_service = mixnet_config.as_ref().map(|mixnet_config| {
        let (config, notification_service) =
            sc_mixnet::peers_set_config(mixnet_protocol_name.clone(), mixnet_config);
        net_config.add_notification_protocol(config);
        notification_service
    });

    let warp_sync = Arc::new(sc_consensus_grandpa::warp_proof::NetworkProvider::new(
        backend.clone(),
        import_setup.1.shared_authority_set().clone(),
        Vec::default(),
    ));

    let (network, system_rpc_tx, tx_handler_controller, network_starter, sync_service) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            net_config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            block_announce_validator_builder: None,
            warp_sync_params: Some(WarpSyncParams::WithProvider(warp_sync)),
            block_relay: None,
        })?;

    if let Some(mixnet_config) = mixnet_config {
        let mixnet = sc_mixnet::run(
            mixnet_config,
            mixnet_api_backend.expect("Mixnet API backend created if mixnet enabled"),
            client.clone(),
            sync_service.clone(),
            network.clone(),
            mixnet_protocol_name,
            transaction_pool.clone(),
            Some(keystore_container.keystore()),
            mixnet_notification_service
                .expect("`NotificationService` exists since mixnet was enabled; qed"),
        );
        task_manager.spawn_handle().spawn("mixnet", None, mixnet);
    }

    let role = config.role.clone();
    let force_authoring = config.force_authoring;
    let backoff_authoring_blocks =
        Some(sc_consensus_slots::BackoffAuthoringOnFinalizedHeadLagging::default());
    let name = config.network.node_name.clone();
    let enable_grandpa = !config.disable_grandpa;
    let prometheus_registry = config.prometheus_registry().cloned();
    let enable_offchain_worker = config.offchain_worker.enabled;

    let rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
        config,
        backend: backend.clone(),
        client: client.clone(),
        keystore: keystore_container.keystore(),
        network: network.clone(),
        rpc_builder: Box::new(rpc_builder),
        transaction_pool: transaction_pool.clone(),
        task_manager: &mut task_manager,
        system_rpc_tx,
        tx_handler_controller,
        sync_service: sync_service.clone(),
        telemetry: telemetry.as_mut(),
    })?;

    if let Some(hwbench) = hwbench {
        sc_sysinfo::print_hwbench(&hwbench);
        match SUBSTRATE_REFERENCE_HARDWARE.check_hardware(&hwbench) {
            Err(err) if role.is_authority() => {
                log::warn!(
					"⚠️  The hardware does not meet the minimal requirements {} for role 'Authority'.",
					err
				);
            }
            _ => {}
        }

        if let Some(ref mut telemetry) = telemetry {
            let telemetry_handle = telemetry.handle();
            task_manager.spawn_handle().spawn(
                "telemetry_hwbench",
                None,
                sc_sysinfo::initialize_hwbench_telemetry(telemetry_handle, hwbench),
            );
        }
    }

    let (block_import, grandpa_link, babe_link) = import_setup;

    (with_startup_data)(&block_import, &babe_link);

    if let sc_service::config::Role::Authority { .. } = &role {
        let proposer = sc_basic_authorship::ProposerFactory::new(
            task_manager.spawn_handle(),
            client.clone(),
            transaction_pool.clone(),
            prometheus_registry.as_ref(),
            telemetry.as_ref().map(|x| x.handle()),
        );

        let client_clone = client.clone();
        let slot_duration = babe_link.config().slot_duration();
        let babe_config = sc_consensus_babe::BabeParams {
            keystore: keystore_container.keystore(),
            client: client.clone(),
            select_chain,
            env: proposer,
            block_import,
            sync_oracle: sync_service.clone(),
            justification_sync_link: sync_service.clone(),
            create_inherent_data_providers: move |parent, ()| {
                let client_clone = client_clone.clone();
                async move {
                    let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                    let slot =
						sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
							*timestamp,
							slot_duration,
						);

                    let storage_proof =
                        sp_transaction_storage_proof::registration::new_data_provider(
                            &*client_clone,
                            &parent,
                        )?;

                    Ok((slot, timestamp, storage_proof))
                }
            },
            force_authoring,
            backoff_authoring_blocks,
            babe_link,
            block_proposal_slot_portion: SlotProportion::new(0.5),
            max_block_proposal_slot_portion: None,
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        };

        let babe = sc_consensus_babe::start_babe(babe_config)?;
        task_manager.spawn_essential_handle().spawn_blocking(
            "babe-proposer",
            Some("block-authoring"),
            babe,
        );
    }

    // Spawn authority discovery module.
    if role.is_authority() {
        let authority_discovery_role =
            sc_authority_discovery::Role::PublishAndDiscover(keystore_container.keystore());
        let dht_event_stream =
            network
                .event_stream("authority-discovery")
                .filter_map(|e| async move {
                    match e {
                        Event::Dht(e) => Some(e),
                        _ => None,
                    }
                });
        let (authority_discovery_worker, _service) =
            sc_authority_discovery::new_worker_and_service_with_config(
                sc_authority_discovery::WorkerConfig {
                    publish_non_global_ips: auth_disc_publish_non_global_ips,
                    ..Default::default()
                },
                client.clone(),
                network.clone(),
                Box::pin(dht_event_stream),
                authority_discovery_role,
                prometheus_registry.clone(),
            );

        task_manager.spawn_handle().spawn(
            "authority-discovery-worker",
            Some("networking"),
            authority_discovery_worker.run(),
        );
    }

    // if the node isn't actively participating in consensus then it doesn't
    // need a keystore, regardless of which protocol we use below.
    let keystore = if role.is_authority() {
        Some(keystore_container.keystore())
    } else {
        None
    };

    let grandpa_config = sc_consensus_grandpa::Config {
        // FIXME #1578 make this available through chainspec
        gossip_duration: std::time::Duration::from_millis(333),
        justification_generation_period: GRANDPA_JUSTIFICATION_PERIOD,
        name: Some(name),
        observer_enabled: false,
        keystore,
        local_role: role.clone(),
        telemetry: telemetry.as_ref().map(|x| x.handle()),
        protocol_name: grandpa_protocol_name,
    };

    if enable_grandpa {
        // start the full GRANDPA voter
        // NOTE: non-authorities could run the GRANDPA observer protocol, but at
        // this point the full voter should provide better guarantees of block
        // and vote data availability than the observer. The observer has not
        // been tested extensively yet and having most nodes in a network run it
        // could lead to finality stalls.
        let grandpa_params = sc_consensus_grandpa::GrandpaParams {
            config: grandpa_config,
            link: grandpa_link,
            network: network.clone(),
            sync: Arc::new(sync_service.clone()),
            notification_service: grandpa_notification_service,
            telemetry: telemetry.as_ref().map(|x| x.handle()),
            voting_rule: sc_consensus_grandpa::VotingRulesBuilder::default().build(),
            prometheus_registry: prometheus_registry.clone(),
            shared_voter_state,
            offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(transaction_pool.clone()),
        };

        // the GRANDPA voter task is considered infallible, i.e.
        // if it fails we take down the service with it.
        task_manager.spawn_essential_handle().spawn_blocking(
            "grandpa-voter",
            None,
            sc_consensus_grandpa::run_grandpa_voter(grandpa_params)?,
        );
    }

    // Spawn statement protocol worker
    let statement_protocol_executor = {
        let spawn_handle = task_manager.spawn_handle();
        Box::new(move |fut| {
            spawn_handle.spawn("network-statement-validator", Some("networking"), fut);
        })
    };
    let statement_handler = statement_handler_proto.build(
        network.clone(),
        sync_service.clone(),
        statement_store.clone(),
        prometheus_registry.as_ref(),
        statement_protocol_executor,
    )?;
    task_manager.spawn_handle().spawn(
        "network-statement-handler",
        Some("networking"),
        statement_handler.run(),
    );

    if enable_offchain_worker {
        task_manager.spawn_handle().spawn(
            "offchain-workers-runner",
            "offchain-work",
            sc_offchain::OffchainWorkers::new(sc_offchain::OffchainWorkerOptions {
                runtime_api_provider: client.clone(),
                keystore: Some(keystore_container.keystore()),
                offchain_db: backend.offchain_storage(),
                transaction_pool: Some(OffchainTransactionPoolFactory::new(
                    transaction_pool.clone(),
                )),
                network_provider: network.clone(),
                is_validator: role.is_authority(),
                enable_http_requests: true,
                custom_extensions: move |_| {
                    vec![Box::new(statement_store.clone().as_statement_store_ext()) as Box<_>]
                },
            })
            .run(client.clone(), task_manager.spawn_handle())
            .boxed(),
        );
    }

    network_starter.start_network();
    Ok(NewFullBase {
        task_manager,
        client,
        network,
        sync: sync_service,
        transaction_pool,
        rpc_handlers,
    })
}

/// Builds a new service for a full client.
pub fn new_full(config: Configuration, cli: Cli) -> Result<TaskManager, ServiceError> {
    let mixnet_config = cli.mixnet_params.config(config.role.is_authority());
    let database_source = config.database.clone();
    let task_manager = new_full_base(config, mixnet_config, cli.no_hardware_benchmarks, |_, _| ())
        .map(|NewFullBase { task_manager, .. }| task_manager)?;

    sc_storage_monitor::StorageMonitorService::try_spawn(
        cli.storage_monitor,
        database_source,
        &task_manager.spawn_essential_handle(),
    )
    .map_err(|e| ServiceError::Application(e.into()))?;

    Ok(task_manager)
}
