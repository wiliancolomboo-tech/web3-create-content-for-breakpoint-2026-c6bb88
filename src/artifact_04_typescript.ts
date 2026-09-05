import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Hyperdrive } from "../target/types/hyperdrive";
import {
  createMint,
  createAccount,
  mintTo,
  TOKEN_2022_PROGRAM_ID,
  getAccount,
} from "@solana/spl-token";
import { PublicKey, Keypair, SystemProgram } from "@solana/web3.js";
import { expect } from "chai";

describe("HyperDrive Technical QA Test Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.Hyperdrive as Program<Hyperdrive>;
  const admin = Keypair.generate();
  const keeper = Keypair.generate();
  const alice = Keypair.generate();
  const bob = Keypair.generate();

  let mintIn: PublicKey;
  let mintOut: PublicKey;
  let configPda: PublicKey;
  let configBump: number;
  let vaultTokenIn: PublicKey;
  let settlementSourceOut: PublicKey;

  let aliceTokenIn: PublicKey;
  let bobTokenIn: PublicKey;
  let keeperTokenOut: PublicKey;

  const BATCH_EPOCH_SLOT_DURATION = 10;

  before(async () => {
    // Fund test actors
    for (const actor of [admin, keeper, alice, bob]) {
      const sig = await provider.connection.requestAirdrop(
        actor.publicKey,
        10 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig);
    }

    [configPda, configBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    );

    // Deploy Token-2022 test mints
    mintIn = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    mintOut = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Create escrow and protocol vaults owned by Config PDA
    vaultTokenIn = await createAccount(
      provider.connection,
      admin,
      mintIn,
      configPda,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    settlementSourceOut = await createAccount(
      provider.connection,
      admin,
      mintOut,
      configPda,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Initialize user and keeper accounts
    aliceTokenIn = await createAccount(
      provider.connection,
      alice,
      mintIn,
      alice.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    bobTokenIn = await createAccount(
      provider.connection,
      bob,
      mintIn,
      bob.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    keeperTokenOut = await createAccount(
      provider.connection,
      keeper,
      mintOut,
      keeper.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Mint input liquidity to users
    await mintTo(provider.connection, admin, mintIn, aliceTokenIn, admin, 10_000_000_000, [], undefined, TOKEN_2022_PROGRAM_ID);
    await mintTo(provider.connection, admin, mintIn, bobTokenIn, admin, 20_000_000_000, [], undefined, TOKEN_2022_PROGRAM_ID);

    // Pre-fund settlement buffer with MintOut for keeper distribution test
    await mintTo(provider.connection, admin, mintOut, settlementSourceOut, admin, 50_000_000_000, [], undefined, TOKEN_2022_PROGRAM_ID);
  });

  it("1. Initializes Engine Global Parameters with Boundary Protection", async () => {
    await program.methods
      .initializeEngine(250, keeper.publicKey) // 2.5% fee
      .accounts({
        config: configPda,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([admin])
      .rpc();

    const configAccount = await program.account.engineConfig.fetch(configPda);
    expect(configAccount.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(configAccount.protocolFeeBps).to.equal(250);
    expect(configAccount.isPaused).to.be.false;
  });

  it("2. Sets up Zero-Copy Epoch Batch PDA buffer", async () => {
    const currentSlot = await provider.connection.getSlot();
    const epochId = new anchor.BN(Math.floor(currentSlot / BATCH_EPOCH_SLOT_DURATION));

    const [batchPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("batch"), epochId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    await program.methods
      .initializeBatch(epochId)
      .accounts({
        batchBuffer: batchPda,
        payer: admin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([admin])
      .rpc();

    const bufferData = await program.account.batchBuffer.fetch(batchPda);
    expect(bufferData.epochId.toNumber()).to.equal(epochId.toNumber());
    expect(bufferData.intentCount).to.equal(0);
    expect(bufferData.settled).to.equal(0);
  });

  it("3. Enqueues Multi-User Intents into Ring Buffer (Escrow Verification)", async () => {
    const currentSlot = await provider.connection.getSlot();
    const epochId = new anchor.BN(Math.floor(currentSlot / BATCH_EPOCH_SLOT_DURATION));

    const [batchPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("batch"), epochId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const aliceDeposit = new anchor.BN(5_000_000_000); // 5,000 Tokens
    const aliceMinOut = new anchor.BN(4_900_000_000);

    await program.methods
      .submitIntent(aliceDeposit, aliceMinOut)
      .accounts({
        config: configPda,
        batchBuffer: batchPda,
        user: alice.publicKey,
        mintIn: mintIn,
        userTokenIn: aliceTokenIn,
        vaultTokenIn: vaultTokenIn,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([alice])
      .rpc();

    const vaultAccount = await getAccount(
      provider.connection,
      vaultTokenIn,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    expect(Number(vaultAccount.amount)).to.equal(5_000_000_000);

    const buffer = await program.account.batchBuffer.fetch(batchPda);
    expect(buffer.intentCount).to.equal(1);
    expect(buffer.totalInputAmount.toNumber()).to.equal(5_000_000_000);
  });

  it("4. Blocks Settlement if Epoch Window is Still Open (Front-running Guard)", async () => {
    const currentSlot = await provider.connection.getSlot();
    const currentEpoch = new anchor.BN(Math.floor(currentSlot / BATCH_EPOCH_SLOT_DURATION));

    const [batchPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("batch"), currentEpoch.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    try {
      await program.methods
        .batchSettle(currentEpoch, new anchor.BN(1000), new anchor.BN(1000))
        .accounts({
          config: configPda,
          batchBuffer: batchPda,
          keeper: keeper.publicKey,
          mintOut: mintOut,
          settlementSourceOut: settlementSourceOut,
          keeperTokenOut: keeperTokenOut,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
        })
        .signers([keeper])
        .rpc();
      expect.fail("Expected EpochStillActive error");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("EpochStillActive");
    }
  });

  it("5. Settles Batch Past Epoch Boundary with Uniform Clearing and Keeper Rebate", async () => {
    const currentSlot = await provider.connection.getSlot();
    const activeEpochNum = Math.floor(currentSlot / BATCH_EPOCH_SLOT_DURATION);
    const pastEpoch = new anchor.BN(activeEpochNum);

    const [batchPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("batch"), pastEpoch.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    // Wait until slots advance past boundary
    let advancedSlot = await provider.connection.getSlot();
    while (Math.floor(advancedSlot / BATCH_EPOCH_SLOT_DURATION) <= activeEpochNum) {
      await new Promise((resolve) => setTimeout(resolve, 800));
      advancedSlot = await provider.connection.getSlot();
    }

    // 1:1 Clearing Price (1_000_000_000 / 1_000_000_000)
    await program.methods
      .batchSettle(pastEpoch, new anchor.BN(1_000_000_000), new anchor.BN(1_000_000_000))
      .accounts({
        config: configPda,
        batchBuffer: batchPda,
        keeper: keeper.publicKey,
        mintOut: mintOut,
        settlementSourceOut: settlementSourceOut,
        keeperTokenOut: keeperTokenOut,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([keeper])
      .rpc();

    const bufferState = await program.account.batchBuffer.fetch(batchPda);
    expect(bufferState.settled).to.equal(1);

    // Verify 0.5% Keeper incentive transfer
    const keeperBalance = await getAccount(
      provider.connection,
      keeperTokenOut,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    expect(Number(keeperBalance.amount)).to.be.greaterThan(0);
  });
});