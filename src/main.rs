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
use std::sync::Arc;
use std::{env, panic};

mod wallet;

use challenger_lib::{contract::HttpScribeOptimisticProvider, metrics::ERRORS_COUNTER};
use challenger_lib::{metrics, Challenger};

use tokio::signal;
use tokio::task::JoinSet;

use wallet::{CustomWallet, KeystoreWallet, PrivateKeyWallet};
use warp::{reject::Rejection, reply::Reply, Filter};

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
        long,
        help = "Private key in format `0x******` or `*******`. If provided, no need to use --keystore"
    )]
    secret_key: Option<String>,
    #[arg(
        long,
        env = "ETH_KEYSTORE",
        help = "Keystore file (NOT FOLDER), path to key .json file. If provided, no need to use --secret-key"
    )]
    keystore: Option<String>,
    #[arg(long, requires = "keystore", help = "Key raw password as text")]
    password: Option<String>,
    #[arg(
        long,
        requires = "keystore",
        env = "ETH_PASSWORD",
        help = "Path to key password file"
    )]
    password_file: Option<String>,
    #[arg(
        long,
        help = "If no chain_id provided binary will try to get chain_id from given RPC"
    )]
    chain_id: Option<u64>,
}

impl PrivateKeyWallet for Cli {
    fn private_key(&self) -> Option<String> {
        self.secret_key.clone()
    }
}

impl KeystoreWallet for Cli {
    fn keystore(&self) -> Option<String> {
        self.keystore.clone()
    }

    fn password(&self) -> Option<String> {
        self.password.clone()
    }

    fn password_file(&self) -> Option<String> {
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

    for address in &addresses {
        let address = address.parse::<Address>()?;

        let client_clone = client.clone();
        set.spawn(async move {
            info!("Address {:?} starting monitoring opPokes", address);

            let contract_provider = HttpScribeOptimisticProvider::new(address, client_clone);
            let mut challenger = Challenger::new(address, contract_provider, None, None);

            let res = challenger.start().await;
            // Check and add error into metrics
            if res.is_err() {
                ERRORS_COUNTER
                    .with_label_values(&[
                        &format!("{:?}", address),
                        &format!("{:?}", signer_address),
                        &res.err().unwrap().to_string(),
                    ])
                    .inc();
            }
        });
    }

    // TODO: Start HTTP server for `:9090/metrics`
    set.spawn(async move {
        metrics::register_custom_metrics();

        let metrics_route = warp::path!("metrics").and_then(metrics_handle);
        let health_route = warp::path!("health").map(|| warp::reply::json(&"OK"));

        let port = env::var("HTTP_PORT")
            .unwrap_or(String::from("9090"))
            .parse::<u16>()
            .unwrap();

        info!("Starting HTTP server on port {}", port);
        warp::serve(metrics_route.or(health_route))
            .run(([0, 0, 0, 0], port))
            .await;
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

async fn metrics_handle() -> Result<impl Reply, Rejection> {
    match metrics::as_encoded_string() {
        Ok(v) => Ok(v),
        Err(e) => {
            error!("could not encode custom metrics: {}", e);
            Ok(String::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn builds_wallet_from_private_key() {
        let cli = Cli {
            addresses: vec![],
            secret_key: Some(
                "def90b5b5cb2d68c5cd9de7b3e6d767cbb1b8d5fd8560bd6c42cbc4a4da30b16".to_string(),
            ),
            chain_id: None,
            keystore: None,
            password: None,
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
            secret_key: Some(
                "0xdef90b5b5cb2d68c5cd9de7b3e6d767cbb1b8d5fd8560bd6c42cbc4a4da30b16".to_string(),
            ),
            chain_id: None,
            keystore: None,
            password: None,
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

        let keystore_password_file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/keystore/password")
            .into_os_string();

        let cli = Cli {
            addresses: vec![],
            secret_key: None,
            chain_id: None,
            keystore: Some(keystore_file.to_str().unwrap().to_string()),
            password: None,
            password_file: Some(keystore_password_file.into_string().unwrap()),
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
            secret_key: None,
            chain_id: None,
            keystore: Some(keystore_file.to_str().unwrap().to_string()),
            password: Some("keystorepassword".to_string()),
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
