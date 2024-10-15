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
use log::{ error, info };
use scribe::contract::EventWithMetadata;
use scribe::events_listener::Poller;
use scribe::metrics;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{ env, panic, path::PathBuf, time::Duration };
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use scribe::event_handler;

mod wallet;

use tokio::task::JoinSet;
use tokio::{ select, signal };

use wallet::{ CustomWallet, KeystoreWallet, PrivateKeyWallet };

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

    #[arg(long, help = "Flashbot Node HTTP RPC_URL, normally starts with https://****")]
    flashbot_rpc_url: String,

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

    #[arg(long = "password", requires = "keystore_path", help = "Key raw password as text")]
    raw_password: Option<String>,

    #[arg(
        long,
        requires = "keystore_path",
        env = "ETH_PASSWORD",
        help = "Path to key password file"
    )]
    password_file: Option<PathBuf>,

    #[arg(long, help = "If no chain_id provided binary will try to get chain_id from given RPC")]
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
    info!("Using {:?} for signing transactions.", signer.default_signer().address());

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
            .on_client(client)
    );

    // Create new HTTP client for flashbots
    // TODO add correct gas handling etc.
    let flashbot_client = ClientBuilder::default()
        .layer(RetryBackoffLayer::new(15, 200, 300))
        .http(args.flashbot_rpc_url.parse()?);

    let flashbot_provider = Arc::new(
        ProviderBuilder::new()
            // Add gas automatic gas field completion
            .with_recommended_fillers()
            // Add chain id request from rpc
            .filler(ChainIdFiller::new(args.chain_id))
            // Add default signer
            .wallet(signer.clone())
            .on_client(flashbot_client)
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

    let port = env::var("HTTP_PORT").unwrap_or(String::from("9090")).parse::<u16>().unwrap();

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
        30
    );

    // Create event distributor
    let mut event_distributor = event_handler::EventDistributor::new(
        addresses.clone(),
        cancel_token.clone(),
        provider.clone(),
        flashbot_provider.clone(),
        rx
    );

    // Run events listener process
    set.spawn(async move {
        log::info!("Starting events listener");
        if let Err(err) = poller.start().await {
            log::error!("Poller error: {:?}", err);
        }
    });

    // Run event distributor process
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
            flashbot_rpc_url: "http://localhost:8545".to_string(),
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
            flashbot_rpc_url: "http://localhost:8545".to_string(),
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
            flashbot_rpc_url: "http://localhost:8545".to_string(),
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
            flashbot_rpc_url: "http://localhost:8545".to_string(),
        };

        let wallet = cli.wallet().unwrap().unwrap();

        assert_eq!(
            wallet.default_signer().address(),
            address!("ec554aeafe75601aaab43bd4621a22284db566c2")
        );
    }

}

#[cfg(test)]
mod integration_tests {
    use core::panic;
    use std::future::Future;
    use std::sync::Arc;
    use std::vec;

    use alloy::dyn_abi::abi::token;
    use alloy::hex;
    use alloy::network::{ Ethereum, EthereumWallet, Network };
    use alloy::providers::ext::AnvilApi;
    use alloy::providers::fillers::{
        BlobGasFiller,
        ChainIdFiller,
        FillProvider,
        GasFiller,
        JoinFill,
        NonceFiller,
        WalletFiller,
    };
    use alloy::providers::{ Identity, Provider, ProviderBuilder, RootProvider };
    use alloy::rpc::client::ClientBuilder;
    use alloy::signers::local::PrivateKeySigner;
    use alloy::signers::SignerSync;
    use alloy::transports::http::reqwest::Url;
    use alloy::transports::http::{ Client, Http };
    use alloy::transports::layers::RetryBackoffLayer;
    use alloy::transports::Transport;
    use alloy::{ primitives::U256, sol };
    use alloy::node_bindings::{ Anvil, AnvilInstance };
    use alloy::primitives::{ Address, FixedBytes };
    use futures_util::SinkExt;
    use log::Level;
    use scribe::contract::EventWithMetadata;
    use scribe::event_handler;
    use scribe::events_listener::{ Poller, RetryProviderWithSigner };
    use tokio::task::JoinSet;
    use tokio_util::sync::CancellationToken;
    use ScribeOptimisitic::ScribeOptimisiticInstance;

