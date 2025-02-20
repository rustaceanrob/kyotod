use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use bdk_kyoto::builder::LightClientBuilder;
use bdk_kyoto::logger::TraceLogger;
use bdk_kyoto::{EventSender, EventSenderExt, FeeRate, LightClient};
use bdk_wallet::bitcoin::{Address, Amount, Network, Psbt};
use bdk_wallet::rusqlite::Connection;
use bdk_wallet::{KeychainKind, PersistedWallet, SignOptions, Wallet};

use kyotod::daemon_server::{Daemon, DaemonServer};
use kyotod::{
    BalanceReply, BalanceRequest, BroadcastPsbtRequest, BroadcastPsbtResponse, CoinRequest,
    CoinResponse, CreatePsbtRequest, CreatePsbtResponse, DescriptorRequest, DescriptorResponse,
    DrainPsbtRequest, DrainPsbtResponse, IsMineRequest, IsMineResponse, ReceiveRequest,
    ReceiveResponse, StopRequest, StopResponse,
};
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use tokio::sync::mpsc;
use tokio::sync::Mutex;

mod kyotod {
    tonic::include_proto!("kyotod");
}

#[derive(serde::Deserialize)]
struct WalletConfig {
    network: Option<String>,
    wallet: WalletKeys,
    node: NodeKeys,
}

#[derive(serde::Deserialize)]
struct WalletKeys {
    receive: String,
    change: String,
    lookahead: Option<u32>,
    birthday: Option<u32>,
}

#[derive(serde::Deserialize)]
struct NodeKeys {
    connections: Option<u8>,
}

#[derive(Debug)]
struct WalletService {
    wallet: Arc<Mutex<PersistedWallet<Connection>>>,
    sender: Arc<Mutex<EventSender>>,
    conn: Arc<Mutex<Connection>>,
    shutdown: mpsc::Sender<()>,
}

impl WalletService {
    fn new(
        wallet: Arc<Mutex<PersistedWallet<Connection>>>,
        sender: Arc<Mutex<EventSender>>,
        conn: Arc<Mutex<Connection>>,
        shutdown: mpsc::Sender<()>,
    ) -> Self {
        Self {
            wallet,
            sender,
            conn,
            shutdown,
        }
    }
}

#[tonic::async_trait]
impl Daemon for WalletService {
    async fn balance(
        &self,
        request: Request<BalanceRequest>,
    ) -> Result<Response<BalanceReply>, Status> {
        let req = request.into_inner();
        let wallet_lock = self.wallet.lock().await;
        let balance = wallet_lock.balance();
        let unconfirmed = balance.trusted_pending + balance.untrusted_pending;
        let balance_str = if req.in_satoshis && req.verbose {
            format!(
                "Total: {:<16} SAT, Confirmed: {:<16} SAT, Unconfirmed: {:<16} SAT",
                balance.total().to_sat(),
                balance.confirmed.to_sat(),
                unconfirmed.to_sat(),
            )
        } else if !req.in_satoshis && req.verbose {
            format!(
                "Total: {:<16} BTC, Confirmed: {:<16} BTC, Unconfirmed: {:<16} BTC",
                balance.total().to_btc(),
                balance.confirmed.to_btc(),
                unconfirmed.to_btc(),
            )
        } else if req.in_satoshis && !req.verbose {
            format!("{:<16} SAT", balance.total().to_sat())
        } else {
            format!("{:<16} BTC", balance.total().to_btc())
        };
        let reply = BalanceReply {
            balance: balance_str,
        };
        Ok(Response::new(reply))
    }

