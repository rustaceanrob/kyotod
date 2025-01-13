use kyotod::{daemon_client::DaemonClient, StopRequest};
use kyotod::{BalanceRequest, ReceiveRequest};

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
            let _ = client.stop(request).await;
        }
    }
    Ok(())
}
