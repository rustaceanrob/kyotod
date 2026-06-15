use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::str::FromStr;

use bdk_kyoto::bip157::tokio;
use bdk_kyoto::Requester;
use bdk_wallet::bitcoin::consensus::{self, Decodable};
use bdk_wallet::bitcoin::{Address, Amount, FeeRate, Transaction};
use bdk_wallet::{KeychainKind, SignOptions};
use bip139::WalletBackup;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, error};

use crate::paths::Layout;
use crate::server_capnp;
use crate::sync::ProgressSlot;
use crate::wallet::{self, State};

pub type RequesterSlot = Arc<Mutex<Option<Requester>>>;

pub struct IpcInterface {
    shutdown_tx: mpsc::Sender<()>,
    rebuild_tx: mpsc::Sender<()>,
    state: Arc<Mutex<State>>,
    requester: RequesterSlot,
    progress: ProgressSlot,
    layout: Arc<Layout>,
    network: bdk_wallet::bitcoin::Network,
}

impl IpcInterface {
    pub fn new(
        shutdown_tx: mpsc::Sender<()>,
        rebuild_tx: mpsc::Sender<()>,
        state: Arc<Mutex<State>>,
        requester: RequesterSlot,
        progress: ProgressSlot,
        layout: Arc<Layout>,
        network: bdk_wallet::bitcoin::Network,
    ) -> Self {
        Self {
            shutdown_tx,
            rebuild_tx,
            state,
            requester,
            progress,
            layout,
            network,
        }
    }

    fn requester(&self) -> Result<Requester, capnp::Error> {
        self.requester
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| failed("no wallets loaded; node is not running"))
    }
}

fn valid_wallet_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn failed(msg: impl Into<String>) -> capnp::Error {
    capnp::Error::failed(msg.into())
}

pub struct ServerArgs {
    pub layout: Arc<Layout>,
    pub network: bdk_wallet::bitcoin::Network,
    pub shutdown_tx: mpsc::Sender<()>,
    pub rebuild_tx: mpsc::Sender<()>,
    pub state: Arc<Mutex<State>>,
    pub requester: RequesterSlot,
    pub progress: ProgressSlot,
}

pub fn spawn_server(args: ServerArgs) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("ipc runtime");
        rt.block_on(async move {
            tokio::task::LocalSet::new().run_until(accept_loop(args)).await;
        });
    });
}

async fn accept_loop(args: ServerArgs) {
    let socket_path = args.layout.socket();
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!(target: "ipc", "bind {}: {e}", socket_path.display());
            return;
        }
    };
    debug!(target: "ipc", "listening on {}", socket_path.display());
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                error!(target: "ipc", "accept: {e}");
                continue;
            }
        };
        let (reader, writer) = stream.into_split();
        let reader = futures::io::BufReader::new(reader.compat());
        let writer = futures::io::BufWriter::new(writer.compat_write());
        let net = capnp_rpc::twoparty::VatNetwork::new(
            reader,
            writer,
            capnp_rpc::rpc_twoparty_capnp::Side::Server,
            Default::default(),
        );
        let interface = IpcInterface::new(
            args.shutdown_tx.clone(),
            args.rebuild_tx.clone(),
            args.state.clone(),
            args.requester.clone(),
            args.progress.clone(),
            args.layout.clone(),
            args.network,
        );
        let client: server_capnp::server::Client = capnp_rpc::new_client(interface);
        let rpc = capnp_rpc::RpcSystem::new(Box::new(net), Some(client.client));
        tokio::task::spawn_local(rpc);
    }
}

impl server_capnp::server::Server for IpcInterface {
    async fn shutdown(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::ShutdownParams,
        _: server_capnp::server::ShutdownResults,
    ) -> Result<(), capnp::Error> {
        let _ = self.shutdown_tx.send(()).await;
        Ok(())
    }

    async fn set_active(
        self: capnp::capability::Rc<Self>,
        params: server_capnp::server::SetActiveParams,
        mut results: server_capnp::server::SetActiveResults,
    ) -> Result<(), capnp::Error> {
        let name = params.get()?.get_name()?.to_string()?;
        let mut state = self.state.lock().unwrap();
        match state.set_active(&name) {
            Ok(()) => {
                let mut r = results.get();
                r.set_ok(true);
                r.set_message(format!("active wallet is now '{name}'").as_str());
            }
            Err(e) => {
                let mut r = results.get();
                r.set_ok(false);
                r.set_message(e.to_string().as_str());
            }
        }
        Ok(())
    }

