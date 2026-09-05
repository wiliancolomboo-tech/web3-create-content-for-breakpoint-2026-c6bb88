# Terminal 1: Start test validator with Token-2022 extensions enabled
solana-test-validator --reset

# Terminal 2: Deploy program
anchor deploy --program-name hyperdrive

# Initialize Engine configuration via Solana CLI / custom script
solana program dump $PROGRAM_ID target/deploy/hyperdrive_dump.so
echo "HyperDrive program deployed successfully to localnet with ID: $PROGRAM_ID"