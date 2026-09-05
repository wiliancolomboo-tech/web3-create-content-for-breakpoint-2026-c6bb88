# Build smart contracts with Anchor
anchor build

# Generate deterministic Program Keypair and sync with program ID
solana-keygen new -o target/deploy/hyperdrive-keypair.json --force --no-bip39-passphrase
export PROGRAM_ID=$(solana-keygen pubkey target/deploy/hyperdrive-keypair.json)
sed -i "s/HypRDrive1111111111111111111111111111111111/$PROGRAM_ID/g" programs/hyperdrive/src/lib.rs Anchor.toml
anchor build