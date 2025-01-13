default:
  just --list

build:
  cargo build

check:
   cargo fmt
   cargo clippy --all-targets

serve:
  cargo run --bin server

balance: 
  cargo run --bin client balance

descriptors:
  cargo run --bin client descriptors

receive:
  cargo run --bin client receive

stop:
  cargo run --bin client stop
