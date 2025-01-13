default:
  just --list

build:
  cargo build

check:
   cargo fmt
   cargo clippy --all-targets

serve:
  cargo run --bin server

help:
  cargo run --bin client help

balance: 
  cargo run --bin client balance

coins:
  cargo run --bin client coins

descriptors:
  cargo run --bin client descriptors

receive:
  cargo run --bin client receive

stop:
  cargo run --bin client stop
