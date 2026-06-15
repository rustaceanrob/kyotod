@0xc2e20cc9503cf68f;

struct WalletBalance {
    name @0 :Text;
    sats @1 :UInt64;
    active @2 :Bool;
}

interface Server {
    shutdown @0 () -> ();
    setActive @1 (name :Text) -> (ok :Bool, message :Text);
    exportWallet @2 (name :Text) -> (json :Text);
    receive @3 () -> (address :Text);
    balance @4 () -> (sats :UInt64);
    balances @5 () -> (entries :List(WalletBalance));
    history @6 () -> (entries :Text);
    broadcastTx @7 (tx :Data) -> (txid :Text);
    height @8 () -> (height :UInt32);
    peers @9 () -> (entries :List(Text));
    buildTransaction @10 (recipient :Text, sats :UInt64, satPerVb :Float64, drain :Bool, outPath :Text)
        -> (path :Text, signed :Bool, txid :Text, rawTx :Data, feeSats :UInt64);
    importWallet @11 (json :Text) -> (ok :Bool, name :Text, message :Text);
    syncProgress @12 () -> (percent :Float32, hasData :Bool);
}
