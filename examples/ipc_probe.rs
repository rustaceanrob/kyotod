use std::env;

use bdk_kyoto::bip157::tokio;
use kyotod::server_capnp::server;
use tokio::net::UnixStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let path = env::args().nth(1).expect("usage: ipc_probe <socket>");
    let mode = env::args().nth(2).unwrap_or_else(|| "offline".to_string());
    let import_path = env::args().nth(3);

    tokio::task::LocalSet::new()
        .run_until(async move {
            let stream = UnixStream::connect(&path).await.expect("connect");
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
            let client: server::Client =
                rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
            tokio::task::spawn_local(rpc);

            if let Some(p) = import_path.as_deref() {
                let json = std::fs::read_to_string(p).expect("read import file");
                let mut req = client.import_wallet_request();
                req.get().set_json(json.as_str());
                match req.send().promise.await {
                    Ok(resp) => {
                        let r = resp.get().unwrap();
                        println!(
                            "import ok={} name={} msg={}",
                            r.get_ok(),
                            r.get_name().unwrap().to_string().unwrap(),
                            r.get_message().unwrap().to_string().unwrap()
                        );
                    }
                    Err(e) => println!("import error: {e}"),
                }
            }

            let bal = client.balances_request().send().promise.await.unwrap();
            let entries = bal.get().unwrap().get_entries().unwrap();
            for entry in entries.iter() {
                let name = entry.get_name().unwrap().to_string().unwrap();
                println!(
                    "wallet {} sats={} active={}",
                    name,
                    entry.get_sats(),
                    entry.get_active()
                );
            }

            let rec = client.receive_request().send().promise.await.unwrap();
            let addr = rec
                .get()
                .unwrap()
                .get_address()
                .unwrap()
                .to_string()
                .unwrap();
            println!("receive: {addr}");

            let exp = {
                let mut req = client.export_wallet_request();
                req.get().set_name("alice");
                req.send().promise.await.unwrap()
            };
            let json = exp.get().unwrap().get_json().unwrap().to_string().unwrap();
            println!("export bytes: {}", json.len());

            let mut sa = client.set_active_request();
            sa.get().set_name("bob");
            let sa_res = sa.send().promise.await.unwrap();
            let r = sa_res.get().unwrap();
            println!(
                "setActive ok={} msg={}",
                r.get_ok(),
                r.get_message().unwrap().to_string().unwrap()
            );

            if mode == "online" {
                let h = client
                    .height_request()
                    .send()
                    .promise
                    .await
                    .unwrap()
                    .get()
                    .unwrap()
                    .get_height();
                println!("height: {h}");

                let peers = client.peers_request().send().promise.await.unwrap();
                let entries = peers.get().unwrap().get_entries().unwrap();
                for entry in entries.iter() {
                    println!("peer: {}", entry.unwrap().to_string().unwrap());
                }
            }

            let bt = {
                let mut req = client.build_transaction_request();
                let mut p = req.get();
                p.set_recipient(addr.as_str());
                p.set_sats(10_000);
                p.set_sat_per_vb(1.5);
                p.set_drain(false);
                p.set_out_path("");
                req.send().promise.await
            };
            match bt {
                Ok(resp) => {
                    let r = resp.get().unwrap();
                    println!(
                        "build: path={} signed={} txid={} feeSats={} rawLen={}",
                        r.get_path().unwrap().to_string().unwrap(),
                        r.get_signed(),
                        r.get_txid().unwrap().to_string().unwrap(),
                        r.get_fee_sats(),
                        r.get_raw_tx().unwrap().len()
                    );
                }
                Err(e) => println!("build error (expected on empty wallet): {e}"),
            }

            let _ = client.shutdown_request().send().promise.await;
            println!("shutdown sent");
        })
        .await;
}
