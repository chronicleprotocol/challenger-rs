use core::panic;
use std::{cell::Cell, collections::HashMap, sync::Arc, time::Duration, vec};

use alloy::{
  hex,
  network::{Ethereum, EthereumWallet, Network},
  node_bindings::{Anvil, AnvilInstance},
  primitives::{Address, FixedBytes, U256},
  providers::{
    ext::AnvilApi,
    fillers::{
      BlobGasFiller, CachedNonceManager, ChainIdFiller, FillProvider, GasFiller, JoinFill,
      NonceFiller, WalletFiller,
    },
    Identity, Provider, ProviderBuilder, RootProvider,
  },
  rpc::client::ClientBuilder,
  signers::{local::PrivateKeySigner, SignerSync},
  transports::{
    http::{reqwest::Url, Client, Http},
    layers::RetryBackoffLayer,
    Transport,
  },
};
use futures_util::future::join_all;
use scribe::{
  contract::ScribeContractInstance, provider::EthereumPollProvider, Event, Poller,
  ScribeEventsProcessor,
};
use scribe_optimistic::{
  IScribe, LibSecp256k1, ScribeOptimistic, ScribeOptimistic::ScribeOptimisticInstance,
};
use tokio::{sync::mpsc::Sender, task::JoinSet};
use tokio_util::sync::CancellationToken;

// In a separate module due to problems with autoformatter
#[rustfmt::skip]
mod scribe_optimistic {
  use alloy::sol;

  sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[derive(Debug)]
    ScribeOptimistic,
    "tests/fixtures/bytecode/ScribeOptimistic.json"
  );
}

type AnvilProvider = Arc<
  FillProvider<
    JoinFill<
      JoinFill<
        JoinFill<
          JoinFill<
            Identity,
            JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
          >,
          WalletFiller<EthereumWallet>,
        >,
        ChainIdFiller,
      >,
      NonceFiller<CachedNonceManager>,
    >,
    RootProvider<Http<Client>>,
    Http<Client>,
    Ethereum,
  >,
>;

const PRIVATE_KEY: &str = "d4cf162c2e26ff75095922ea108d516ff07bdd732f050e64ced632980f11320b";

// -- Tests --

