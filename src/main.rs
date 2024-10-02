//  Copyright (C) 2021-2023 Chronicle Labs, Inc.
//
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU Affero General Public License as
//  published by the Free Software Foundation, either version 3 of the
//  License, or (at your option) any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU Affero General Public License for more details.
//
//  You should have received a copy of the GNU Affero General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.

use alloy::primitives::Address;
use alloy::providers::fillers::ChainIdFiller;
use alloy::providers::ProviderBuilder;
use alloy::rpc::client::ClientBuilder;
use alloy::transports::layers::RetryBackoffLayer;
use clap::Parser;
use env_logger::Env;

use eyre::Result;
use log::{error, info};
use scribe::contract::EventWithMetadata;
use scribe::events_listener::Poller;
use scribe::metrics;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{env, panic, path::PathBuf, time::Duration};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use scribe::event_handler;

mod wallet;

use tokio::task::JoinSet;
use tokio::{select, signal};

use wallet::{CustomWallet, KeystoreWallet, PrivateKeyWallet};

use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_process::Collector;

#[derive(Parser, Debug)]
#[command(author, version, about)]
/// Challenger searches for `opPoked` events for `ScribeOptimistic` contract.
/// Verifies poke schnorr signature and challenges it, if it's invalid.
struct Cli {
    #[arg(
        short = 'a',
        long,
        help = "ScribeOptimistic contract addresses. Example: `0x891E368fE81cBa2aC6F6cc4b98e684c106e2EF4f`"
    )]
    addresses: Vec<String>,

    #[arg(long, help = "Node HTTP RPC_URL, normally starts with https://****")]
    rpc_url: String,

    #[arg(
        long = "secret-key",
        help = "Private key in format `0x******` or `*******`. If provided, no need to use --keystore"
    )]
    raw_secret_key: Option<String>,

    #[arg(
        long = "keystore",
        env = "ETH_KEYSTORE",
        help = "Keystore file (NOT FOLDER), path to key .json file. If provided, no need to use --secret-key"
    )]
    keystore_path: Option<PathBuf>,

    #[arg(
        long = "password",
        requires = "keystore_path",
        help = "Key raw password as text"
    )]
    raw_password: Option<String>,

    #[arg(
        long,
        requires = "keystore_path",
        env = "ETH_PASSWORD",
        help = "Path to key password file"
    )]
    password_file: Option<PathBuf>,

    #[arg(
        long,
        help = "If no chain_id provided binary will try to get chain_id from given RPC"
    )]
    chain_id: Option<u64>,
}

impl PrivateKeyWallet for Cli {
    fn raw_private_key(&self) -> Option<String> {
        self.raw_secret_key.clone()
    }
}

impl KeystoreWallet for Cli {
    fn keystore_path(&self) -> Option<PathBuf> {
        self.keystore_path.clone()
    }

    fn raw_password(&self) -> Option<String> {
        self.raw_password.clone()
    }

    fn password_file(&self) -> Option<PathBuf> {
        self.password_file.clone()
    }
}

impl CustomWallet for Cli {}

