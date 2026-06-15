use std::error::Error;
use std::path::PathBuf;

use bdk_kyoto::bip157::tokio;
use bdk_wallet::bitcoin::hex::FromHex;
use clap::{Args, Parser, Subcommand};
use kyotod::paths;
use kyotod::server_capnp::server;
use tokio::net::UnixStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const DEFAULT_DATADIR: &str = "~/.kyotod";

#[derive(Parser)]
#[command(name = "cli", about = "Control kyotod over its unix socket")]
struct Cli {
    #[arg(long, default_value = DEFAULT_DATADIR, global = true)]
    datadir: String,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stop the daemon.
    Stop,
    /// Set the active wallet.
    SetActive { name: String },
    /// Export a wallet's BIP-139 JSON to stdout.
    Export { name: String },
    /// Import a BIP-139 JSON file. The name is taken from the JSON.
    Import { path: PathBuf },
    /// Create and import a wallet from a name plus external/change descriptors.
    Create {
        name: String,
        external: String,
        change: String,
        /// Wallet birthday height. The node will skip filter download below
        /// this height once it has resolved the corresponding block hash.
        #[arg(long)]
        birthday: Option<u32>,
    },
    /// Print the next receiving address of the active wallet.
    Receive,
    /// Print the active wallet's balance in sats.
    Balance,
    /// List balances for every loaded wallet.
    Balances,
    /// Print transaction history for the active wallet.
    History,
    /// Broadcast a hex-encoded raw transaction.
    Broadcast { hex: String },
    /// Print the node's current chain height.
    Height,
    /// List connected peers.
    Peers,
    /// Build (and sign if possible) a transaction; writes PSBT to disk.
    Send(SendArgs),
}

#[derive(Args)]
struct SendArgs {
    /// Recipient bitcoin address.
    recipient: String,
    /// Amount in sats. Conflicts with --drain.
    #[arg(long, conflicts_with = "drain")]
    sats: Option<u64>,
    /// Sweep all funds to the recipient.
    #[arg(long)]
    drain: bool,
    /// Fee rate in sat/vB. May be fractional.
    #[arg(long, default_value_t = 2.0)]
    sat_per_vb: f64,
    /// Output path for the PSBT. Defaults to <datadir>/tx.psbt.
    #[arg(long)]
    out: Option<PathBuf>,
}

async fn connect(datadir: &str) -> Result<server::Client, Box<dyn Error>> {
    let sock = paths::expand(datadir).join("node.sock");
    let stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| format!("connect {}: {e}", sock.display()))?;
    let (r, w) = stream.into_split();
    let r = futures::io::BufReader::new(r.compat());
    let w = futures::io::BufWriter::new(w.compat_write());
    let net = capnp_rpc::twoparty::VatNetwork::new(
        r,
        w,
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc = capnp_rpc::RpcSystem::new(Box::new(net), None);
    let client: server::Client = rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc);
    Ok(client)
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = rt.block_on(
        tokio::task::LocalSet::new().run_until(async move {
            let client = connect(&cli.datadir).await?;
            run(client, cli.cmd).await
        }),
    );
    if let Err(e) = result {
        eprintln!("error: {}", clean(&e.to_string()));
        std::process::exit(1);
    }
}

async fn send_import(client: &server::Client, json: String) -> Result<(), Box<dyn Error>> {
    let mut req = client.import_wallet_request();
    req.get().set_json(json.as_str());
    let resp = req.send().promise.await?;
    let r = resp.get()?;
    let name = r.get_name()?.to_string()?;
    let msg = r.get_message()?.to_string()?;
    if r.get_ok() {
        println!("ok {name}: {msg}");
        Ok(())
    } else {
        Err(format!("{name}: {msg}").into())
    }
}

fn clean(msg: &str) -> String {
    let s = msg.strip_prefix("Failed: ").unwrap_or(msg);
    s.strip_prefix("remote exception: ").unwrap_or(s).to_string()
}

