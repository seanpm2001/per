use {
    crate::{
        api::{
            self,
            ws,
        },
        auction::{
            get_express_relay_contract,
            run_submission_loop,
            run_tracker_loop,
        },
        config::{
            ChainId,
            Config,
            RunOptions,
        },
        models,
        opportunity_adapter::{
            get_eip_712_domain,
            get_weth_address,
            run_verification_loop,
        },
        per_metrics,
        state::{
            ChainStore,
            OpportunityStore,
            Store,
        },
        traced_client::TracedClient,
    },
    anyhow::anyhow,
    axum_prometheus::{
        metrics_exporter_prometheus::{
            PrometheusBuilder,
            PrometheusHandle,
        },
        utils::SECONDS_DURATION_BUCKETS,
    },
    ethers::{
        prelude::LocalWallet,
        providers::Middleware,
        signers::Signer,
    },
    futures::{
        future::join_all,
        Future,
    },
    sqlx::{
        migrate,
        postgres::PgPoolOptions,
        PgPool,
    },
    std::{
        collections::HashMap,
        sync::{
            atomic::{
                AtomicBool,
                AtomicUsize,
                Ordering,
            },
            Arc,
        },
        time::Duration,
    },
    tokio::{
        sync::RwLock,
        time::sleep,
    },
    tokio_util::task::TaskTracker,
};


async fn fault_tolerant_handler<F, Fut>(name: String, f: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    Fut::Output: Send + 'static,
{
    loop {
        let res = tokio::spawn(f()).await;
        match res {
            Ok(result) => match result {
                Ok(_) => break, // This will happen on graceful shutdown
                Err(err) => {
                    tracing::error!("{} returned error: {:?}", name, err);
                    sleep(Duration::from_millis(500)).await;
                }
            },
            Err(err) => {
                tracing::error!("{} is panicked or canceled: {:?}", name, err);
                SHOULD_EXIT.store(true, Ordering::Release);
                break;
            }
        }
    }
}

async fn fetch_access_tokens(db: &PgPool) -> HashMap<models::AccessTokenToken, models::Profile> {
    let access_tokens = sqlx::query_as!(
        models::AccessToken,
        "SELECT * FROM access_token WHERE revoked_at IS NULL",
    )
    .fetch_all(db)
    .await
    .expect("Failed to fetch access tokens from database");
    let profile_ids: Vec<models::ProfileId> =
        access_tokens.iter().map(|token| token.profile_id).collect();
    let profiles: Vec<models::Profile> = sqlx::query_as("SELECT * FROM profile WHERE id = ANY($1)")
        .bind(profile_ids)
        .fetch_all(db)
        .await
        .expect("Failed to fetch profiles from database");

    access_tokens
        .into_iter()
        .map(|token| {
            let profile = profiles
                .iter()
                .find(|profile| profile.id == token.profile_id)
                .expect("Profile not found");
            (token.token, profile.clone())
        })
        .collect()
}

pub fn setup_metrics_recorder() -> anyhow::Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .set_buckets(SECONDS_DURATION_BUCKETS)
        .unwrap()
        .install_recorder()
        .map_err(|err| anyhow!("Failed to set up metrics recorder: {:?}", err))
}

