# Challenger Rust draft

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