    sol!(
        #[allow(missing_docs)]
        #[sol(rpc)]
        #[derive(Debug)]
        ScribeOptimisitic,
        "tests/fixtures/bytecode/ScribeOptimistic.json"
    );

    type AnvilProvider = Arc<
        FillProvider<
            JoinFill<
                JoinFill<
                    JoinFill<
                        Identity,
                        JoinFill<
                            GasFiller,
                            JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>
                        >
                    >,
                    WalletFiller<EthereumWallet>
                >,
                ChainIdFiller
            >,
            RootProvider<Http<Client>>,
            Http<Client>,
            Ethereum
        >
    >;

    // -- Tests --

    #[tokio::test]
    async fn challenge_constract() {
        // Test an invalid poke on multiple scribe instances is successfully challenged
        const NUM_SCRIBE_INSTANCES: usize = 10;
        // ------------------------------------------------------------------------------------------------------------
        let private_key = "d4cf162c2e26ff75095922ea108d516ff07bdd732f050e64ced632980f11320b";
        let (anvil, anvil_provider, signer) = create_anvil_instances(private_key).await;

        // set to a low current time for now, this avoids having stale poke error later
        anvil_provider.anvil_set_time(1000).await.expect("Failed to set time");

        // deploy scribe instance
        let mut scribes = vec![];
        let mut scribe_addresses = vec![];
        for i in 0..NUM_SCRIBE_INSTANCES {
            let scribe_optimistic = deploy_scribe(anvil_provider.clone(), signer.clone()).await;
            scribe_addresses.push(scribe_optimistic.address().clone());
            scribes.push(scribe_optimistic);
            anvil_provider
                .anvil_set_balance(
                    scribes[i].address().clone(),
                    U256::from_str_radix("1000000000000000000000000000000000000000", 10).unwrap()
                ).await
                .expect("Unable to set balance");
        }
        // Update current anvil time to be far from last scribe config update
        let current_timestamp = (chrono::Utc::now().timestamp() as u64) - 100;
        anvil_provider.anvil_set_time(current_timestamp).await.expect("Failed to set time");

        // ------------------------------------------------------------------------------------------------------------
        let cancel_token: CancellationToken = CancellationToken::new();

        // start the event listener as a sub process
        {
            let addresses = scribe_addresses.clone();
            let cancel_token = cancel_token.clone();
            let url = anvil.endpoint_url();
            let signer = signer.clone();
            tokio::spawn(async move {
                start_event_listener(url, signer, cancel_token, addresses).await;
            });
        }
        // Let first poll occur on poller
        tokio::time::sleep(tokio::time::Duration::from_millis(1200)).await;

        // Increase current block count to move away from poller intialisation block
        anvil_provider
            .anvil_mine(Some(U256::from(1)), Some(U256::from(1))).await
            .expect("Failed to mine");

        // ------------------------------------------------------------------------------------------------------------
        for i in 0..NUM_SCRIBE_INSTANCES{
             // Assert that the current contract balance is not 0
            let balance = anvil_provider
                .get_balance(scribe_addresses[i]).await
                .expect("Failed to get balance");
            assert_ne!(balance, U256::from(0));
            let current_timestamp = (chrono::Utc::now().timestamp() as u64) - 100;

            // Make invalid poke
            make_invalid_op_poke(current_timestamp,private_key, &scribes[i]).await;
        }

        // Mine at least one block to ensure log is processed
        anvil_provider
            .anvil_mine(Some(U256::from(1)), Some(U256::from(1))).await
            .expect("Failed to mine");

        let mut result = true;
        for i in 0..NUM_SCRIBE_INSTANCES{
            result &= poll_balance_is_zero(&anvil_provider, &scribe_addresses[i], 10).await;
        }

        // Poll the contract for up to 5 seconds to ensure that the challenge was successfull by querying balance
        if result {
            cancel_token.cancel();
            return;
        }
        cancel_token.cancel();
        panic!("Failed to challenge");
    }

