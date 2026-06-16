# kyotod

> [!WARNING]
> This software is entirely LLM generated and may contain bugs. Use at your own risk.

A daemon-style wallet built around a BIP-157 compact-block-filter light client
(via [`bdk_kyoto`](https://crates.io/crates/bdk_kyoto)). Wallets are described as
[BIP-139](https://github.com/bitcoin/bips/blob/master/bip-0139.mediawiki)
metadata backups and persisted to SQLite. A Cap'n Proto IPC server on a unix
socket exposes the wallet surface to a `tui` client.

## Dependencies

- [Rust compiler](https://rust-lang.org/tools/install/)
- [Capnp compiler](https://capnproto.org/install.html)

## Components

| Binary       | Role                                                                                     |
|--------------|------------------------------------------------------------------------------------------|
| `kyotod`     | The daemon. Loads wallets, drives the light client, serves IPC.                          |
| `tui`        | Interactive terminal UI over the IPC socket.                                             |


## Quick start

Start the daemon in the background, which is intended to run continuously. `kyotod` performs minimal disk and network I/O, so even limited systems may run this as a `systemd` service.

```sh
# 1. Start the daemon (signet by default). It will sit idle until a wallet
#    exists, then begin syncing.
cargo run --bin kyotod --release --daemon true
```

Then launch the TUI.

```sh
cargo run --bin tui --release
```

## Configuration

`kyotod` reads its config via [`configure_me`](https://crates.io/crates/configure_me).
Each parameter can be set via CLI flag or environment variable:

| Flag                  | Env                    | Default        | Notes                                                                          |
|-----------------------|------------------------|----------------|--------------------------------------------------------------------------------|
| `--network <N>`       | `KYOTOD_NETWORK`       | `signet`       | `bitcoin`, `signet`, `testnet`, `testnet4`, `regtest`.                         |
| `--datadir <PATH>`    | `KYOTOD_DATADIR`       | `~/.kyotod`    | Holds wallets, sqlite stores, socket, pid file, log.                           |
| `--connect <ADDR>`    | `KYOTOD_CONNECT`       | unset          | Optional `ip:port` or `host:port` for a single peer (skips DNS bootstrap).     |
| `--daemon true`       | `KYOTOD_DAEMON`        | `false`        | Fork into the background after startup.                                        |

Default `--datadir` is `~/.kyotod`. The socket is always `<datadir>/node.sock`.

## Using the TUI

`tui` is an interactive front-end on the same unix socket. Start the
daemon, then:

```sh
cargo run --bin tui --release   # default --datadir ~/.kyotod
cargo run --bin tui --release -- --datadir /path/to/dd
```

### Importing wallets

From the home screen:

- **`c` create** — three-field form (name, external descriptor, change
  descriptor). The TUI builds a BIP-139 backup in-process and submits it
  via `importWallet`.
- **`i` import** — single-field path to a BIP-139 JSON file. `~/` is
  expanded.

The daemon tears down the running light client, rebuilds it over
the new wallet set, and resumes syncing.

### Help and quit

- `?` toggles a centered overlay listing all keys for every screen.
- `Esc` dismisses the help overlay or pops one level of navigation.
- `q` quits from the home screen; `Ctrl-c` quits from anywhere.

If `kyotod` isn't reachable on the socket, the TUI prints an error and exits
before entering raw mode, so your terminal stays clean.

