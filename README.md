<div align="center">
  <h1>Kyotod</h1>
  <p>
    <strong>A nerdy Bitcoin wallet using compact block filters and BDK</strong>
  </p>
</div>

## Disclaimer 

This is a work-in-progress and should not be used with real funds.

## Quick Start

Begin by running a daemon in one terminal
 
```
cargo run --bin server
```

Interact with the daemon using the client in another terminal

```
cargo run --bin client balance
```

```
cargo run --bin client help
```

Shut down the daemon from the client

```
cargo run --bin client stop
```

## Configuration

Wallet configuration is done through a `wallet.toml` file. The following tables describe the available configurations. 

#### Global keys

| key | value | options |
|-----|-------|---------|
| network | String | signet / regtest / bitcoin |

#### `[wallet]`

| key | value | description |
|-----|-------|-------------|
| receive | String | A valid descriptor for the configured network | 
| change  | String | A valid descriptor for the configured network |
| lookahead | Optional Uint32 | The number of scripts to peek ahead when checking block filters. Useful for recovering wallets with an approximate number of transactions |
| birthday | Optional Uint32 | The block height to look for transactions *strictly after* |

#### `[node]`
| key | value | description |
|-----|-------|-------------|
| connections | Optional Uint8 | The number of connections for the node to maintain |


## Usage

The preferred workflow to issue most commands is by using `just`. Read more about [`just`](https://github.com/casey/just).

All wallet data is stored within a `.wallet` folder. By default, the server will store wallet data and search for the `wallet.toml` in the current working directory. If your `wallet.toml` is located at a different path, you may pass a working directory to the server using a CLI argument:


```
cargo run --bin server /path/to/toml/ --release
```

Once your node has synced to its peers, you may get information about your configured wallet by issuing commands from another terminal:

Check the balance of the wallet:

```
just balance
```

List the UTXOs in the wallet:

```
just coins
```

To see a full list of commands:

```
just help
```

To see the options for an individual command:

```
cargo run --bin client <COMMAND> help
```

## How it works

When starting the server, a compact block filter node begins to scan for blocks for the descriptors provided on start up. 

The client then issues requests to the server using gRPC. Issuing the `stop` command kills the gRPC server and light client.

## License

Licensed under either of

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