    async fn broadcast_psbt(
        &self,
        request: Request<BroadcastPsbtRequest>,
    ) -> Result<Response<BroadcastPsbtResponse>, Status> {
        let req = request.into_inner();
        let path = PathBuf::from(&req.file);
        let file = File::open(path).map_err(|e| {
            Status::new(tonic::Code::Aborted, format!("Could not open PSBT: {}", e))
        })?;
        let mut reader = BufReader::new(file);
        let mut psbt = Psbt::deserialize_from_reader(&mut reader).map_err(|e| {
            Status::new(
                tonic::Code::Aborted,
                format!("Could not deserialize PSBT: {}", e),
            )
        })?;
        let wallet_lock = self.wallet.lock().await;
        let finalized = wallet_lock
            .finalize_psbt(&mut psbt, SignOptions::default())
            .map_err(|e| {
                Status::new(
                    tonic::Code::Aborted,
                    format!("Could not finalize PSBT: {}", e),
                )
            })?;
        if finalized {
            let extracted = psbt.extract_tx().map_err(|e| {
                Status::new(
                    tonic::Code::Aborted,
                    format!("Could not extract transaction: {}", e),
                )
            })?;
            let client = self.sender.lock().await;
            client
                .broadcast_tx(bdk_kyoto::TxBroadcast {
                    tx: extracted,
                    broadcast_policy: bdk_kyoto::TxBroadcastPolicy::RandomPeer,
                })
                .await
                .map_err(|_| {
                    Status::new(tonic::Code::Aborted, "Failed to broadcast transaction")
                })?;
            Ok(Response::new(BroadcastPsbtResponse {
                response: "Successfully sent transaction over the wire".into(),
            }))
        } else {
            return Err(Status::new(
                tonic::Code::Aborted,
                "PSBT finalization failed",
            ));
        }
    }

    async fn next_address(
        &self,
        _request: Request<ReceiveRequest>,
    ) -> Result<Response<ReceiveResponse>, Status> {
        let mut wallet_lock = self.wallet.lock().await;
        let next_address = wallet_lock.reveal_next_address(KeychainKind::External);
        let mut conn = self.conn.lock().await;
        if let Err(e) = wallet_lock.persist(&mut conn) {
            tracing::warn!("Wallet database operation failed");
            return Err(Status::new(
                tonic::Code::Aborted,
                format!("Datbase operation failed {}", e),
            ));
        }
        let index = next_address.index;
        let address = next_address.address;
        let reply = ReceiveResponse {
            address: address.to_string(),
            index,
        };
        tracing::info!("Revealing address for payment");
        Ok(Response::new(reply))
    }

    async fn coins(&self, request: Request<CoinRequest>) -> Result<Response<CoinResponse>, Status> {
        let req = request.into_inner();
        let wallet_lock = self.wallet.lock().await;
        let unspent_outputs = wallet_lock.list_unspent();
        let mut coins = Vec::new();
        let filtered_coins = unspent_outputs
            .filter(|o| o.txout.value.to_sat() > req.sat_threshold)
            .filter(|o| {
                o.chain_position
                    .confirmation_height_upper_bound()
                    .map_or(true, |upperbound| upperbound > req.height_threshold)
            });
        for unspent in filtered_coins {
            let keychain = match unspent.keychain {
                KeychainKind::Internal => "change",
                KeychainKind::External => "receive",
            };
            let index = unspent.derivation_index;
            let confirmation = if unspent.chain_position.is_confirmed() {
                format!(
                    "confirmed at {:>7}",
                    unspent
                        .chain_position
                        .confirmation_height_upper_bound()
                        .unwrap_or_default()
                )
            } else {
                "unconfirmed".into()
            };
            let amount = if req.in_satoshis {
                let sat = unspent.txout.value.to_sat();
                format!("{:<16} SAT", sat)
            } else {
                let btc = unspent.txout.value.to_btc();
                format!("{:<16} BTC", btc)
            };
            let coin = format!("{} {:>8}/{:<3} {}", amount, keychain, index, confirmation);
            coins.push(coin);
        }
        let reply = CoinResponse { coins };
        Ok(Response::new(reply))
    }