    async fn export_wallet(
        self: capnp::capability::Rc<Self>,
        params: server_capnp::server::ExportWalletParams,
        mut results: server_capnp::server::ExportWalletResults,
    ) -> Result<(), capnp::Error> {
        let name = params.get()?.get_name()?.to_string()?;
        let state = self.state.lock().unwrap();
        let entry = state
            .wallets
            .get(&name)
            .ok_or_else(|| failed(format!("no wallet named {name}")))?;
        let json = entry
            .backup
            .to_json_pretty()
            .map_err(|e| failed(format!("serialize: {e}")))?;
        results.get().set_json(json.as_str());
        Ok(())
    }

    async fn receive(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::ReceiveParams,
        mut results: server_capnp::server::ReceiveResults,
    ) -> Result<(), capnp::Error> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .active_entry_mut()
            .ok_or_else(|| failed("no active wallet"))?;
        let info = entry.wallet.reveal_next_address(KeychainKind::External);
        entry
            .wallet
            .persist(&mut entry.conn)
            .map_err(|e| failed(format!("persist: {e}")))?;
        results.get().set_address(info.address.to_string().as_str());
        Ok(())
    }

    async fn balance(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::BalanceParams,
        mut results: server_capnp::server::BalanceResults,
    ) -> Result<(), capnp::Error> {
        let state = self.state.lock().unwrap();
        let entry = state
            .active_entry()
            .ok_or_else(|| failed("no active wallet"))?;
        results
            .get()
            .set_sats(entry.wallet.balance().total().to_sat());
        Ok(())
    }

    async fn balances(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::BalancesParams,
        mut results: server_capnp::server::BalancesResults,
    ) -> Result<(), capnp::Error> {
        let state = self.state.lock().unwrap();
        let mut names: Vec<&String> = state.wallets.keys().collect();
        names.sort();
        let mut list = results.get().init_entries(names.len() as u32);
        for (i, name) in names.iter().enumerate() {
            let entry = state.wallets.get(*name).unwrap();
            let mut row = list.reborrow().get(i as u32);
            row.set_name(name.as_str());
            row.set_sats(entry.wallet.balance().total().to_sat());
            row.set_active(state.active.as_deref() == Some(name.as_str()));
        }
        Ok(())
    }

    async fn history(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::HistoryParams,
        mut results: server_capnp::server::HistoryResults,
    ) -> Result<(), capnp::Error> {
        let state = self.state.lock().unwrap();
        let entry = state
            .active_entry()
            .ok_or_else(|| failed("no active wallet"))?;
        let text = entry
            .wallet
            .transactions()
            .map(|t| format!("{} {:?}", t.tx_node.txid, t.chain_position))
            .collect::<Vec<_>>()
            .join("\n");
        results.get().set_entries(text.as_str());
        Ok(())
    }

    async fn broadcast_tx(
        self: capnp::capability::Rc<Self>,
        params: server_capnp::server::BroadcastTxParams,
        mut results: server_capnp::server::BroadcastTxResults,
    ) -> Result<(), capnp::Error> {
        let mut raw = params.get()?.get_tx()?;
        let tx = Transaction::consensus_decode(&mut raw)
            .map_err(|e| failed(format!("decode tx: {e}")))?;
        let txid = tx.compute_txid().to_string();
        self.requester()?
            .submit_package(tx)
            .await
            .map_err(|e| failed(format!("broadcast: {e}")))?;
        results.get().set_txid(txid.as_str());
        Ok(())
    }

    async fn height(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::HeightParams,
        mut results: server_capnp::server::HeightResults,
    ) -> Result<(), capnp::Error> {
        let tip = self
            .requester()?
            .chain_tip()
            .await
            .map_err(|e| failed(format!("chain tip: {e}")))?;
        results.get().set_height(tip.height);
        Ok(())
    }

    async fn peers(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::PeersParams,
        mut results: server_capnp::server::PeersResults,
    ) -> Result<(), capnp::Error> {
        let peers = self
            .requester()?
            .peer_info()
            .await
            .map_err(|e| failed(format!("peer info: {e}")))?;
        let mut list = results.get().init_entries(peers.len() as u32);
        for (i, (addr, services)) in peers.iter().enumerate() {
            list.set(i as u32, format!("{addr:?} services={services}").as_str());
        }
        Ok(())
    }

    async fn sync_progress(
        self: capnp::capability::Rc<Self>,
        _: server_capnp::server::SyncProgressParams,
        mut results: server_capnp::server::SyncProgressResults,
    ) -> Result<(), capnp::Error> {
        let p = *self.progress.lock().unwrap();
        let mut r = results.get();
        r.set_percent(p.unwrap_or(0.0));
        r.set_has_data(p.is_some());
        Ok(())
    }

    async fn build_transaction(
        self: capnp::capability::Rc<Self>,
        params: server_capnp::server::BuildTransactionParams,
        mut results: server_capnp::server::BuildTransactionResults,
    ) -> Result<(), capnp::Error> {
        let p = params.get()?;
        let recipient = p.get_recipient()?.to_string()?;
        let sats = p.get_sats();
        let sat_per_vb = p.get_sat_per_vb();
        let drain = p.get_drain();
        let out_arg = p.get_out_path()?.to_string()?;

        if !sat_per_vb.is_finite() || sat_per_vb < 0.0 {
            return Err(failed("satPerVb must be a non-negative finite number"));
        }
        let fee_rate = FeeRate::from_sat_per_kwu((sat_per_vb * 250.0).round() as u64);

        let out_path = if out_arg.is_empty() {
            self.layout.root.join("tx.psbt")
        } else {
            PathBuf::from(out_arg)
        };

        let mut state = self.state.lock().unwrap();
        let entry = state
            .active_entry_mut()
            .ok_or_else(|| failed("no active wallet"))?;
        let network = entry.wallet.network();

        let address = Address::from_str(&recipient)
            .map_err(|e| failed(format!("address: {e}")))?
            .require_network(network)
            .map_err(|e| failed(format!("address network: {e}")))?;
        let spk = address.script_pubkey();

        let mut psbt = {
            let mut tb = entry.wallet.build_tx();
            tb.fee_rate(fee_rate);
            if drain {
                tb.drain_wallet().drain_to(spk);
            } else {
                if sats == 0 {
                    return Err(failed("sats must be > 0 when drain=false"));
                }
                tb.add_recipient(spk, Amount::from_sat(sats));
            }
            tb.finish().map_err(|e| failed(format!("build: {e}")))?
        };

        entry
            .wallet
            .persist(&mut entry.conn)
            .map_err(|e| failed(format!("persist: {e}")))?;

        let signed = entry
            .wallet
            .sign(&mut psbt, SignOptions::default())
            .map_err(|e| failed(format!("sign: {e}")))?;
        drop(state);

        let fee_sats = psbt.fee().map(|a| a.to_sat()).unwrap_or(0);
        let psbt_bytes = psbt.serialize();

        let (txid, raw_tx_bytes) = if signed {
            let tx = psbt
                .extract_tx()
                .map_err(|e| failed(format!("extract: {e}")))?;
            (tx.compute_txid().to_string(), consensus::encode::serialize(&tx))
        } else {
            (psbt.unsigned_tx.compute_txid().to_string(), Vec::new())
        };

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out_path)
            .map_err(|e| failed(format!("create {}: {e}", out_path.display())))?;
        file.write_all(&psbt_bytes)
            .map_err(|e| failed(format!("write {}: {e}", out_path.display())))?;

        let mut r = results.get();
        r.set_path(out_path.to_string_lossy().as_ref());
        r.set_signed(signed);
        r.set_txid(txid.as_str());
        r.set_raw_tx(&raw_tx_bytes);
        r.set_fee_sats(fee_sats);
        Ok(())
    }

    async fn import_wallet(
        self: capnp::capability::Rc<Self>,
        params: server_capnp::server::ImportWalletParams,
        mut results: server_capnp::server::ImportWalletResults,
    ) -> Result<(), capnp::Error> {
        let json = params.get()?.get_json()?.to_string()?;
        let backup =
            WalletBackup::from_json(&json).map_err(|e| failed(format!("parse json: {e}")))?;
        backup
            .validate()
            .map_err(|e| failed(format!("validate: {e}")))?;

        let name = backup
            .name
            .clone()
            .ok_or_else(|| failed("backup has no `name` field"))?;
        if !valid_wallet_name(&name) {
            return Err(failed(format!(
                "wallet name '{name}' must match [A-Za-z0-9_-]+"
            )));
        }

        let json_path = self.layout.wallets_dir().join(format!("{name}.json"));
        if json_path.exists() {
            return Err(failed(format!("{} already exists", json_path.display())));
        }

        let mut state = self.state.lock().unwrap();
        if state.wallets.contains_key(&name) {
            return Err(failed(format!("wallet '{name}' already loaded")));
        }

        let entry = wallet::build_entry(&name, backup.clone(), &self.layout, self.network)
            .map_err(|e| failed(format!("build wallet: {e}")))?;

        let canonical = backup
            .to_json_pretty()
            .map_err(|e| failed(format!("serialize backup: {e}")))?;
        std::fs::write(&json_path, canonical)
            .map_err(|e| failed(format!("write {}: {e}", json_path.display())))?;

        state.wallets.insert(name.clone(), entry);
        if state.active.is_none() {
            state.active = Some(name.clone());
        }
        drop(state);

        if let Err(e) = self.rebuild_tx.send(()).await {
            return Err(failed(format!("rebuild signal: {e}")));
        }

        let mut r = results.get();
        r.set_ok(true);
        r.set_name(name.as_str());
        r.set_message(
            format!(
                "imported '{name}'; light client rebuilding (wallet count {})",
                self.state.lock().unwrap().wallets.len()
            )
            .as_str(),
        );
        Ok(())
    }
}
