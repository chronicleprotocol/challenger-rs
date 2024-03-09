# Challenger Rust version

Challenger searches for `opPoked` events for `ScribeOptimistic` contract. Verifies poke schnorr signature and challenges it, if it's invalid

```
Usage: challenger [OPTIONS] --addresses <ADDRESS1> --addresses <ADDRESS2> --rpc-url <RPC_URL>

Options:
  -a, --addresses <ADDRESS>            ScribeOptimistic contract address. Example: `0x891E368fE81cBa2aC6F6cc4b98e684c106e2EF4f`
      --rpc-url <RPC_URL>              Node HTTP RPC_URL, normally starts with https://****
      --secret-key <SECRET_KEY>        Private key in format `0x******` or `*******`. If provided, no need to use --keystore
      --keystore <KEYSTORE>            Keystore file (NOT FOLDER), path to key .json file. If provided, no need to use --secret-key [env: ETH_KEYSTORE=]
      --password <PASSWORD>            Key raw password as text
      --password-file <PASSWORD_FILE>  Path to key password file [env: ETH_PASSWORD=]
      --chain-id <CHAIN_ID>            If no chain_id provided binary will try to get chain_id from given RPC
  -h, --help                           Print help
  -V, --version                        Print version
```

## Example

Starting with private key

```bash
challenger --addresses 0x891E368fE81cBa2aC6F6cc4b98e684c106e2EF4f --addresses 0x******* --rpc-url http://localhost:3334 --secret-key 0x******
```

Starting with key file and password

```bash
challenger -a 0x891E368fE81cBa2aC6F6cc4b98e684c106e2EF4f -a 0x******* --rpc-url http://localhost:3334 --keystore /path/to/key.json --password-file /path/to/file
```

## Logging level

By default `challenger` uses log level `info`.
If you want to get debug information use `RUST_LOG=debug` env variable !


## Building docker image

SERVER_VERSION have to be same as release but without `v`, if release is `v0.0.10` then `SERVER_VERSION=0.0.10`

```bash
docker build --build-arg SERVER_VERSION=0.0.10 -t challenger .
```

usage: 

```bash
docker run --rm challenger

Challenger searches for `opPoked` events for `ScribeOptimistic` contract. Verifies poke schnorr signature and challenges it, if it's invalid

Usage: challenger [OPTIONS] --rpc-url <RPC_URL>

Options:
  -a, --addresses <ADDRESSES>          ScribeOptimistic contract addresses. Example: `0x891E368fE81cBa2aC6F6cc4b98e684c106e2EF4f`
      --rpc-url <RPC_URL>              Node HTTP RPC_URL, normally starts with https://****
      --secret-key <SECRET_KEY>        Private key in format `0x******` or `*******`. If provided, no need to use --keystore
      --keystore <KEYSTORE>            Keystore file (NOT FOLDER), path to key .json file. If provided, no need to use --secret-key [env: ETH_KEYSTORE=]
      --password <PASSWORD>            Key raw password as text
      --password-file <PASSWORD_FILE>  Path to key password file [env: ETH_PASSWORD=]
      --chain-id <CHAIN_ID>            If no chain_id provided binary will try to get chain_id from given RPC
  -h, --help                           Print help
  -V, --version                        Print version
```

```bash
docker run --it --rm --name challenger -e RUST_LOG=debug -a ADDRESS -a ADDRESS2 --rpc-url http://localhost:3334 --secret-key asdfasdfas
```

# Challenger library

To use the `Challenger` library, you first need to create an instance of the `Challenger` struct by calling the `new` method and passing in the contract address and a provider that implements the `ScribeOptimisticProvider` trait (currently only `HttpScribeOptimisticProvider` is implemented). Here's an example:

```rust
use ethers::providers::{Http, Provider};
use challenger::{Challenger, HttpScribeOptimisticProvider};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_provider = Provider::<Http>::connect("https://mainnet.infura.io/v3/your-project-id").await?;
    let contract_address = "0x1234567890123456789012345678901234567890".parse()?;
    let provider = HttpScribeOptimisticProvider::new(contract_address, rpc_provider);
    let challenger = Challenger::new(contract_address, provider, Duration::from_secs(30), None);

    // ...
    Ok(())
}
```

In this example, we're using the `Http` provider from the `ethers` crate to connect to the Ethereum mainnet. We're also passing in a contract address and a tick interval of 30 seconds to the `Challenger::new` method.

Once you have an instance of the `Challenger` struct, you can start processing pokes by calling the `start` method. This method will start continues checks for new pokes every `tick_interval` seconds and process them if they're within the challenge period. 

You can also customize the behavior of the `Challenger` struct by setting the `max_failure_count` field to control how many failures are allowed before processing is stopped for a particular address, and by setting the `challenge_period_reload_interval` field to control how often the challenge period is reloaded from the contract. Here's an example:

```rust
use ethers::providers::{Http, Provider};
use challenger::{Challenger, HttpScribeOptimisticProvider};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_provider = Provider::<Http>::connect("https://mainnet.infura.io/v3/your-project-id").await?;
    let contract_address = "0x1234567890123456789012345678901234567890".parse()?;
    let provider = HttpScribeOptimisticProvider::new(contract_address, rpc_provider);
    let max_failure: u8 = 5;
    let challenger = Challenger::new(contract_address, provider, Duration::from_secs(30), Some(max_failure));

    challenger.start().await?;
}
```

## Metrics & Health checks

By default challenger exposes metrics and health checks on `:9090/metrics` route.
Metrics are exposed in prometheus format and health checks are exposed in json format.

Health check route is `:9090/health`.

`HTTP_PORT` env variable can be used to change the port.
Example: `HTTP_PORT=8080` will expose metrics on `:8080/metrics` and health checks on `:8080/health`.
