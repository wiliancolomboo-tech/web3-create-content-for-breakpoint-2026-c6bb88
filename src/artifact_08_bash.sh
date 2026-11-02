# Clone and enter the repository
cd breakpoint_settler

# Install NPM dependencies
yarn install

# Synchronize Anchor Program ID with generated Keypair
anchor keys sync

# Build with optimizations
anchor build

# Execute Anchor Mocha Test Suite against Localnet
anchor test --skip-local-validator