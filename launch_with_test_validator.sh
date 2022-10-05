#!/usr/bin/bash

# check if keys doesn't exist, create it and fill with keys
#if [! -d "keys" ]; then
#    mkdir keys
#    for i in `seq 1 10`; do solana-keygen new -s --no-bip39-passphrase -o keys/program_$i.json; done
#fi
#make

# run test validator
#solana-test-validator
# create ne default signer
#solana-keygen new -o keys/admin.json
#solana airdrop 50000 <PUBKEY> 
#for i in `seq 1 10`; do solana -u localhost --use-quic -k keys/admin.json program deploy --program-id keys/program_$i.json target/program_$i.so; done
# cargo build --manifest-path client/Cargo.toml --release
#for i in `seq 1 10`; do ./client/target/release/make_contention_accounts --fee-payer ./keys/admin.json --rpc-node http://localhost:8899  --program-id ./keys/program_$i.json; done

#./client/target/release/fetch_tpu --gossip-entrypoint 127.0.0.1:1024 --output-file tpu.bin

cargo build --manifest-path client/Cargo.toml --release
./client/target/release/client\
         --tpu-file tpu.bin \
         --rpc-server http://localhost:8899 \
         --funds-source ./keys/admin.json \
         --program-id  59TbbR1hF3hZbeeV7PZTPp9zGaVybETevnTe6S2qJywK\
         --program-id  49Ffhy9USyAFKxEn1NLy9wBcJg2yoZLFkpaUWn71NmpH\
         --program-id  6uPMsjKkjrKwB63rMVuuCdPAewFzsqyepbkP8xCMJe2z\
         --num-threads 1 \
         --total-transactions 10