    // test dont challenge if outside challenge period

    // test dont challenge if receive op challenged

    // test dont challenge if received new op poke

    // test flashbotnused first

    // test fallback to normal rpc

    // -- Helper functions --

    async fn create_anvil_instances(
        private_key: &str
    ) -> (AnvilInstance, AnvilProvider, EthereumWallet) {
        let anvil: AnvilInstance = Anvil::new()
            .port(8545 as u16)
            .chain_id(31337)
            .block_time_f64(0.1)
            .try_spawn()
            .expect("Failed to spawn anvil");
        // random private key
        // let private_key = "d4cf162c2e26ff75095922ea108d516ff07bdd732f050e64ced632980f11320b";
        let signer: EthereumWallet = EthereumWallet::new(
            private_key.parse::<PrivateKeySigner>().unwrap()
        );

        let anvil_provider = Arc::new(
            ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(signer.clone())
                .filler(ChainIdFiller::new(Some(31337)))
                .on_http(anvil.endpoint_url())
        );

        // set initial balance for deployer
        anvil_provider
            .anvil_set_balance(
                signer.default_signer().address(),
                U256::from_str_radix("1000000000000000000000000000000000000000", 10).unwrap()
            ).await
            .expect("Unable to set balance");

        // configure anvil settings
        anvil_provider.anvil_set_auto_mine(true).await.expect("Failed to set auto mine");
        anvil_provider
            .anvil_mine(Some(U256::from(3)), Some(U256::from(3))).await
            .expect("Failed to mine");
        anvil_provider
            .anvil_impersonate_account(signer.default_signer().address().clone()).await
            .expect("Failed to impersonate account");

        return (anvil, anvil_provider, signer);
    }

    async fn deploy_scribe<P: Provider<T, N>, T: Clone + Transport, N: Network>(
        provider: P,
        signer: EthereumWallet
    ) -> ScribeOptimisiticInstance<T, P, N> {
        // deploy scribe instance
        let initial_authed = signer.default_signer().address();
        let wat: [u8; 32] = hex!(
            "
            0123456789abcdef0123456789abcdef
            0123456789abcdef0123456789abcdef
        "
        );
        let scribe_optimistic = ScribeOptimisitic::deploy(
            provider,
            initial_authed.clone(),
            FixedBytes(wat).clone()
        );
        let scribe_optimistic = scribe_optimistic.await.unwrap();

        let receipt = scribe_optimistic.setBar(1);
        let receipt = receipt.send().await.expect("Failed to set bar");
        receipt.watch().await.expect("Failed to set bar");

        // lift validator
        let pub_key = LibSecp256k1::Point {
            x: U256::from_str_radix(
                "95726579611468854699048782904089382286224374897874075137780214269565012360365",
                10
            ).unwrap(),
            y: U256::from_str_radix(
                "95517337328947037046967076057450300670379811052080651187456799621439597083272",
                10
            ).unwrap(),
        };
        let ecdsa_data = IScribe::ECDSAData {
            v: 0x1b,
            r: FixedBytes(hex!("0ced9fd231ad454eaac301d6e15a56b6aaa839a55d664757e3ace927e95948ec")),
            s: FixedBytes(hex!("21b2813ad85945f320d7728fbfc9b83cbbb564135e67c16db16d5f4e74392119")),
        };
        let receipt = scribe_optimistic.lift_0(pub_key, ecdsa_data);
        let receipt = receipt.send().await.expect("Failed to lift validator");
        receipt.watch().await.expect("Failed to lift validator");

        // set challenge period
        let receipt = scribe_optimistic.setOpChallengePeriod(300);
        let receipt = receipt.send().await.expect("Failed to lift validator");
        receipt.watch().await.expect("Failed to set opChallenge");

        return scribe_optimistic;
    }