#[tokio::main]
async fn main() -> Result<()> {
    // Setting default log level to info
    env_logger::Builder::from_env(Env::default().default_filter_or("info,challenger=debug")).init();

    let args = Cli::parse();

    log::info!("Using RPC URL: {:?}", &args.rpc_url);

    // Building tx signer for provider
    let signer = args.wallet()?.unwrap();
    info!(
        "Using {:?} for signing transactions.",
        signer.default_signer().address()
    );

    // Create new HTTP client with retry backoff layer
    let client = ClientBuilder::default()
        .layer(RetryBackoffLayer::new(15, 200, 300))
        .http(args.rpc_url.parse()?);

    let provider = Arc::new(
        ProviderBuilder::new()
            // Add gas automatic gas field completion
            .with_recommended_fillers()
            // Add chain id request from rpc
            .filler(ChainIdFiller::new(args.chain_id))
            // Add default signer
            .wallet(signer.clone())
            .on_client(client),
    );

    // Create new HTTP client for flashbots
    let flashbot_client = ClientBuilder::default()
        .layer(RetryBackoffLayer::new(15, 200, 300))
        .http(args.rpc_url.parse()?);

    let flashbot_provider = Arc::new(
        ProviderBuilder::new()
            // Add gas automatic gas field completion
            .with_recommended_fillers()
            // Add chain id request from rpc
            .filler(ChainIdFiller::new(args.chain_id))
            // Add default signer
            .wallet(signer.clone())
            .on_client(flashbot_client),
    );


    let mut set = JoinSet::new();
    let cancel_token = CancellationToken::new();

    // Removing duplicates from list of provided addresses
    let mut addresses = args.addresses;
    addresses.dedup();
    let addresses: Vec<Address> = addresses
        .iter()
        .map(|a| a.parse().unwrap())
        .collect::<Vec<_>>();

    // Register Prometheus metrics
    let builder = PrometheusBuilder::new();

    let port = env::var("HTTP_PORT")
        .unwrap_or(String::from("9090"))
        .parse::<u16>()
        .unwrap();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let _ = builder.with_http_listener(addr).install();

    // .expect("failed to install Prometheus recorder");

    log::info!("Starting Prometheus metrics collector on port: {}", port);

    // Add challenger metrics description
    metrics::describe();

    // Add Prometheus metrics help for process metrics
    let collector = Collector::new("challenger_");
    collector.describe();

    let (tx, rx) = tokio::sync::mpsc::channel::<EventWithMetadata>(100);

    // Create events listener
    let mut poller = Poller::new(
        addresses.clone(),
        cancel_token.clone(),
        provider.clone(),
        tx.clone(),
    );

    // Create event handler
    let mut event_distributor = event_handler::EventDistributor::new(
        addresses.clone(),
        cancel_token.clone(),
        provider.clone(),
        flashbot_provider.clone(),
        rx,
    );

    // Run events listener process
    set.spawn(async move {
        log::info!("Starting events listener");
        if let Err(err) = poller.start().await {
            log::error!("Poller error: {:?}", err);
        }
    });

    // Run event handler process
    set.spawn(async move {
        log::info!("Starting log handler");
        if let Err(err) = event_distributor.start().await {
            log::error!("Log Handler error: {:?}", err);
        }
    });

    // Run metrics collector process
    let metrics_cancelation_token = cancel_token.clone();
    set.spawn(async move {
        let duration = Duration::from_millis(750);
        log::info!("Starting metrics collector");

        loop {
            select! {
                _ = metrics_cancelation_token.cancelled() => {
                    log::info!("Metrics collector stopped");
                    return;
                },
                _ = sleep(duration) => {
                    collector.collect();
                }
            }
        }
    });

    // TODO: Start challenger and event verifier process

    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl-C, shutting down");
            cancel_token.cancel();
        },

        // some process terminated, no need to wait for others
        res = set.join_next() => {
            match res.unwrap() {
                Ok(_) => {
                    info!("Task terminated without error, shutting down");
                    cancel_token.cancel();
                },
                Err(e) => {
                    error!("Task terminated with error: {:#?}", e.to_string());
                },
            }
        },
    }

    // Wait for all tasks to finish
    set.join_all().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use alloy::primitives::address;

    use super::*;

    #[test]
    fn builds_wallet_from_private_key() {
        let cli = Cli {
            addresses: vec![],
            raw_secret_key: Some(
                "def90b5b5cb2d68c5cd9de7b3e6d767cbb1b8d5fd8560bd6c42cbc4a4da30b16".to_string(),
            ),
            chain_id: None,
            keystore_path: None,
            raw_password: None,
            password_file: None,
            rpc_url: "http://localhost:8545".to_string(),
        };

        let wallet = cli.wallet().unwrap().unwrap();

        assert_eq!(
            wallet.default_signer().address(),
            address!("91543660a715018cb35918add3085d08d7194724")
        );

        // Works with `0x` prefix
        let cli = Cli {
            addresses: vec![],
            raw_secret_key: Some(
                "0xdef90b5b5cb2d68c5cd9de7b3e6d767cbb1b8d5fd8560bd6c42cbc4a4da30b16".to_string(),
            ),
            chain_id: None,
            keystore_path: None,
            raw_password: None,
            password_file: None,
            rpc_url: "http://localhost:8545".to_string(),
        };

        let wallet = cli.wallet().unwrap().unwrap();

        assert_eq!(
            wallet.default_signer().address(),
            address!("91543660a715018cb35918add3085d08d7194724")
        );
    }

    #[test]
    fn keystore_works_with_password_in_file() {
        let keystore = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keystore");
        let keystore_file = keystore
            .join("UTC--2022-12-20T10-30-43.591916000Z--ec554aeafe75601aaab43bd4621a22284db566c2");

        let keystore_password_file =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keystore/password");

        let cli = Cli {
            addresses: vec![],
            raw_secret_key: None,
            chain_id: None,
            keystore_path: Some(keystore_file),
            raw_password: None,
            password_file: Some(keystore_password_file),
            rpc_url: "http://localhost:8545".to_string(),
        };

        let wallet = cli.wallet().unwrap().unwrap();

        assert_eq!(
            wallet.default_signer().address(),
            address!("ec554aeafe75601aaab43bd4621a22284db566c2")
        );
    }

    #[test]
    fn keystore_works_with_raw_password() {
        let keystore = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keystore");
        let keystore_file = keystore
            .join("UTC--2022-12-20T10-30-43.591916000Z--ec554aeafe75601aaab43bd4621a22284db566c2");

        let cli = Cli {
            addresses: vec![],
            raw_secret_key: None,
            chain_id: None,
            keystore_path: Some(keystore_file),
            raw_password: Some("keystorepassword".to_string()),
            password_file: None,
            rpc_url: "http://localhost:8545".to_string(),
        };

        let wallet = cli.wallet().unwrap().unwrap();

        assert_eq!(
            wallet.default_signer().address(),
            address!("ec554aeafe75601aaab43bd4621a22284db566c2")
        );
    }
}
