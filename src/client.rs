use bdk_kyoto::kyoto::Address;
use bdk_wallet::bitcoin::address::NetworkUnchecked;
use kyotod::{daemon_client::DaemonClient, StopRequest};
use kyotod::{BalanceRequest, CoinRequest, DescriptorRequest, IsMineRequest, ReceiveRequest};

use clap::{Args, Parser, Subcommand};
use qrcode::render::unicode;
use qrcode::QrCode;

mod kyotod {
    tonic::include_proto!("kyotod");
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Get the balance of the underlying wallet.
    Balance(Balance),
    /// List the coins (unspent outputs) owned by the wallet.
    Coins(GetCoin),
    /// Print the descriptors of the underlying wallet.
    Descriptors,
    /// Check if a Bitcoin address belongs to the wallet.
    IsMine(IsMine),
    /// Generate a new receiving address.
    Receive,
    /// Stop the daemon.
    Stop,
}

#[derive(Debug, Args)]
struct Balance {
    /// Should the balance be returned as satoshis.
    #[arg(long, default_value_t = false)]
    in_satoshis: bool,
    /// Include confirmed and unconfirmed balances.
    #[arg(short, long, default_value_t = false)]
    verbose: bool,
}

#[derive(Debug, Args)]
struct GetCoin {
    /// Should the coin value be returned as satoshis.
    #[arg(long, default_value_t = false)]
    in_satoshis: bool,
    /// Only return unspent coins above a satoshi threshold.
    #[arg(long)]
    satoshi_threshold: Option<u64>,
    /// Return coins sent to the wallet after the specified block.
    #[arg(long)]
    after_block: Option<u32>,
}

#[derive(Debug, Args)]
struct IsMine {
    /// A Bitcoin address to check for inclusion in the wallet.
    #[arg(long)]
    address: Address<NetworkUnchecked>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = DaemonClient::connect("http://[::1]:50051").await?;
    let cli = Arguments::parse();
    match cli.command {
        Command::Balance(Balance {
            in_satoshis,
            verbose,
        }) => {
            let request = BalanceRequest {
                in_satoshis,
                verbose,
            };
            let balance_response = client.balance(request).await?;
            let balance = balance_response.into_inner().balance;
            println!("{balance}")
        }
        Command::Coins(GetCoin {
            in_satoshis,
            satoshi_threshold,
            after_block,
        }) => {
            let request = CoinRequest {
                in_satoshis,
                sat_threshold: satoshi_threshold.unwrap_or(0),
                height_threshold: after_block.unwrap_or(0),
            };
            let coins_response = client.coins(request).await?;
            let coins = coins_response.into_inner().coins;
            println!("Coins:");
            for coin in coins {
                println!("{coin}")
            }
        }
        Command::Descriptors => {
            let request = DescriptorRequest {};
            let descriptor_response = client.descriptors(request).await?;
            let descriptors = descriptor_response.into_inner();
            println!("Receive (external) descriptor: {}", descriptors.receive);
            println!("Change  (internal) descriptor: {}", descriptors.change);
        }
        Command::IsMine(IsMine { address }) => {
            let request = IsMineRequest {
                address: address.assume_checked().to_string(),
            };
            let is_mine_response = client.is_mine(request).await?;
            let is_mine = is_mine_response.into_inner().response;
            println!("{is_mine}");
        }
        Command::Receive => {
            let request = ReceiveRequest {};
            let address_response = client.next_address(request).await?;
            let inner = address_response.into_inner();
            println!("===============================================================");
            println!("{}", inner.address);
            println!("===============================================================");
            println!("");
            println!("Address revealed to index {}", inner.index);
            let uri = format!("bitcoin:{}", inner.address);
            println!("{uri}");
            println!("");
            let qr_code = QrCode::new(uri)?;
            let qr_string = qr_code
                .render()
                .quiet_zone(false)
                .min_dimensions(40, 40)
                .max_dimensions(40, 40)
                .module_dimensions(1, 1)
                .dark_color(unicode::Dense1x2::Dark)
                .light_color(unicode::Dense1x2::Light)
                .build();
            println!("{qr_string}");
        }
        Command::Stop => {
            let request = StopRequest {};
            client.stop(request).await?;
        }
    }
    Ok(())
}
