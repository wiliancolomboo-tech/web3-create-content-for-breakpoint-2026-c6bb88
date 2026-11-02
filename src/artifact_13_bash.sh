# 1. Configure CLI to Devnet
solana config set --url https://api.devnet.solana.com

# 2. Fund deployer wallet
solana airdrop 2

# 3. Synchronize program ID in code
anchor keys sync

# 4. Rebuild & Deploy
anchor build
anchor deploy --provider.cluster Devnet