const NOTIFICATIONS_CHAN_LEN: usize = 1000;
pub async fn start_server(run_options: RunOptions) -> anyhow::Result<()> {
    tokio::spawn(async move {
        tracing::info!("Registered shutdown signal handler...");
        tokio::signal::ctrl_c().await.unwrap();
        tracing::info!("Shut down signal received, waiting for tasks...");
        SHOULD_EXIT.store(true, Ordering::Release);
    });

    let config = Config::load(&run_options.config.config).map_err(|err| {
        anyhow!(
            "Failed to load config from file({path}): {:?}",
            err,
            path = run_options.config.config
        )
    })?;

    let wallet = run_options.relayer_private_key.parse::<LocalWallet>()?;
    tracing::info!("Using wallet address: {}", wallet.address().to_string());

    let chain_store: anyhow::Result<HashMap<ChainId, ChainStore>> =
        join_all(config.chains.iter().map(|(chain_id, chain_config)| {
            let (chain_id, chain_config, wallet) =
                (chain_id.clone(), chain_config.clone(), wallet.clone());
            async move {
                let mut provider = TracedClient::new(chain_id.clone(), &chain_config.geth_rpc_addr)
                    .map_err(|err| {
                        anyhow!(
                            "Failed to connect to chain({chain_id}) at {rpc_addr}: {:?}",
                            err,
                            chain_id = chain_id,
                            rpc_addr = chain_config.geth_rpc_addr
                        )
                    })?;
                provider.set_interval(Duration::from_secs(chain_config.poll_interval));

                let id = provider.get_chainid().await?.as_u64();
                let weth =
                    get_weth_address(chain_config.opportunity_adapter_contract, provider.clone())
                        .await?;
                let eip_712_domain =
                    get_eip_712_domain(provider.clone(), chain_config.opportunity_adapter_contract)
                        .await
                        .map_err(|err| {
                            anyhow!(
                                "Failed to get domain separator for chain({chain_id}): {:?}",
                                err,
                                chain_id = chain_id
                            )
                        })?;

                let express_relay_contract = get_express_relay_contract(
                    chain_config.express_relay_contract,
                    provider.clone(),
                    wallet.clone(),
                    chain_config.legacy_tx,
                    id,
                );

                Ok((
                    chain_id.clone(),
                    ChainStore {
                        provider,
                        network_id: id,
                        token_spoof_info: Default::default(),
                        config: chain_config.clone(),
                        weth,
                        eip_712_domain,
                        express_relay_contract: Arc::new(express_relay_contract),
                    },
                ))
            }
        }))
        .await
        .into_iter()
        .collect();

    let (broadcast_sender, broadcast_receiver) =
        tokio::sync::broadcast::channel(NOTIFICATIONS_CHAN_LEN);

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&run_options.server.database_url)
        .await
        .expect("Server should start with a valid database connection.");
    match migrate!("./migrations").run(&pool).await {
        Ok(()) => {}
        Err(err) => match err {
            sqlx::migrate::MigrateError::VersionMissing(version) => {
                tracing::info!(
                    "Found missing migration ({}) probably because of downgrade",
                    version
                );
            }
            _ => {
                return Err(anyhow!("Failed to run migrations: {:?}", err));
            }
        },
    }
    let task_tracker = TaskTracker::new();

    let access_tokens = fetch_access_tokens(&pool).await;
    let store = Arc::new(Store {
        db:                 pool,
        bids:               Default::default(),
        chains:             chain_store?,
        opportunity_store:  OpportunityStore::default(),
        event_sender:       broadcast_sender.clone(),
        relayer:            wallet,
        ws:                 ws::WsState {
            subscriber_counter: AtomicUsize::new(0),
            broadcast_sender,
            broadcast_receiver,
        },
        task_tracker:       task_tracker.clone(),
        auction_lock:       Default::default(),
        submitted_auctions: Default::default(),
        secret_key:         run_options.secret_key.clone(),
        access_tokens:      RwLock::new(access_tokens),
        metrics_recorder:   setup_metrics_recorder()?,
    });

    tokio::join!(
        async {
            let submission_loops = store.chains.keys().map(|chain_id| {
                fault_tolerant_handler(
                    format!("submission loop for chain {}", chain_id.clone()),
                    || run_submission_loop(store.clone(), chain_id.clone()),
                )
            });
            join_all(submission_loops).await;
        },
        async {
            let tracker_loops = store.chains.keys().map(|chain_id| {
                fault_tolerant_handler(
                    format!("tracker loop for chain {}", chain_id.clone()),
                    || run_tracker_loop(store.clone(), chain_id.clone()),
                )
            });
            join_all(tracker_loops).await;
        },
        fault_tolerant_handler("verification loop".to_string(), || run_verification_loop(
            store.clone()
        )),
        fault_tolerant_handler("start api".to_string(), || api::start_api(
            run_options.clone(),
            store.clone()
        )),
        fault_tolerant_handler("start metrics".to_string(), || per_metrics::start_metrics(
            run_options.clone(),
            store.clone()
        )),
    );

    // To make sure all the spawned tasks will finish their job before shut down
    // Closing task tracker doesn't mean that it won't accept new tasks!!
    task_tracker.close();
    task_tracker.wait().await;

    Ok(())
}

// A static exit flag to indicate to running threads that we're shutting down. This is used to
// gracefully shutdown the application.
//
// NOTE: A more idiomatic approach would be to use a tokio::sync::broadcast channel, and to send a
// shutdown signal to all running tasks. However, this is a bit more complicated to implement and
// we don't rely on global state for anything else.
pub(crate) static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);
pub const EXIT_CHECK_INTERVAL: Duration = Duration::from_secs(1);