#[tokio::test]
async fn challenge_contract() {
  // Test an invalid poke on multiple scribe instances in parallel are successfully challenged
  const NUM_SCRIBE_INSTANCES: usize = 100;
  const CHALLENGE_PERIOD: u16 = 1000;

  // ------------------------------------------------------------------------------------------------------------
  let private_key = PRIVATE_KEY;
  let (anvil, anvil_provider, signer) = create_anvil_instances(private_key, 8545).await;

  // set to a low current time for now, this avoids having stale poke error later
  anvil_provider
    .anvil_set_time(1000)
    .await
    .expect("Failed to set time");

  let mut deployments = vec![];
  let mut scribes = vec![];
  for i in 0..NUM_SCRIBE_INSTANCES {
    deployments.push(deploy_scribe(
      anvil_provider.clone(),
      signer.clone(),
      CHALLENGE_PERIOD,
    ));
    // Only deploy at most 20 in parallel
    if i % 20 == 0 {
      scribes.append(&mut join_all(deployments).await);
      deployments = vec![];
    }
  }
  scribes.append(&mut join_all(deployments).await);
  let mut scribe_addresses = vec![];

  let mut balance_updates = vec![];
  for scribe_optimistic in scribes.iter().take(NUM_SCRIBE_INSTANCES) {
    scribe_addresses.push(*scribe_optimistic.address());
    balance_updates.push(anvil_provider.anvil_set_balance(
      *scribe_optimistic.address(),
      U256::from_str_radix("1000000000000000000000000000000000000000", 10).unwrap(),
    ));
  }
  for balance_update in join_all(balance_updates).await {
    balance_update.expect("Unable to set balance");
  }

  // Update current anvil time to be far from last scribe config update
  // The more scribe instances the further back in time a the anvil block timestamp doesn't stay in sync with chrono
  // TODO add challenge period variable
  let current_timestamp = (chrono::Utc::now().timestamp() as u64) - (CHALLENGE_PERIOD as u64) + 1;
  anvil_provider
    .anvil_set_time(current_timestamp)
    .await
    .expect("Failed to set time");

  // ------------------------------------------------------------------------------------------------------------
  let cancel_token: CancellationToken = CancellationToken::new();

  // start the event listener as a sub process
  {
    let signer: EthereumWallet = EthereumWallet::new(PrivateKeySigner::random());
    anvil_provider
      .anvil_set_balance(
        signer.default_signer().address(),
        U256::from_str_radix("1000000000000000000000000000000000000000", 10).unwrap(),
      )
      .await
      .expect("Unable to set balance");
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

  // Increase current block count to move away from poller initialisation block
  anvil_provider
    .anvil_mine(Some(U256::from(1)), Some(U256::from(1)))
    .await
    .expect("Failed to mine");

  // ------------------------------------------------------------------------------------------------------------
  let mut invalid_pokes = vec![];
  for i in 0..NUM_SCRIBE_INSTANCES {
    // Assert that the current contract balance is not 0
    let balance = anvil_provider
      .get_balance(scribe_addresses[i])
      .await
      .expect("Failed to get balance");
    assert_ne!(balance, U256::from(0));
    invalid_pokes.push(make_invalid_op_poke(
      current_timestamp,
      private_key,
      &scribes[i],
    ));
    // Do at most 50 at the same time
    if i % 50 == 0 {
      join_all(invalid_pokes).await;
      invalid_pokes = vec![];
    }
  }
  join_all(invalid_pokes).await;

  // Mine at least one block to ensure log is processed
  anvil_provider
    .anvil_mine(Some(U256::from(1)), Some(U256::from(1)))
    .await
    .expect("Failed to mine");

  let result = join_all((0..NUM_SCRIBE_INSTANCES).map(|i| {
    poll_balance_is_zero(
      &anvil_provider,
      &scribe_addresses[i],
      (20 + (10 * NUM_SCRIBE_INSTANCES) / 100).try_into().unwrap(),
    )
  }))
  .await
  .iter()
  .all(|&result| result);

  // Poll to check that the challenge started log is found,
  // (to ensure log hasn't been changed as its non appearance is looked for in other tests)
  for i in 0..5 {
    let success: Cell<bool> = Cell::new(false);
    testing_logger::validate(|captured_logs| {
      let mut found: bool = false;
      for log in captured_logs {
        found |= log
          .body
          .to_lowercase()
          .contains(&"OpPoked validation started".to_lowercase());
      }
      success.set(found);
    });
    if success.get() {
      break;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    if i == 4 {
      panic!("Failed to find log");
    }
  }

  if result {
    cancel_token.cancel();
    return;
  }
  cancel_token.cancel();
  panic!("Failed to challenge");
}

#[tokio::test]
async fn dont_challenge_outside_challenge_period() {
  testing_logger::setup();
  // Test an invalid poke on multiple scribe instances is successfully challenged
  // ------------------------------------------------------------------------------------------------------------
  let private_key = PRIVATE_KEY;
  // Use a new port for each test to avoid conflicts if tests run in parallel
  let (anvil, anvil_provider, signer) = create_anvil_instances(private_key, 8546).await;

  // Set to a low current time for now, this avoids having stale poke error later
  anvil_provider
    .anvil_set_time(1000)
    .await
    .expect("Failed to set time");

  // Deploy scribe instance
  let scribe_optimistic = deploy_scribe(anvil_provider.clone(), signer.clone(), 300).await;
  anvil_provider
    .anvil_set_balance(
      *scribe_optimistic.address(),
      U256::from_str_radix("1000000000000000000000000000000000000000", 10).unwrap(),
    )
    .await
    .expect("Unable to set balance");

  // Update current anvil time to be far from last scribe config update
  // Set the anvil time to be in the past to ensure the challenge period is exceeded later
  let current_timestamp = (chrono::Utc::now().timestamp() as u64) - 400;
  anvil_provider
    .anvil_set_time(current_timestamp)
    .await
    .expect("Failed to set time");

  // ------------------------------------------------------------------------------------------------------------
  let cancel_token: CancellationToken = CancellationToken::new();

  // start the event listener as a sub process
  {
    let signer: EthereumWallet = EthereumWallet::new(PrivateKeySigner::random());
    anvil_provider
      .anvil_set_balance(
        signer.default_signer().address(),
        U256::from_str_radix("1000000000000000000000000000000000000000", 10).unwrap(),
      )
      .await
      .expect("Unable to set balance");
    let addresses = vec![*scribe_optimistic.address()];
    let cancel_token = cancel_token.clone();
    let url = anvil.endpoint_url();
    let signer = signer.clone();
    tokio::spawn(async move {
      start_event_listener(url, signer, cancel_token, addresses).await;
    });
  }
  // Let first poll occur on poller
  tokio::time::sleep(tokio::time::Duration::from_millis(1200)).await;

  // Increase current block count to move away from poller initialisation block
  anvil_provider
    .anvil_mine(Some(U256::from(1)), Some(U256::from(1)))
    .await
    .expect("Failed to mine");

  // ------------------------------------------------------------------------------------------------------------
  // Assert that the current contract balance is not 0
  let balance = anvil_provider
    .get_balance(*scribe_optimistic.address())
    .await
    .expect("Failed to get balance");
  assert_ne!(balance, U256::from(0));

  // Make invalid poke, but since the anvil time is not in sync with chrono time will be in the past
  let current_timestamp = chrono::Utc::now().timestamp() as u64;
  make_invalid_op_poke(current_timestamp - 500, private_key, &scribe_optimistic).await;

  // Mine at least one block to ensure log is processed
  anvil_provider
    .anvil_mine(Some(U256::from(1)), Some(U256::from(1)))
    .await
    .expect("Failed to mine");

  // Poll till expected logs are found
  // Assert challenge started log not found
  // Ensure the challenge was never created
  for _ in 1..5 {
    let success = Cell::new(false);
    testing_logger::validate(|captured_logs| {
      let mut found: bool = false;
      for log in captured_logs {
        assert!(
          !log
            .body
            .to_lowercase()
            .contains(&"Challenge started".to_lowercase()),
          "Challenge started log found"
        );
        found |= log
          .body
          .to_lowercase()
          .contains(&"outside of challenge period".to_lowercase());
      }
      success.set(found);
    });
    if success.get() {
      return;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
  }
  panic!("Failed to find log");
}

// TODO: test dont challenge if receive op challenged

// TODO: test flashbot used first, fallback to normal rpc

// -- Helper functions --

// Search for anvil binary in the target directory if `ANVIL_BIN` is set.
// Otherwise it will try get `anvil` from system.
fn get_anvil() -> Anvil {
  let anvil_bin = std::env::var("ANVIL_BIN").unwrap_or_else(|_| "anvil".to_string());
  Anvil::at(anvil_bin)
}

async fn create_anvil_instances(
  private_key: &str,
  port: u16,
) -> (AnvilInstance, AnvilProvider, EthereumWallet) {
  let anvil: AnvilInstance = get_anvil()
    .port(port)
    .chain_id(31337)
    .block_time_f64(0.1)
    .try_spawn()
    .expect("Failed to spawn anvil");

  let signer: EthereumWallet =
    EthereumWallet::new(private_key.parse::<PrivateKeySigner>().unwrap());

  let anvil_provider = Arc::new(
    ProviderBuilder::new()
      // .with_cached_nonce_management()
      .with_recommended_fillers()
      .wallet(signer.clone())
      .filler(ChainIdFiller::new(Some(31337)))
      .filler(NonceFiller::<CachedNonceManager>::default())
      .on_http(anvil.endpoint_url()),
  );

  // set initial balance for deployer
  anvil_provider
    .anvil_set_balance(
      signer.default_signer().address(),
      U256::from_str_radix("1000000000000000000000000000000000000000", 10).unwrap(),
    )
    .await
    .expect("Unable to set balance");

  // configure anvil settings
  anvil_provider
    .anvil_set_auto_mine(true)
    .await
    .expect("Failed to set auto mine");
  anvil_provider
    .anvil_mine(Some(U256::from(3)), Some(U256::from(3)))
    .await
    .expect("Failed to mine");
  anvil_provider
    .anvil_impersonate_account(signer.default_signer().address())
    .await
    .expect("Failed to impersonate account");

  (anvil, anvil_provider, signer)
}

async fn deploy_scribe<P: Provider<T, N>, T: Clone + Transport, N: Network>(
  provider: P,
  signer: EthereumWallet,
  challenge_period: u16,
) -> ScribeOptimisticInstance<T, P, N> {
  // deploy scribe instance
  let initial_authed = signer.default_signer().address();
  // let private_key = signer.
  let wat: [u8; 32] = hex!(
    "
            0123456789abcdef0123456789abcdef
            0123456789abcdef0123456789abcdef
        "
  );
  let scribe_optimistic = ScribeOptimistic::deploy(provider, initial_authed, FixedBytes(wat));

  let scribe_optimistic = scribe_optimistic.await.unwrap();
  let receipt = scribe_optimistic.setBar(1);
  let receipt = receipt.send().await.expect("Failed to set bar");
  receipt
    .with_timeout(Some(Duration::from_secs(15)))
    .watch()
    .await
    .expect("Failed to set bar");

  // TODO generate the public key and v r s from private key
  // lift validator
  let pub_key = LibSecp256k1::Point {
    x: U256::from_str_radix(
      "95726579611468854699048782904089382286224374897874075137780214269565012360365",
      10,
    )
    .unwrap(),
    y: U256::from_str_radix(
      "95517337328947037046967076057450300670379811052080651187456799621439597083272",
      10,
    )
    .unwrap(),
  };
  let ecdsa_data = IScribe::ECDSAData {
    v: 0x1b,
    r: FixedBytes(hex!(
      "0ced9fd231ad454eaac301d6e15a56b6aaa839a55d664757e3ace927e95948ec"
    )),
    s: FixedBytes(hex!(
      "21b2813ad85945f320d7728fbfc9b83cbbb564135e67c16db16d5f4e74392119"
    )),
  };
  let receipt = scribe_optimistic.lift_0(pub_key, ecdsa_data);
  let receipt = receipt.send().await.expect("Failed to lift validator");
  receipt
    .with_timeout(Some(Duration::from_secs(15)))
    .watch()
    .await
    .expect("Failed to lift validator");

  // set challenge period
  let receipt = scribe_optimistic.setOpChallengePeriod(challenge_period);
  let receipt = receipt.send().await.expect("Failed to lift validator");
  receipt
    .with_timeout(Some(Duration::from_secs(15)))
    .watch()
    .await
    .expect("Failed to set opChallenge");

  scribe_optimistic
}

async fn start_event_listener(
  url: Url,
  signer: EthereumWallet,
  cancel_token: CancellationToken,
  addresses: Vec<Address>,
) {
  let client = ClientBuilder::default()
    .layer(RetryBackoffLayer::new(15, 200, 300))
    .http(url);

  let provider = Arc::new(
    ProviderBuilder::new()
      .with_recommended_fillers()
      .filler(ChainIdFiller::new(Some(31337)))
      .wallet(signer.clone())
      .on_client(client),
  );

  let flashbot_provider = provider.clone();

  let mut set: JoinSet<()> = JoinSet::new();
  // let addresses = vec![scribe_optimistic.address().clone()];
  let mut processors: HashMap<Address, Sender<Event>> = HashMap::new();

  for address in addresses.iter() {
    let contract =
      ScribeContractInstance::new(*address, provider.clone(), Some(flashbot_provider.clone()));

    let (mut event_distributor, tx) = ScribeEventsProcessor::new(contract, cancel_token.clone());

    // Run event distributor process
    set.spawn(async move {
      event_distributor.start().await;
    });
    // Storing event processor channel to send events to it.
    processors.insert(address.clone(), tx);
  }
  // Create events listener
  let mut poller = Poller::new(
    signer.default_signer().address(),
    processors,
    cancel_token.clone(),
    EthereumPollProvider::new(provider.clone()),
    1,
    None,
  );

  // Run events listener process
  set.spawn(async move {
    log::info!("Starting events listener");
    if let Err(err) = poller.start().await {
      log::error!("Poller error: {:?}", err);
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
  timeout: u64,
) -> bool {
  let start_time = chrono::Utc::now().timestamp() as u64;
  while (chrono::Utc::now().timestamp() as u64) < start_time + timeout {
    let balance = anvil_provider
      .get_balance(*address)
      .await
      .expect("Failed to get balance");
    if balance == U256::from(0) {
      return true;
    }
    anvil_provider
      .anvil_mine(Some(U256::from(1)), Some(U256::from(1)))
      .await
      .expect("Failed to mine");
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
  }
  false
}

async fn make_invalid_op_poke(
  age: u64,
  private_key: &str,
  scribe_optimistic: &ScribeOptimisticInstance<Http<Client>, AnvilProvider, Ethereum>,
) {
  let poke_data: IScribe::PokeData = IScribe::PokeData {
    val: 10,
    age: age as u32,
  };
  let schnorr_data = IScribe::SchnorrData {
    signature: FixedBytes(hex!(
      "0000000000000000000000000000000000000000000000000000000000000000"
    )),
    commitment: alloy::primitives::Address::ZERO,
    feedIds: hex!("00").into(),
  };
  let op_poke_message = scribe_optimistic
    .constructOpPokeMessage(poke_data.clone(), schnorr_data.clone())
    .call()
    .await
    .expect("Failed to read current age");
  let op_poke_message = op_poke_message._0;
  let ecdsa_signer = private_key
    .parse::<PrivateKeySigner>()
    .expect("Failed to parse private key");
  let signature = ecdsa_signer
    .sign_hash_sync(&op_poke_message)
    .expect("Failed to sign message");
  let ecdsa_data = IScribe::ECDSAData {
    v: 0x1b + (signature.v() as u8),
    r: FixedBytes(signature.r().to_be_bytes()),
    s: FixedBytes(signature.s().to_be_bytes()),
  };

  // Make the invalid poke
  let receipt = scribe_optimistic.opPoke(poke_data.clone(), schnorr_data.clone(), ecdsa_data);
  let receipt = receipt.send().await.expect("Failed to send op poke");
  receipt.watch().await.expect("Failed to watch op poke");
}