    async fn start_event_listener(
        url: Url,
        signer: EthereumWallet,
        cancel_token: CancellationToken,
        addresses: Vec<Address>
    ) {
        let client = ClientBuilder::default().layer(RetryBackoffLayer::new(15, 200, 300)).http(url);

        let provider = Arc::new(
            ProviderBuilder::new()
                .with_recommended_fillers()
                .filler(ChainIdFiller::new(Some(31337)))
                .wallet(signer.clone())
                .on_client(client)
        );

        let flashbot_provider = provider.clone();

        let mut set: JoinSet<()> = JoinSet::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<EventWithMetadata>(100);

        // let addresses = vec![scribe_optimisitic.address().clone()];

        // Create events listener
        let mut poller = Poller::new(
            addresses.clone(),
            cancel_token.clone(),
            provider.clone(),
            tx.clone(),
            1
        );

        // Create event distributor
        let mut event_distributor = event_handler::EventDistributor::new(
            addresses.clone(),
            cancel_token.clone(),
            provider.clone(),
            flashbot_provider.clone(),
            rx
        );

        // Run events listener process
        set.spawn(async move {
            log::info!("Starting events listener");
            if let Err(err) = poller.start().await {
                log::error!("Poller error: {:?}", err);
            }
        });

        // Run event distributor process
        set.spawn(async move {
            log::info!("Starting log handler");
            if let Err(err) = event_distributor.start().await {
                log::error!("Log Handler error: {:?}", err);
            }
        });

        while let Some(res) = set.join_next().await {
            match res {
                Ok(_) => log::info!("Task completed successfully"),
                Err(err) => log::error!("Task failed: {:?}", err),
            }
        }
    }

    async fn poll_balance_is_zero(
        anvil_provider: &AnvilProvider,
        address: &Address,
        timeout: u64
    ) -> bool {
        let start_time = chrono::Utc::now().timestamp() as u64;
        while (chrono::Utc::now().timestamp() as u64) < start_time + timeout {
            let balance = anvil_provider
                .get_balance(address.clone()).await
                .expect("Failed to get balance");
            if balance == U256::from(0) {
                return true;
            }
            anvil_provider
                .anvil_mine(Some(U256::from(1)), Some(U256::from(1))).await
                .expect("Failed to mine");
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
        return false;
    }

    async fn make_invalid_op_poke(
        age : u64,
        private_key: &str,
        scribe_optimisitic: &ScribeOptimisiticInstance<Http<Client>, AnvilProvider, Ethereum>
    ) {
        let poke_data: IScribe::PokeData = IScribe::PokeData {
            val: 10,
            age: age as u32,
        };
        let schnorr_data = IScribe::SchnorrData {
            signature: FixedBytes(
                hex!("0000000000000000000000000000000000000000000000000000000000000000")
            ),
            commitment: alloy::primitives::Address::ZERO.clone(),
            feedIds: hex!("00").into(),
        };
        let op_poke_message = scribe_optimisitic
            .constructOpPokeMessage(poke_data.clone(), schnorr_data.clone())
            .call().await
            .expect("Failed to read current age");
        let op_poke_message = op_poke_message._0;
        let ecdsa_signer = private_key
            .parse::<PrivateKeySigner>()
            .expect("Failed to parse private key");
        let signature = ecdsa_signer
            .sign_hash_sync(&op_poke_message)
            .expect("Failed to sign message");
        let ecdsa_data = IScribe::ECDSAData {
            v: 0x1b + (signature.v().to_u64() as u8),
            r: FixedBytes(signature.r().to_be_bytes()),
            s: FixedBytes(signature.s().to_be_bytes()),
        };

        // Make the invalid poke
        let receipt = scribe_optimisitic.opPoke(
            poke_data.clone(),
            schnorr_data.clone(),
            ecdsa_data
        );
        let receipt = receipt.send().await.expect("Failed to send op poke");
        receipt.watch().await.expect("Failed to watch op poke");
    }
}
