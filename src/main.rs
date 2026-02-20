// Copyright (C) 2024, 2025 Hydra-Pool Developers (see AUTHORS)
//
// This file is part of Hydra-Pool.
//
// Hydra-Pool is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option)
// any later version.
//
// Hydra-Pool is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with
// Hydra-Pool. If not, see <https://www.gnu.org/licenses/>.

use clap::Parser;
use p2poolv2_api::start_api_server;
use p2poolv2_lib::accounting::stats::metrics;
use p2poolv2_lib::config::Config;
use p2poolv2_lib::logging::setup_logging;
use p2poolv2_lib::node::actor::NodeHandle;
use p2poolv2_lib::shares::chain::chain_store_handle::ChainStoreHandle;
use p2poolv2_lib::shares::share_block::ShareBlock;
use p2poolv2_lib::store::Store;
use p2poolv2_lib::store::writer::{StoreHandle, StoreWriter, write_channel};
use p2poolv2_lib::stratum::client_connections::start_connections_handler;
use p2poolv2_lib::stratum::emission::Emission;
use p2poolv2_lib::stratum::server::StratumServerBuilder;
use p2poolv2_lib::stratum::work::gbt::start_gbt;
use p2poolv2_lib::stratum::work::notify::start_notify;
use p2poolv2_lib::stratum::work::tracker::start_tracker_actor;
use p2poolv2_lib::stratum::zmq_listener::{ZmqListener, ZmqListenerTrait};
use p2poolv2_lib::stratum_sv2::channels::start_channel_manager;
use p2poolv2_lib::stratum_sv2::connection::{AuthorityKeypair, Sv2ServerConfig, run_accept_loop};
use p2poolv2_lib::stratum_sv2::connections::start_sv2_connections_handler;
use p2poolv2_lib::stratum_sv2::handler::{Sv2ConnectionContext, handle_sv2_connection};
use p2poolv2_lib::stratum_sv2::job_distributor::start_job_distributor;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::error;
use tracing::info;

/// Interval in seconds to poll for new block templates since the last zmq signal
const GBT_POLL_INTERVAL: u64 = 10; // seconds

/// Maximum number of pending shares from all clients connected to stratum server
const STRATUM_SHARES_BUFFER_SIZE: usize = 1000;

/// 100% donation in bips, skip address validation
const FULL_DONATION_BIPS: u16 = 10_000;

/// Notify channel enqueues requests to send notify updates to new
/// clients. If we have more than notify channel capacity of pending
/// clients in queue, some will be dropped.
const NOTIFY_CHANNEL_CAPACITY: usize = 1000;

/// Wait for shutdown signals (Ctrl+C, SIGTERM on Unix) or internal shutdown signal.
/// Returns when any shutdown signal is received.
#[cfg(unix)]
async fn wait_for_shutdown_signal(stopping_rx: oneshot::Receiver<()>) {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("Failed to set up SIGTERM handler");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, initiating graceful shutdown...");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, initiating graceful shutdown...");
        }
        _ = stopping_rx => {
            info!("Node stopping due to internal signal...");
        }
    }
}

