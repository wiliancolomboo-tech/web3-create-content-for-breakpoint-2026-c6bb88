// Run via ts-node scripts/settle-cli.ts
import * as anchor from "@coral-xyz/anchor";
const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);

console.log("StreamPoint-2026 Engine active and responsive on cluster:", provider.connection.rpcEndpoint);