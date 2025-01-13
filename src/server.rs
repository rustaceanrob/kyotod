use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use bdk_kyoto::builder::LightClientBuilder;
use bdk_kyoto::logger::TraceLogger;
use bdk_kyoto::{EventSender, EventSenderExt, LightClient};
use bdk_wallet::bitcoin::{Address, Network};
use bdk_wallet::rusqlite::Connection;
use bdk_wallet::{KeychainKind, PersistedWallet, Wallet};

use kyotod::daemon_server::{Daemon, DaemonServer};
use kyotod::{
    BalanceReply, BalanceRequest, CoinRequest, CoinResponse, DescriptorRequest, DescriptorResponse,
    IsMineRequest, IsMineResponse, ReceiveRequest, ReceiveResponse, StopRequest, StopResponse,
};
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use tokio::sync::mpsc;
use tokio::sync::Mutex;

mod kyotod {
    tonic::include_proto!("kyotod");
}

configure_me::include_config!();

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
        if let Err(_) = self.shutdown.send(()).await {
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

    let (config, _) = Config::including_optional_config_files::<&[&str]>(&[]).unwrap_or_exit();
    // General
    let mut root_dir = PathBuf::from(".");
    root_dir.push(".wallet");
    if !root_dir.exists() {
        std::fs::create_dir_all(&root_dir)?
    }
    // Wallet configs
    let receive = config.receive_descriptor;
    let change = config.change_descriptor;
    let lookahead = config.lookahead;
    let network = Network::from_str(&config.network)?;
    // Node configs
    let connections = config.peers;
    let height = config.height;

    let mut conn = Connection::open(root_dir.join(".bdk_wallet.sqlite"))?;

    let wallet_opt = Wallet::load()
        .descriptor(KeychainKind::External, Some(receive.clone()))
        .descriptor(KeychainKind::Internal, Some(change.clone()))
        .lookahead(lookahead)
        .check_network(Network::Signet)
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
