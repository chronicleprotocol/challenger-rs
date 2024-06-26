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

use clap::Parser;
use env_logger::Env;
use ethers::{
    core::types::Address,
    prelude::SignerMiddleware,
    providers::{Http, Middleware, Provider},
    signers::Signer,
};
use eyre::Result;
use log::{error, info};
use std::{env, panic, path::PathBuf, time::Duration};
use std::{net::SocketAddr, sync::Arc};

mod wallet;

use challenger_lib::{contract::HttpScribeOptimisticProvider, metrics, Challenger};

use tokio::signal;
use tokio::{task::JoinSet, time};

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
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let args = Cli::parse();

    let provider = Provider::<Http>::try_from(args.rpc_url.as_str())?;

    info!("Connected to {:?}", provider.url());
    let chain_id = args
        .chain_id
        .unwrap_or(provider.get_chainid().await?.as_u64());

    info!("Chain id: {:?}", chain_id);
    // Generating signer from given private key
    let signer = args.wallet()?.unwrap().with_chain_id(chain_id);

    info!(
        "Using {:?} for signing and chain_id {:?}",
        signer.address(),
        signer.chain_id()
    );

    let signer_address = signer.address();
    let client = Arc::new(SignerMiddleware::new(provider, signer));

    let mut set = JoinSet::new();

    // Removing duplicates from list of provided addresses
    let mut addresses = args.addresses;
    addresses.dedup();

    // Register Prometheus metrics
    let builder = PrometheusBuilder::new();

    let port = env::var("HTTP_PORT")
        .unwrap_or(String::from("9090"))
        .parse::<u16>()
        .unwrap();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    builder
        .with_http_listener(addr)
        .install()
        .expect("failed to install Prometheus recorder");

    // Add challenger metrics description
    metrics::describe();

    let collector = Collector::new("challenger_");
    // Add Prometheus metrics help for process metrics
    collector.describe();

    // Start processing per address
    for address in &addresses {
        let address = address.parse::<Address>()?;

        let client_clone = client.clone();
        set.spawn(async move {
            info!("[{:?}] starting monitoring opPokes", address);

            let contract_provider = HttpScribeOptimisticProvider::new(address, client_clone);
            let mut challenger = Challenger::new(address, contract_provider, None, None);

            let res = challenger.start().await;
            // Check and add error into metrics
            if res.is_err() {
                // Increment error counter
                metrics::inc_errors_counter(
                    address,
                    signer_address,
                    &res.err().unwrap().to_string(),
                );
            }
        });
    }

    // Run metrics collector process
    set.spawn(async move {
        let mut interval = time::interval(Duration::from_millis(750));

        loop {
            collector.collect();
            interval.tick().await;
        }
    });

    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl-C, shutting down");
        },

        // some process terminated, no need to wait for others
        res = set.join_next() => {
            match res.unwrap() {
                Ok(_) => info!("Task terminated without error, shutting down"),
                Err(e) => {
                    error!("Task terminated with error: {:#?}", e.to_string());
                },
            }
        },
    }

    // Terminating all remaining tasks
    set.shutdown().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
            wallet.address(),
            "91543660a715018cb35918add3085d08d7194724".parse().unwrap()
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
            wallet.address(),
            "91543660a715018cb35918add3085d08d7194724".parse().unwrap()
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
            wallet.address(),
            "ec554aeafe75601aaab43bd4621a22284db566c2".parse().unwrap()
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
            wallet.address(),
            "ec554aeafe75601aaab43bd4621a22284db566c2".parse().unwrap()
        );
    }
}
