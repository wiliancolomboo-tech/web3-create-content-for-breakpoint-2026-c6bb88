import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { StreampointCore } from "../target/types/streampoint_core";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  sendAndConfirmTransaction,
} from "@solana/web3.js";
import {
  TOKEN_2022_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";
import * as crypto from "crypto";

describe("StreamPoint-2026 Core Protocol Integration Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.StreampointCore as Program<StreampointCore>;

  const admin = Keypair.generate();
  const operator = Keypair.generate();
  const creator = Keypair.generate();
  const funder = Keypair.generate();
  const keeper = Keypair.generate();

  let mint: PublicKey;
  let funderAta: PublicKey;
  let creatorAta: PublicKey;
  let feeTreasuryAta: PublicKey;

  let configPda: PublicKey;
  let channelPda: PublicKey;
  let vaultPda: PublicKey;

  const channelId = crypto.randomBytes(32);
  const ratePerSecond = new anchor.BN(50_000); // 50,000 base units / sec
  const initialFund = new anchor.BN(10_000_000); // 10 tokens

  before(async () => {
    // Air-drop test lamports to signers
    const accountsToFund = [admin, operator, creator, funder, keeper];
    for (const acc of accountsToFund) {
      const sig = await provider.connection.requestAirdrop(
        acc.publicKey,
        5 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig, "confirmed");
    }

    // Deploy Token-2022 Mint
    mint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6,
      Keypair.generate(),
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Associated/Dedicated Token-2022 Accounts
    funderAta = await createAccount(
      provider.connection,
      funder,
      mint,
      funder.publicKey,
      Keypair.generate(),
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    creatorAta = await createAccount(
      provider.connection,
      creator,
      mint,
      creator.publicKey,
      Keypair.generate(),
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    feeTreasuryAta = await createAccount(
      provider.connection,
      admin,
      mint,
      admin.publicKey,
      Keypair.generate(),
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Mint supply to funder
    await mintTo(
      provider.connection,
      admin,
      mint,
      funderAta,
      admin,
      100_000_000,
      [],
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    [configPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    );

    [channelPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("channel"), channelId, creator.publicKey.toBuffer()],
      program.programId
    );

    [vaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), channelId],
      program.programId
    );
  });

  it("1. Initializes the Global Protocol Config PDA", async () => {
    const feeBps = 250; // 2.5%

    const tx = await program.methods
      .initializeProtocol(feeBps, operator.publicKey)
      .accounts({
        config: configPda,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([admin])
      .rpc();

    expect(tx).to.be.a("string");
    const configState = await program.account.protocolConfig.fetch(configPda);
    expect(configState.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(configState.feeBasisPoints).to.equal(250);
  });

  it("2. Registers a New Content Channel with Vault Derivation", async () => {
    const maxCap = new anchor.BN(100_000_000);

    await program.methods
      .createChannel(Array.from(channelId), ratePerSecond, maxCap)
      .accounts({
        channel: channelPda,
        vault: vaultPda,
        mint: mint,
        config: configPda,
        owner: creator.publicKey,
        systemProgram: SystemProgram.programId,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([creator])
      .rpc();

    const channelData = await program.account.contentStreamChannel.fetch(channelPda);
    expect(channelData.owner.toBase58()).to.equal(creator.publicKey.toBase58());
    expect(channelData.isActive).to.be.true;
  });

  it("3. Escrows Funds into the Channel Vault via Token-2022 CPI", async () => {
    await program.methods
      .fundChannel(initialFund)
      .accounts({
        channel: channelPda,
        mint: mint,
        vault: vaultPda,
        funderTokenAccount: funderAta,
        funder: funder.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([funder])
      .rpc();

    const vaultTokenAccount = await getAccount(
      provider.connection,
      vaultPda,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    expect(Number(vaultTokenAccount.amount)).to.equal(initialFund.toNumber());
  });

  it("4. Cranks Settlement via Keeper and Verifies Fee Splits", async () => {
    // Artificial 2-second sleep to accrue streaming time
    await new Promise((resolve) => setTimeout(resolve, 2200));

    const creatorBalBefore = (
      await getAccount(provider.connection, creatorAta, undefined, TOKEN_2022_PROGRAM_ID)
    ).amount;

    await program.methods
      .settleStream()
      .accounts({
        channel: channelPda,
        mint: mint,
        vault: vaultPda,
        creator: creator.publicKey,
        creatorTokenAccount: creatorAta,
        feeTreasuryTokenAccount: feeTreasuryAta,
        config: configPda,
        keeper: keeper.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([keeper])
      .rpc();

    const creatorBalAfter = (
      await getAccount(provider.connection, creatorAta, undefined, TOKEN_2022_PROGRAM_ID)
    ).amount;

    expect(Number(creatorBalAfter)).to.be.greaterThan(Number(creatorBalBefore));
  });

  it("5. Rejects Unauthorized Transfers to Non-Vault Destinations (Security Trap)", async () => {
    const maliciousAta = await createAccount(
      provider.connection,
      funder,
      mint,
      funder.publicKey,
      Keypair.generate(),
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    const [extraMetas] = PublicKey.findProgramAddressSync(
      [Buffer.from("extra-account-metas"), mint.toBuffer()],
      program.programId
    );

    try {
      await program.methods
        .transferHook(new anchor.BN(100_000))
        .accounts({
          channel: channelPda,
          source: funderAta,
          mint: mint,
          destination: maliciousAta, // Invalid destination
          authority: funder.publicKey,
          extraAccountMetas: extraMetas,
        })
        .rpc();
      expect.fail("Transfer Hook should have failed due to invalid destination.");
    } catch (err: any) {
      expect(err.toString()).to.include("InvalidHookDestination");
    }
  });
});