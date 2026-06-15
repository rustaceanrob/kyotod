# kyotod

> [!WARNING]
> This software is entirely LLM generated and may contain bugs. Use at your own risk.

A daemon-style wallet built around a BIP-157 compact-block-filter light client
(via [`bdk_kyoto`](https://crates.io/crates/bdk_kyoto)), modeled after
`bitcoind`/`bitcoin-cli`. Wallets are described as
[BIP-139](https://github.com/bitcoin/bips/blob/master/bip-0139.mediawiki)
metadata backups and persisted to SQLite. A Cap'n Proto IPC server on a unix
socket exposes the wallet surface to a thin `cli` client.

## Dependencies

- [Rust compiler](https://rust-lang.org/tools/install/)
- [Capnp compiler](https://capnproto.org/install.html)

## Components

| Binary       | Role                                                                                     |
|--------------|------------------------------------------------------------------------------------------|
| `kyotod`     | The daemon. Loads wallets, drives the light client, serves IPC.                          |
| `cli`  | Command-line client over the IPC socket.                                                 |
| `tui`  | Interactive terminal UI over the same IPC socket.                                        |


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

`cli` only needs to know the socket location:

```
cli [--datadir <PATH>] <subcommand> [args]
```

Default `--datadir` is `~/.kyotod`. The socket is always `<datadir>/node.sock`.

## Using the TUI

`tui` is an interactive front-end on the same unix socket. Start the
daemon, then:

```sh
tui                          # default --datadir ~/.kyotod
tui --datadir /path/to/dd
```

If `kyotod` isn't reachable on the socket, the TUI prints an error and exits
before entering raw mode, so your terminal stays clean.

### Screens

```
 wallets ──────────────────────┐
 * alice            12345 sats │   ←─ home: list of all wallets, * = active
   bob                  0 sats │      poll refreshes every 2s
 ──────────────────────────────┘
   height 247811   peers 1   active alice
   j/k move  Enter open  c create  i import  a set-active  q quit   ? help
```

Press `Enter` on a wallet to drill into its detail screen — balance, recent
canonical history (left), QR code of the receive address + the address text
(right). `r` reveals a new address (advances the keychain by one), `s` opens
the send form, `Esc` returns to the list.

The send form has four fields (`recipient`, `sats`, `sat/vB`, `psbt out
path`) and a `drain` toggle (`Alt+d`). `Tab` cycles, `Enter` submits. On
success the result screen shows the PSBT path, txid, fee, and whether the
wallet finalized the transaction. If `signed: yes`, `b` broadcasts the raw
transaction over `broadcastTx`.

### Importing wallets

From the home screen:

- **`c` create** — three-field form (name, external descriptor, change
  descriptor). The TUI builds a BIP-139 backup in-process and submits it
  via `importWallet`. Same code path as `cli create`.
- **`i` import** — single-field path to a BIP-139 JSON file. `~/` is
  expanded.

The daemon tears down the running light client, rebuilds it over
the new wallet set, and resumes syncing.

### Help and quit

- `?` toggles a centered overlay listing all keys for every screen.
- `Esc` dismisses the help overlay or pops one level of navigation.
- `q` quits from the home screen; `Ctrl-c` quits from anywhere.
