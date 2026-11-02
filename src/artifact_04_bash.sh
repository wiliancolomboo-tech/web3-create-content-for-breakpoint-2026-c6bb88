# 1. Install TypeScript and Anchor dependencies
yarn install

# 2. Build Anchor Rust program targeting the Token-2022 runtime
anchor build

# 3. Verify Program ID alignment
solana address -k target/deploy/kronos_stream-keypair.json