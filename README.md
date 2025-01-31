# Challenger Rust version

Challenger searches for `opPoked` events for `ScribeOptimistic` contract. Verifies poke schnorr signature and challenges it, if it's invalid

```
Usage: challenger [OPTIONS] --rpc-url <RPC_URL>

Options:
  -a, --addresses <ADDRESSES>
          ScribeOptimistic contract addresses. Example: `0x891E368fE81cBa2aC6F6cc4b98e684c106e2EF4f`
      --rpc-url <RPC_URL>
          Node HTTP RPC_URL, normally starts with https://****
      --flashbot-rpc-url <FLASHBOT_RPC_URL>
          Flashbot Node HTTP RPC_URL, like https://rpc-sepolia.flashbots.net/fast
      --secret-key <RAW_SECRET_KEY>
          Private key in format `0x******` or `*******`. If provided, no need to use --keystore
      --keystore <KEYSTORE_PATH>
          Keystore file (NOT FOLDER), path to key .json file. If provided, no need to use --secret-key [env: ETH_KEYSTORE=]
      --password <RAW_PASSWORD>
          Key raw password as text
      --password-file <PASSWORD_FILE>
          Path to key password file [env: ETH_PASSWORD=]
      --chain-id <CHAIN_ID>
          If no chain_id provided binary will try to get chain_id from given RPC
      --from-block <FROM_BLOCK>
          Block number to start from
  -h, --help
          Print help
  -V, --version
          Print version
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

# Development

## Rust toolchain

For this project we use the nightly toolchain. To install it, run:
```sh
rustup toolchain install nightly
```

To set the nightly toolchain as the default for folder, run:
```sh
rustup override set nightly
```

As well we provide a `rust-toolchain.toml` file that sets the nightly toolchain for the project.

### Rust analyzer

For rust-analyzer to work correctly, you need to install the nightly toolchain and set it as the default for the folder.
Also `RUST_TOOLCHAIN` environment variable should be set to `nightly`.

Intellij Example:

```
settings -> Language & Frameworks -> Rust -> Rustfmt

Add the environment Variable = "RUSTUP_TOOLCHAIN": "nightly"

Add Additional argument =  "+nightly"
```


Zed example:

`.zed/settings.json`
```json
// Folder-specific settings
//
// For a full list of overridable settings, and general information on folder-specific settings,
// see the documentation: https://zed.dev/docs/configuring-zed#settings-files
{
  "tab_size": 2,
  "lsp": {
    "rust-analyzer": {
      "initialization_options": {
        "server": {
          "extraEnv": {
            "RUSTUP_TOOLCHAIN": "nightly"
          }
        },
        "rustfmt": {
          "extraArgs": ["+nightly"]
        }
      }
    }
  }
}
```


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
  -a, --addresses <ADDRESSES>
          ScribeOptimistic contract addresses. Example: `0x891E368fE81cBa2aC6F6cc4b98e684c106e2EF4f`
      --rpc-url <RPC_URL>
          Node HTTP RPC_URL, normally starts with https://****
      --flashbot-rpc-url <FLASHBOT_RPC_URL>
          Flashbot Node HTTP RPC_URL, like https://rpc-sepolia.flashbots.net/fast
      --secret-key <RAW_SECRET_KEY>
          Private key in format `0x******` or `*******`. If provided, no need to use --keystore
      --keystore <KEYSTORE_PATH>
          Keystore file (NOT FOLDER), path to key .json file. If provided, no need to use --secret-key [env: ETH_KEYSTORE=]
      --password <RAW_PASSWORD>
          Key raw password as text
      --password-file <PASSWORD_FILE>
          Path to key password file [env: ETH_PASSWORD=]
      --chain-id <CHAIN_ID>
          If no chain_id provided binary will try to get chain_id from given RPC
      --from-block <FROM_BLOCK>
          Block number to start from
  -h, --help
          Print help
  -V, --version
          Print version
```

```bash
docker run --it --rm --name challenger -e RUST_LOG=debug -a ADDRESS -a ADDRESS2 --rpc-url http://localhost:3334 --secret-key asdfasdfas
```

## Metrics & Health checks

By default challenger exposes metrics and health checks on `:9090/metrics` route.
Metrics are exposed in prometheus format and health checks are exposed in json format.

Health check route is `:9090/health`.

`HTTP_PORT` env variable can be used to change the port.
Example: `HTTP_PORT=8080` will expose metrics on `:8080/metrics` and health checks on `:8080/health`.
