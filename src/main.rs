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

use alloy::{
  primitives::Address,
  providers::{fillers::ChainIdFiller, ProviderBuilder},
  rpc::client::ClientBuilder,
  transports::layers::RetryBackoffLayer,
};
use clap::Parser;
use env_logger::Env;
use eyre::Result;
use log::{error, info, warn};
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_process::Collector;
use scribe::{
  contract::ScribeContractInstance, metrics, provider::EthereumPollProvider, Event, PollerBuilder,
  ScribeEventsProcessor,
};
use std::{
  collections::HashMap, env, net::SocketAddr, panic, path::PathBuf, sync::Arc, time::Duration,
};
use tokio::{select, signal, sync::mpsc::Sender, task::JoinSet, time::sleep};
use tokio_util::sync::CancellationToken;

mod wallet;
use wallet::{CustomWallet, KeystoreWallet, PrivateKeyWallet};

// Constants for retry backoff configuration
const RETRY_BACKOFF_MAX_ATTEMPTS: u32 = 15;
const RETRY_BACKOFF_INITIAL_MS: u64 = 200;
const RETRY_BACKOFF_MAX_MS: u64 = 300;

// Constants for timing and metrics
const DEFAULT_METRICS_PORT: &str = "9090";
const POLL_INTERVAL_SECS: u64 = 30;
const METRICS_COLLECT_INTERVAL_MS: u64 = 750;

