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

## Usage

The preferred workflow to issue most commands is by using `just`. Read more about [`just`](https://github.com/casey/just).

Settings for the daemon are pulled from the `config_spec.toml`. You may alter any one of these parameters to your discretion, or pass them as CLI arguments when starting the server. For instance:

```
cargo run --bin server --height=170000
```

Notably, you may want to change the descriptors to a signet wallet you control.

All wallet data is stored within a `.wallet` folder contained in this directory.

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