    async fn create_psbt(
        &self,
        request: Request<CreatePsbtRequest>,
    ) -> Result<Response<CreatePsbtResponse>, Status> {
        let req = request.into_inner();
        let mut wallet_lock = self.wallet.lock().await;
        let fee_rate = FeeRate::from_sat_per_vb(req.feerate)
            .ok_or(Status::new(tonic::Code::Aborted, "Invalid fee rate"))?;
        let address = Address::from_str(&req.address)
            .map_err(|e| Status::new(tonic::Code::Aborted, format!("Invalid address {}", e)))?;
        let address_checked = address
            .require_network(wallet_lock.network())
            .map_err(|e| Status::new(tonic::Code::Aborted, format!("Wrong network: {}", e)))?;
        let amount = Amount::from_sat(req.sats);
        let psbt = {
            let mut tx_builder = wallet_lock.build_tx();
            tx_builder
                .add_recipient(address_checked.script_pubkey(), amount)
                .fee_rate(fee_rate);
            tx_builder.finish().map_err(|e| {
                Status::new(
                    tonic::Code::Aborted,
                    format!("Could not create PSBT: {}", e),
                )
            })?
        };
        let path = PathBuf::from(".").join("unsigned_transaction.psbt");
        let mut file = std::fs::File::create(path).map_err(|e| {
            Status::new(
                tonic::Code::Aborted,
                format!("Could not create file for PSBT {}", e),
            )
        })?;
        psbt.serialize_to_writer(&mut file).map_err(|e| {
            Status::new(
                tonic::Code::Aborted,
                format!("Could not write to file {}", e),
            )
        })?;
        let mut conn = self.conn.lock().await;
        if let Err(e) = wallet_lock.persist(&mut conn) {
            tracing::warn!("Wallet database operation failed");
            return Err(Status::new(
                tonic::Code::Aborted,
                format!("Datbase operation failed {}", e),
            ));
        }
        Ok(Response::new(CreatePsbtResponse {
            response: "Successfully created PSBT at `unsigned_transaction.psbt`".into(),
        }))
    }

    async fn drain_psbt(
        &self,
        request: Request<DrainPsbtRequest>,
    ) -> Result<Response<DrainPsbtResponse>, Status> {
        let req = request.into_inner();
        let mut wallet_lock = self.wallet.lock().await;
        let fee_rate = FeeRate::from_sat_per_vb(req.feerate)
            .ok_or(Status::new(tonic::Code::Aborted, "Invalid fee rate"))?;
        let address = Address::from_str(&req.address)
            .map_err(|e| Status::new(tonic::Code::Aborted, format!("Invalid address {}", e)))?;
        let address_checked = address
            .require_network(wallet_lock.network())
            .map_err(|e| Status::new(tonic::Code::Aborted, format!("Wrong network: {}", e)))?;
        let psbt = {
            let mut tx_builder = wallet_lock.build_tx();
            tx_builder
                .drain_wallet()
                .drain_to(address_checked.script_pubkey())
                .fee_rate(fee_rate);
            tx_builder.finish().map_err(|e| {
                Status::new(
                    tonic::Code::Aborted,
                    format!("Could not create PSBT: {}", e),
                )
            })?
        };
        let path = PathBuf::from(".").join("unsigned_transaction.psbt");
        let mut file = std::fs::File::create(path).map_err(|e| {
            Status::new(
                tonic::Code::Aborted,
                format!("Could not create file for PSBT {}", e),
            )
        })?;
        psbt.serialize_to_writer(&mut file).map_err(|e| {
            Status::new(
                tonic::Code::Aborted,
                format!("Could not write to file {}", e),
            )
        })?;
        let mut conn = self.conn.lock().await;
        if let Err(e) = wallet_lock.persist(&mut conn) {
            tracing::warn!("Wallet database operation failed");
            return Err(Status::new(
                tonic::Code::Aborted,
                format!("Datbase operation failed {}", e),
            ));
        }
        Ok(Response::new(DrainPsbtResponse {
            response: "Successfully created PSBT at `unsigned_transaction.psbt`".into(),
        }))
    }

    async fn descriptors(
        &self,
        _request: Request<DescriptorRequest>,
    ) -> Result<Response<DescriptorResponse>, Status> {
        let wallet_lock = self.wallet.lock().await;
        let receive = wallet_lock
            .public_descriptor(KeychainKind::External)
            .to_string();
        let change = wallet_lock
            .public_descriptor(KeychainKind::Internal)
            .to_string();
        let reply = DescriptorResponse { receive, change };
        Ok(Response::new(reply))
    }