/// Wait for shutdown signals (Ctrl+C) or internal shutdown signal.
/// Returns when any shutdown signal is received.
#[cfg(not(unix))]
async fn wait_for_shutdown_signal(stopping_rx: oneshot::Receiver<()>) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, initiating graceful shutdown...");
        }
        _ = stopping_rx => {
            info!("Node stopping due to internal signal...");
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    config: String,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    info!("Starting Hydrapool...");
    // Parse command line arguments
    let args = Args::parse();

    // Load configuration
    let config = Config::load(&args.config);
    if config.is_err() {
        let err = config.unwrap_err();
        error!("Failed to load config: {err}");
        return Err(format!("Failed to load config: {err}"));
    }
    let config = config.unwrap();
    // Configure logging based on config
    let logging_result = setup_logging(&config.logging);
    // hold guard to ensure logging is set up correctly
    let _guard = match logging_result {
        Ok(guard) => {
            info!("Logging set up successfully");
            guard
        }
        Err(e) => {
            error!("Failed to set up logging: {e}");
            return Err(format!("Failed to set up logging: {e}"));
        }
    };

    let genesis = ShareBlock::build_genesis_for_network(config.stratum.network);
    let store = Arc::new(Store::new(config.store.path.clone(), false).unwrap());

    // Create StoreWriter for serialized database writes (runs on dedicated blocking thread)
    let (write_tx, write_rx) = write_channel();
    let store_writer = StoreWriter::new(store.clone(), write_rx);
    tokio::task::spawn_blocking(move || store_writer.run());

    // Create StoreHandle and ChainStoreHandle
    let store_handle = StoreHandle::new(store.clone(), write_tx);
    let chain_store_handle = ChainStoreHandle::new(store_handle, config.stratum.network);

    if let Err(e) = chain_store_handle
        .init_or_setup_genesis(genesis.clone())
        .await
    {
        error!("Failed to initialize chain: {e}");
        return Err(format!("Failed to initialize chain: {e}"));
    }

    let tip = chain_store_handle.store_handle().get_chain_tip();
    let height = chain_store_handle.get_tip_height();
    info!("Latest tip {:?} at height {:?}", tip, height);

    let background_tasks_store = store.clone();
    p2poolv2_lib::store::background_tasks::start_background_tasks(
        background_tasks_store,
        Duration::from_secs(config.store.background_task_frequency_hours * 3600),
        Duration::from_secs(config.store.pplns_ttl_days * 3600 * 24),
    );

    let stratum_config = config.stratum.clone().parse().unwrap();
    let bitcoinrpc_config = config.bitcoinrpc.clone();

    let (stratum_shutdown_tx, stratum_shutdown_rx) = tokio::sync::oneshot::channel();
    let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(NOTIFY_CHANNEL_CAPACITY);
    let tracker_handle = start_tracker_actor();

    let notify_tx_for_gbt = notify_tx.clone();
    let bitcoinrpc_config_cloned = bitcoinrpc_config.clone();
    // Setup ZMQ publisher for block notifications
    let zmq_trigger_rx = match ZmqListener.start(&stratum_config.zmqpubhashblock) {
        Ok(rx) => rx,
        Err(e) => {
            error!("Failed to set up ZMQ publisher: {e}");
            return Err("Failed to set up ZMQ publisher".into());
        }
    };

    tokio::spawn(async move {
        if let Err(e) = start_gbt(
            bitcoinrpc_config_cloned,
            notify_tx_for_gbt,
            GBT_POLL_INTERVAL,
            stratum_config.network,
            zmq_trigger_rx,
        )
        .await
        {
            tracing::error!("Failed to fetch block template. Shutting down. \n {e}");
            exit(1);
        }
    });

    let connections_handle = start_connections_handler().await;
    let connections_cloned = connections_handle.clone();

    let tracker_handle_cloned = tracker_handle.clone();
    let chain_store_handle_for_notify = chain_store_handle.clone();
    let miner_pubkey = config
        .miner
        .as_ref()
        .map(|miner_config| miner_config.pubkey);

    let cloned_stratum_config = stratum_config.clone();
    tokio::spawn(async move {
        info!("Starting Stratum notifier...");
        // This will run indefinitely, sending new block templates to the Stratum server as they arrive
        start_notify(
            notify_rx,
            connections_cloned,
            chain_store_handle_for_notify,
            tracker_handle_cloned,
            &cloned_stratum_config,
            miner_pubkey,
        )
        .await;
    });

    let (emissions_tx, emissions_rx) =
        tokio::sync::mpsc::channel::<Emission>(STRATUM_SHARES_BUFFER_SIZE);

    // --- SV2 Server Startup ---
    // Clone emissions_tx before SV1 takes ownership, so SV2 shares feed
    // into the same accounting pipeline.
    let mut _sv2_shutdown_tx: Option<oneshot::Sender<()>> = None;
    if let Some(ref sv2_config) = config.stratum_sv2 {
        if sv2_config.enabled {
            if let Err(e) = sv2_config.validate() {
                error!("Invalid SV2 config: {e}");
                return Err(format!("Invalid SV2 config: {e}"));
            }

            let sv2_emissions_tx = emissions_tx.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            _sv2_shutdown_tx = Some(shutdown_tx);

            // Resolve authority keypair
            let authority = match (
                &sv2_config.authority_public_key,
                &sv2_config.authority_secret_key,
            ) {
                (Some(pk), Some(sk)) => {
                    AuthorityKeypair::from_config(pk, sk, sv2_config.cert_validity_seconds)
                        .map_err(|e| format!("SV2 keypair error: {e}"))?
                }
                _ => {
                    error!("SV2 enabled but authority_public_key/authority_secret_key not set");
                    return Err(
                        "SV2 requires authority_public_key and authority_secret_key in config"
                            .to_string(),
                    );
                }
            };

            let sv2_server_config = Sv2ServerConfig {
                hostname: sv2_config.hostname.clone(),
                port: sv2_config.port,
                authority,
            };

            // Start SV2 actors
            let sv2_connections = start_sv2_connections_handler().await;
            let sv2_job_dist = start_job_distributor(sv2_config.server_id);

            // Default target: difficulty 1 (all 0xff except first 4 bytes)
            let mut default_target = [0xff; 32];
            default_target[0..4].copy_from_slice(&[0x00, 0x00, 0xff, 0xff]);
            let sv2_channels = start_channel_manager(sv2_config.server_id, default_target);

            // Determine validate_addresses from donation config
            let sv2_validate_addresses =
                stratum_config.donation.unwrap_or_default() != FULL_DONATION_BIPS;

            // Build the shared context for per-connection handlers
            let sv2_ctx = Sv2ConnectionContext {
                connections: sv2_connections,
                channels: sv2_channels,
                job_distributor: sv2_job_dist,
                emissions_tx: sv2_emissions_tx,
                chain_store: chain_store_handle.clone(),
                validate_addresses: sv2_validate_addresses,
                network: stratum_config.network,
            };

            // Start the TCP accept loop for SV2
            let (handshake_tx, mut handshake_rx) = tokio::sync::mpsc::channel(64);
            tokio::spawn(async move {
                if let Err(e) = run_accept_loop(sv2_server_config, handshake_tx, shutdown_rx).await
                {
                    error!("SV2 accept loop error: {e}");
                }
            });

            // Spawn per-connection handlers as handshakes complete
            let sv2_ctx_for_loop = sv2_ctx.clone();
            tokio::spawn(async move {
                while let Some(handshake) = handshake_rx.recv().await {
                    let ctx = sv2_ctx_for_loop.clone();
                    tokio::spawn(async move {
                        handle_sv2_connection(handshake, ctx).await;
                    });
                }
            });

            info!(
                "SV2 server listening on {}:{} (Noise NX encrypted)",
                sv2_config.hostname, sv2_config.port
            );
        }
    }

    let metrics_handle = match metrics::start_metrics(config.logging.stats_dir.clone()).await {
        Ok(handle) => handle,
        Err(e) => {
            return Err(format!("Failed to start metrics: {e}"));
        }
    };
    let metrics_cloned = metrics_handle.clone();
    let metrics_for_shutdown = metrics_handle.clone();
    let stats_dir_for_shutdown = config.logging.stats_dir.clone();
    let chain_store_handle_for_stratum = chain_store_handle.clone();
    let tracker_handle_cloned = tracker_handle.clone();

    tokio::spawn(async move {
        let mut stratum_server = StratumServerBuilder::default()
            .shutdown_rx(stratum_shutdown_rx)
            .connections_handle(connections_handle.clone())
            .emissions_tx(emissions_tx)
            .hostname(stratum_config.hostname)
            .port(stratum_config.port)
            .start_difficulty(stratum_config.start_difficulty)
            .minimum_difficulty(stratum_config.minimum_difficulty)
            .maximum_difficulty(stratum_config.maximum_difficulty)
            .ignore_difficulty(stratum_config.ignore_difficulty)
            .validate_addresses(Some(
                stratum_config.donation.unwrap_or_default() != FULL_DONATION_BIPS,
            )) // 100% donation in bips, skip address validation
            .network(stratum_config.network)
            .version_mask(stratum_config.version_mask)
            .chain_store_handle(chain_store_handle_for_stratum)
            .build()
            .await
            .unwrap();
        info!("Starting Stratum server...");
        let result = stratum_server
            .start(
                None,
                notify_tx,
                tracker_handle_cloned,
                bitcoinrpc_config,
                metrics_cloned,
            )
            .await;
        if result.is_err() {
            error!("Failed to start Stratum server: {}", result.unwrap_err());
        }
        info!("Stratum server stopped");
    });

    let api_shutdown_tx = match start_api_server(
        config.api.clone(),
        chain_store_handle.clone(),
        metrics_handle.clone(),
        tracker_handle,
        stratum_config.network,
        stratum_config.pool_signature,
    )
    .await
    {
        Ok(shutdown_tx) => shutdown_tx,
        Err(e) => {
            info!("Error starting server: {}", e);
            return Err("Failed to start API Server. Quitting.".into());
        }
    };
    info!(
        "API server started on host {} port {}",
        config.api.hostname, config.api.port
    );

    match NodeHandle::new(config, chain_store_handle, emissions_rx, metrics_handle).await {
        Ok((node_handle, stopping_rx)) => {
            info!("Node started");

            wait_for_shutdown_signal(stopping_rx).await;

            info!("Node shutting down ...");

            // Shutdown node first to stop accepting new work
            if let Err(e) = node_handle.shutdown().await {
                error!("Error during node shutdown: {e}");
            }

            // Save metrics before shutdown to prevent data loss
            let metrics = metrics_for_shutdown.get_metrics().await;
            if let Err(e) = p2poolv2_lib::accounting::stats::pool_local_stats::save_pool_local_stats(
                &metrics,
                &stats_dir_for_shutdown,
            ) {
                error!("Failed to save metrics on shutdown: {e}");
            } else {
                info!("Metrics saved on shutdown");
            }

            stratum_shutdown_tx
                .send(())
                .expect("Failed to send shutdown signal to Stratum server");

            if let Some(sv2_tx) = _sv2_shutdown_tx.take() {
                let _ = sv2_tx.send(());
                info!("SV2 server stopped");
            }

            api_shutdown_tx
                .send(())
                .expect("Failed to send shutdown signal to API server");

            info!("Node stopped");
        }
        Err(e) => {
            error!("Failed to start node: {e}");
            return Err(format!("Failed to start node: {e}"));
        }
    }
    Ok(())
}