/// CLI interface for the challenger.
#[derive(Parser, Debug)]
#[command(author, version, about)]
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
    help = "Flashbot Node HTTP RPC_URL, like https://rpc-sepolia.flashbots.net/fast"
  )]
  flashbot_rpc_url: Option<String>,

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

  #[arg(long, help = "Block number to start from")]
  from_block: Option<u64>,

  #[arg(
    long,
    help = "Maximum number of blocks to fetch per RPC request. Splits wide ranges into multiple calls."
  )]
  max_block_range: Option<u64>,
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
  env_logger::Builder::from_env(
    Env::default().default_filter_or("info,challenger=debug,scribe=trace"),
  )
  .init();

  let args = Cli::parse();

  info!("Using RPC URL: {:?}", &args.rpc_url);

  // Building tx signer for provider
  let signer = args
    .wallet()?
    .ok_or_else(|| eyre::eyre!("No wallet credentials provided"))?;
  info!(
    "Using {:?} for signing transactions.",
    signer.default_signer().address()
  );

  // Create new HTTP client with retry backoff layer
  let client = ClientBuilder::default()
    .layer(RetryBackoffLayer::new(
      RETRY_BACKOFF_MAX_ATTEMPTS,
      RETRY_BACKOFF_INITIAL_MS,
      RETRY_BACKOFF_MAX_MS,
    ))
    .http(args.rpc_url.parse()?);

  let provider = Arc::new(
    ProviderBuilder::new()
      // Add chain id request from rpc
      .filler(ChainIdFiller::new(args.chain_id))
      // Add default signer
      .wallet(signer.clone())
      .connect_client(client),
  );

  // Genrerating flashbot provider if provided
  let flashbot_provider = match args.flashbot_rpc_url {
    None => None,
    Some(url) => {
      // Create new HTTP client for flashbots
      let flashbot_client = ClientBuilder::default()
        .layer(RetryBackoffLayer::new(
          RETRY_BACKOFF_MAX_ATTEMPTS,
          RETRY_BACKOFF_INITIAL_MS,
          RETRY_BACKOFF_MAX_MS,
        ))
        .http(url.parse()?);

      Some(Arc::new(
        ProviderBuilder::new()
          // Add chain id request from rpc
          .filler(ChainIdFiller::new(args.chain_id))
          // Add default signer
          .wallet(signer.clone())
          .connect_client(flashbot_client),
      ))
    }
  };

  let mut set: JoinSet<Result<()>> = JoinSet::new();
  let cancellation_token = CancellationToken::new();

  // Removing duplicates from list of provided addresses
  let mut addresses = args.addresses;
  addresses.dedup();
  let addresses: Vec<Address> = addresses
    .iter()
    .map(|a| {
      a.parse::<Address>()
        .map_err(|e| eyre::eyre!("Invalid address '{}': {}", a, e))
    })
    .collect::<Result<Vec<_>, _>>()?;

  // Register Prometheus metrics
  let builder = PrometheusBuilder::new();

  let port = env::var("HTTP_PORT")
    .unwrap_or_else(|_| DEFAULT_METRICS_PORT.to_string())
    .parse::<u16>()
    .unwrap_or_else(|e| {
      warn!(
        "Invalid HTTP_PORT value, using default {}: {}",
        DEFAULT_METRICS_PORT, e
      );
      DEFAULT_METRICS_PORT
        .parse()
        .expect("DEFAULT_METRICS_PORT constant is invalid")
    });
  let addr = SocketAddr::from(([0, 0, 0, 0], port));

  builder
    .with_http_listener(addr)
    .install()
    .expect("failed to install Prometheus recorder");

  log::info!("Starting Prometheus metrics collector on port: {}", port);

  // Add challenger metrics description
  metrics::describe();

  // Add Prometheus metrics help for process metrics
  let collector = Collector::new("challenger_");
  collector.describe();

  let mut processors: HashMap<Address, Sender<Event>> = HashMap::new();

  for address in addresses.iter() {
    let scribe_contract = ScribeContractInstance::new(
      *address,
      provider.clone(),
      flashbot_provider.clone(),
      signer.default_signer().address(),
    );

    // Create event processor for each address
    let (mut event_processor, tx) =
      ScribeEventsProcessor::new(scribe_contract, cancellation_token.clone(), None);

    // Run event distributor process
    set.spawn(async move {
      if let Err(e) = event_processor.start().await {
        log::error!("Event processor error: {:#?}", e);
        return Err(e.into());
      }
      Ok(())
    });

    // Storing event processor channel to send events to it.
    processors.insert(*address, tx);
  }

  // Create new poller
  let mut poller = PollerBuilder::builder()
    .with_signer_address(signer.default_signer().address())
    .with_handler_channels(processors)
    .with_cancellation_token(cancellation_token.clone())
    .with_from_block(args.from_block)
    .with_max_block_range(args.max_block_range)
    .with_poll_interval(Duration::from_secs(POLL_INTERVAL_SECS))
    .build(EthereumPollProvider::new(provider.clone()));

  // Run events poller process
  set.spawn(async move {
    log::info!("Starting events poller");
    if let Err(err) = poller.start().await {
      log::error!("Poller error: {:#?}", err);
      // Poller failure is critical - return error to trigger shutdown
      return Err(err.into());
    }
    Ok(())
  });

  // Run metrics collector process
  let metrics_cancelation_token = cancellation_token.clone();
  set.spawn(async move {
    let duration = Duration::from_millis(METRICS_COLLECT_INTERVAL_MS);
    log::info!("Starting metrics collector");

    loop {
      select! {
          _ = metrics_cancelation_token.cancelled() => {
              log::info!("Metrics collector stopped");
              return Ok(());
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
          cancellation_token.cancel();
      },

      // some process terminated, no need to wait for others
      res = set.join_next() => {
          match res {
              Some(Ok(Ok(_))) => {
                  info!("Task terminated successfully, shutting down");
                  cancellation_token.cancel();
              },
              Some(Ok(Err(e))) => {
                  error!("Task returned error: {:#?}", e);
                  cancellation_token.cancel();
              },
              Some(Err(e)) => {
                  error!("Task panicked: {:#?}", e.to_string());
                  cancellation_token.cancel();
              },
              None => {
                  info!("All tasks completed");
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
      flashbot_rpc_url: Some("http://localhost:8545".to_string()),
      from_block: None,
      max_block_range: None,
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
      flashbot_rpc_url: Some("http://localhost:8545".to_string()),
      from_block: None,
      max_block_range: None,
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
      flashbot_rpc_url: Some("http://localhost:8545".to_string()),
      from_block: None,
      max_block_range: None,
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
      flashbot_rpc_url: Some("http://localhost:8545".to_string()),
      from_block: None,
      max_block_range: None,
    };

    let wallet = cli.wallet().unwrap().unwrap();

    assert_eq!(
      wallet.default_signer().address(),
      address!("ec554aeafe75601aaab43bd4621a22284db566c2")
    );
  }
}