    async fn is_mine(
        &self,
        request: Request<IsMineRequest>,
    ) -> Result<Response<IsMineResponse>, Status> {
        let req = request.into_inner();
        let addr_res = Address::from_str(&req.address);
        if let Err(e) = addr_res {
            let reply = IsMineResponse {
                response: format!("Invalid address: {}", e),
            };
            return Ok(Response::new(reply));
        }
        let wallet_lock = self.wallet.lock().await;
        let addr_res = addr_res.unwrap().require_network(wallet_lock.network());
        if let Err(e) = addr_res {
            let reply = IsMineResponse {
                response: format!("Invalid address: {}", e),
            };
            return Ok(Response::new(reply));
        }
        let is_mine = wallet_lock.is_mine(addr_res.unwrap().into());
        let reply = IsMineResponse {
            response: format!("{}", is_mine),
        };
        Ok(Response::new(reply))
    }

    async fn stop(&self, _request: Request<StopRequest>) -> Result<Response<StopResponse>, Status> {
        let client_lock = self.sender.lock().await;
        tracing::info!("Shutting down");
        let _ = client_lock.shutdown().await;
        if self.shutdown.send(()).await.is_err() {
            return Err(Status::new(
                tonic::Code::Aborted,
                "Failed to shut down server",
            ));
        }
        Ok(Response::new(StopResponse {}))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen = "[::1]:50051".parse()?;

    let mut args = std::env::args();
    args.next();
    let path = args.next().unwrap_or(".".into());
    let mut root_dir = PathBuf::from(path);
    // General
    let wallet_toml_path = root_dir.clone().join("wallet.toml");
    let wallet_toml = std::fs::read_to_string(wallet_toml_path)?;

    root_dir.push(".wallet");
    if !root_dir.exists() {
        std::fs::create_dir_all(&root_dir)?
    }
    // Wallet configs
    let wallet_config: WalletConfig = toml::from_str(&wallet_toml)?;
    let receive = wallet_config.wallet.receive;
    let change = wallet_config.wallet.change;
    let lookahead = wallet_config.wallet.lookahead.unwrap_or(30);
    let network_str = wallet_config.network.unwrap_or("signet".to_string());
    let network = Network::from_str(&network_str)?;
    // Node configs
    let connections = wallet_config.node.connections.unwrap_or(2);
    let height = wallet_config.wallet.birthday;

    let mut conn = Connection::open(root_dir.join(".bdk_wallet.sqlite"))?;

    let wallet_opt = Wallet::load()
        .descriptor(KeychainKind::External, Some(receive.clone()))
        .descriptor(KeychainKind::Internal, Some(change.clone()))
        .lookahead(lookahead)
        .check_network(network)
        .load_wallet(&mut conn)?;

    let wallet = match wallet_opt {
        Some(wallet) => wallet,
        None => Wallet::create(receive, change)
            .network(network)
            .lookahead(lookahead)
            .create_wallet(&mut conn)?,
    };

    let mut builder = LightClientBuilder::new();

    if let Some(height) = height {
        builder = builder.scan_after(height)
    };

    let LightClient {
        sender,
        mut receiver,
        node,
    } = builder
        .connections(connections)
        .data_dir(root_dir)
        .build(&wallet)?;

    tokio::task::spawn(async move { node.run().await });

    let wallet = Arc::new(wallet.into());
    let sender = Arc::new(sender.into());
    let conn = Arc::new(conn.into());
    let (tx, mut rx) = mpsc::channel::<()>(5);

    let service = WalletService::new(
        Arc::clone(&wallet),
        Arc::clone(&sender),
        Arc::clone(&conn),
        tx,
    );

    tokio::task::spawn(async move {
        let logger = TraceLogger::new().unwrap();
        loop {
            if let Some(update) = receiver.update(&logger).await {
                let mut wallet_lock = wallet.lock().await;
                wallet_lock.apply_update(update).unwrap();
                let sender_lock = sender.lock().await;
                sender_lock
                    .add_revealed_scripts(&wallet_lock)
                    .await
                    .unwrap();
                let mut conn_lock = conn.lock().await;
                wallet_lock.persist(&mut conn_lock).unwrap();
            }
        }
    });

    Server::builder()
        .add_service(DaemonServer::new(service))
        .serve_with_shutdown(listen, async move {
            let _ = rx.recv().await;
        })
        .await?;

    Ok(())
}
