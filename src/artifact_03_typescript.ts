import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { BreakpointSettler } from "../target/types/breakpoint_settler";
import {
  TOKEN_2022_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { Keypair, SystemProgram, PublicKey } from "@solana/web3.js";
import { expect } from "chai";
import { keccak_256 } from "@noble/hashes/sha3";

describe("Breakpoint 2026: Aegis Intent Settlement Engine Suite", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.BreakpointSettler as Program<BreakpointSettler>;

  const admin = Keypair.generate();
  const settlementAuthority = Keypair.generate();
  const feeRecipient = Keypair.generate();
  const depositor = Keypair.generate();
  const taker = Keypair.generate();

  let mint: PublicKey;
  let depositorTokenAccount: PublicKey;
  let takerTokenAccount: PublicKey;
  let feeTokenAccount: PublicKey;
  let vaultTokenAccount: PublicKey;
  let configPda: PublicKey;
  let escrowVaultPda: PublicKey;

  const nonce = new anchor.BN(1337);
  const depositAmount = new anchor.BN(5_000_000_000); // 5 tokens (9 decimals)

  before(async () => {
    // AirDrop SOL to actors
    for (const actor of [admin, settlementAuthority, depositor, taker]) {
      const sig = await provider.connection.requestAirdrop(actor.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      const latestBlockhash = await provider.connection.getLatestBlockhash();
      await provider.connection.confirmTransaction({
        signature: sig,
        blockhash: latestBlockhash.blockhash,
        lastValidBlockHeight: latestBlockhash.lastValidBlockHeight,
      });
    }

    // Create Token-2022 Mint
    mint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      9,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Derive Config PDA
    [configPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    );

    // Derive Escrow PDA
    [escrowVaultPda] = PublicKey.findProgramAddressSync(
      [
        Buffer.from("settlement_vault"),
        settlementAuthority.publicKey.toBuffer(),
        nonce.toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    );

    // Create Associated Token Accounts
    depositorTokenAccount = await createAccount(
      provider.connection,
      depositor,
      mint,
      depositor.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    takerTokenAccount = await createAccount(
      provider.connection,
      taker,
      mint,
      taker.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    feeTokenAccount = await createAccount(
      provider.connection,
      admin,
      mint,
      feeRecipient.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    vaultTokenAccount = await createAccount(
      provider.connection,
      depositor,
      mint,
      escrowVaultPda,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Mint tokens to depositor
    await mintTo(
      provider.connection,
      admin,
      mint,
      depositorTokenAccount,
      admin,
      10_000_000_000,
      [],
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
  });

  it("1. Initializes Market with deterministic fee and max slippage boundaries", async () => {
    const tx = await program.methods
      .initializeMarket(200, 50) // 2% max slippage, 0.5% protocol fee
      .accounts({
        config: configPda,
        settlementAuthority: settlementAuthority.publicKey,
        feeRecipient: feeRecipient.publicKey,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([admin, settlementAuthority])
      .rpc();

    const configAccount = await program.account.globalSettlementConfig.fetch(configPda);
    expect(configAccount.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(configAccount.settlementAuthority.toBase58()).to.equal(settlementAuthority.publicKey.toBase58());
    expect(configAccount.maxSlippageBps).to.equal(200);
    expect(configAccount.protocolFeeBps).to.equal(50);
  });

  it("2. Escrow liquidity with dynamic nonce via Token-2022 CPI", async () => {
    await program.methods
      .escrowDeposit(nonce, depositAmount)
      .accounts({
        config: configPda,
        escrowVault: escrowVaultPda,
        depositorTokenAccount: depositorTokenAccount,
        vaultTokenAccount: vaultTokenAccount,
        mint: mint,
        depositor: depositor.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([depositor])
      .rpc();

    const vaultAccount = await program.account.escrowVault.fetch(escrowVaultPda);
    expect(vaultAccount.escrowAmount.toString()).to.equal(depositAmount.toString());
    expect(vaultAccount.owner.toBase58()).to.equal(depositor.publicKey.toBase58());

    const tokenBal = await getAccount(provider.connection, vaultTokenAccount, undefined, TOKEN_2022_PROGRAM_ID);
    expect(tokenBal.amount.toString()).to.equal(depositAmount.toString());
  });

  it("3. Verifies Merkle Intent Root and settles single-slot atomic batch fill", async () => {
    const batchId = new anchor.BN(882026);
    const fillAmount = new anchor.BN(1_000_000_000); // 1 Token
    const minOut = new anchor.BN(990_000_000);
    const expiry = new anchor.BN(Math.floor(Date.now() / 1000) + 3600);

    // Compute Leaf Hash matching program hash logic
    const fillLeafBuffer = Buffer.concat([
      depositor.publicKey.toBuffer(),
      taker.publicKey.toBuffer(),
      fillAmount.toArrayLike(Buffer, "le", 8),
      minOut.toArrayLike(Buffer, "le", 8),
      expiry.toArrayLike(Buffer, "le", 8),
    ]);
    const leafHash = Buffer.from(keccak_256(fillLeafBuffer));

    const [fillReceiptPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("receipt"), batchId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const intentPayload = {
      maker: depositor.publicKey,
      taker: taker.publicKey,
      amountIn: fillAmount,
      minAmountOut: minOut,
      expiry: expiry,
    };

    const tx = await program.methods
      .settleBatchIntents(
        batchId,
        Array.from(leafHash),
        [], // Trivial tree depth = 0 for single leaf
        [intentPayload]
      )
      .accounts({
        config: configPda,
        settlementAuthority: settlementAuthority.publicKey,
        escrowVault: escrowVaultPda,
        fillReceipt: fillReceiptPda,
        vaultTokenAccount: vaultTokenAccount,
        recipientTokenAccount: takerTokenAccount,
        feeTokenAccount: feeTokenAccount,
        mint: mint,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([settlementAuthority])
      .rpc();

    // Verify receipt creation
    const receipt = await program.account.fillReceipt.fetch(fillReceiptPda);
    expect(receipt.isProcessed).to.be.true;

    // Verify fee calculation (0.5% of 1_000_000_000 = 5_000_000)
    const feeBal = await getAccount(provider.connection, feeTokenAccount, undefined, TOKEN_2022_PROGRAM_ID);
    expect(feeBal.amount.toString()).to.equal("5000000");

    // Verify net payout (1_000_000_000 - 5_000_000 = 995_000_000)
    const takerBal = await getAccount(provider.connection, takerTokenAccount, undefined, TOKEN_2022_PROGRAM_ID);
    expect(takerBal.amount.toString()).to.equal("995000000");
  });

  it("4. Prevents double-spend replay attacks on identical batch ID", async () => {
    const batchId = new anchor.BN(882026);
    const [fillReceiptPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("receipt"), batchId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    const dummyLeaf = Buffer.alloc(32, 1);

    try {
      await program.methods
        .settleBatchIntents(batchId, Array.from(dummyLeaf), [], [])
        .accounts({
          config: configPda,
          settlementAuthority: settlementAuthority.publicKey,
          escrowVault: escrowVaultPda,
          fillReceipt: fillReceiptPda,
          vaultTokenAccount: vaultTokenAccount,
          recipientTokenAccount: takerTokenAccount,
          feeTokenAccount: feeTokenAccount,
          mint: mint,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([settlementAuthority])
        .rpc();
      expect.fail("Expected transaction to fail due to account already initialized");
    } catch (err: any) {
      expect(err.toString()).to.include("already in use");
    }
  });
});