async fn run(client: server::Client, cmd: Command) -> Result<(), Box<dyn Error>> {
    match cmd {
        Command::Stop => {
            client.shutdown_request().send().promise.await?;
            println!("kyotod stopping");
        }
        Command::SetActive { name } => {
            let mut req = client.set_active_request();
            req.get().set_name(name.as_str());
            let resp = req.send().promise.await?;
            let r = resp.get()?;
            let msg = r.get_message()?.to_string()?;
            if r.get_ok() {
                println!("ok: {msg}");
            } else {
                return Err(msg.into());
            }
        }
        Command::Export { name } => {
            let mut req = client.export_wallet_request();
            req.get().set_name(name.as_str());
            let resp = req.send().promise.await?;
            println!("{}", resp.get()?.get_json()?.to_string()?);
        }
        Command::Import { path } => {
            let json = std::fs::read_to_string(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            send_import(&client, json).await?;
        }
        Command::Create {
            name,
            external,
            change,
            birthday,
        } => {
            use bdk_wallet::miniscript::{Descriptor, DescriptorPublicKey};
            use bip139::{Account, WalletBackup, BIP_NUMBER, VERSION};

            let ext: Descriptor<DescriptorPublicKey> = external
                .parse()
                .map_err(|e| format!("external descriptor: {e}"))?;
            let chg: Descriptor<DescriptorPublicKey> = change
                .parse()
                .map_err(|e| format!("change descriptor: {e}"))?;

            let backup = WalletBackup {
                version: Some(VERSION),
                bip: Some(BIP_NUMBER),
                name: Some(name),
                accounts: vec![Account {
                    account_type: Some("bip_380".into()),
                    descriptor: Some(ext.into()),
                    change_descriptor: Some(chg.into()),
                    block_height: birthday,
                    ..Default::default()
                }],
                ..Default::default()
            };
            send_import(&client, backup.to_json()?).await?;
        }
        Command::Receive => {
            let resp = client.receive_request().send().promise.await?;
            println!("{}", resp.get()?.get_address()?.to_string()?);
        }
        Command::Balance => {
            let resp = client.balance_request().send().promise.await?;
            println!("{} sats", resp.get()?.get_sats());
        }
        Command::Balances => {
            let resp = client.balances_request().send().promise.await?;
            for e in resp.get()?.get_entries()?.iter() {
                let name = e.get_name()?.to_string()?;
                let marker = if e.get_active() { " *" } else { "" };
                println!("{name}{marker}\t{} sats", e.get_sats());
            }
        }
        Command::History => {
            let resp = client.history_request().send().promise.await?;
            let text = resp.get()?.get_entries()?.to_string()?;
            if text.is_empty() {
                println!("(no history)");
            } else {
                println!("{text}");
            }
        }
        Command::Broadcast { hex } => {
            let raw = Vec::<u8>::from_hex(&hex).map_err(|e| format!("hex: {e}"))?;
            let mut req = client.broadcast_tx_request();
            req.get().set_tx(&raw);
            let resp = req.send().promise.await?;
            println!("{}", resp.get()?.get_txid()?.to_string()?);
        }
        Command::Height => {
            let resp = client.height_request().send().promise.await?;
            println!("{}", resp.get()?.get_height());
        }
        Command::Peers => {
            let resp = client.peers_request().send().promise.await?;
            let entries = resp.get()?.get_entries()?;
            if entries.is_empty() {
                println!("(no peers)");
            } else {
                for e in entries.iter() {
                    println!("{}", e?.to_string()?);
                }
            }
        }
        Command::Send(s) => {
            if s.sats.is_none() && !s.drain {
                return Err("--sats <N> or --drain is required".into());
            }
            let mut req = client.build_transaction_request();
            let mut p = req.get();
            p.set_recipient(s.recipient.as_str());
            p.set_sats(s.sats.unwrap_or(0));
            p.set_sat_per_vb(s.sat_per_vb);
            p.set_drain(s.drain);
            p.set_out_path(
                s.out
                    .as_deref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .as_str(),
            );
            let resp = req.send().promise.await?;
            let r = resp.get()?;
            println!("psbt: {}", r.get_path()?.to_string()?);
            println!("txid: {}", r.get_txid()?.to_string()?);
            println!("fee:  {} sats", r.get_fee_sats());
            if r.get_signed() {
                println!(
                    "signed: yes  ({} bytes raw; feed to `cli broadcast <hex>`)",
                    r.get_raw_tx()?.len()
                );
            } else {
                println!("signed: no   (sign the PSBT externally, then broadcast)");
            }
        }
    }
    Ok(())
}
