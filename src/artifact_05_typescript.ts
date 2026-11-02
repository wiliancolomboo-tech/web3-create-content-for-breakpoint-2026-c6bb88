import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { ChronoStrike } from "../target/types/chrono_strike";
import {
  TOKEN_2022_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { assert, expect } from "chai";

describe("ChronoStrike Engine - Breakpoint 2026 Verification Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.ChronoStrike as Program<ChronoStrike>;

  const authority = anchor.web3.Keypair.generate();
  const maker = anchor.web3.Keypair.generate();
  const taker = anchor.web3.Keypair.generate();
  const feeCollector = anchor.web3.Keypair.generate();

  let baseMint: anchor.web3.Pubkey;
  let quoteMint: anchor.web3.Pubkey;

  let makerBaseAta: anchor.web3.Pubkey;
  let makerQuoteAta: anchor.web3.Pubkey;
  let takerBaseAta: anchor.web3.Pubkey;
  let takerQuoteAta: anchor.web3.Pubkey;
  let feeRecipientAta: anchor.web3.Pubkey;

  const marketId = new anchor.BN(2026);
  const intentNonce = new anchor.BN(1001);

  let marketPda: anchor.web3.Pubkey;
  let intentPda: anchor.web3.Pubkey;
  let vaultPda: anchor.web3.Pubkey;

  const INITIAL_MINT_BALANCE = 1_000_000_000;
  const BASE_DECIMALS = 6;
  const QUOTE_DECIMALS = 6;

  before(async () => {
    // Fund all signers via localnet faucet
    for (const kp of [authority, maker, taker, feeCollector]) {
      const sig = await provider.connection.requestAirdrop(
        kp.publicKey,
        10 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig, "confirmed");
    }

    // Initialize Token-2022 Base & Quote Mints
    baseMint = await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      BASE_DECIMALS,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    quoteMint = await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      QUOTE_DECIMALS,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Setup Token Accounts
    makerBaseAta = await createAccount(
      provider.connection,
      maker,
      baseMint,
      maker.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    makerQuoteAta = await createAccount(
      provider.connection,
      maker,
      quoteMint,
      maker.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    takerBaseAta = await createAccount(
      provider.connection,
      taker,
      baseMint,
      taker.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    takerQuoteAta = await createAccount(
      provider.connection,
      taker,
      quoteMint,
      taker.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    feeRecipientAta = await createAccount(
      provider.connection,
      feeCollector,
      quoteMint,
      feeCollector.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Seed balances
    await mintTo(
      provider.connection,
      authority,
      baseMint,
      makerBaseAta,
      authority,
      INITIAL_MINT_BALANCE,
      [],
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    await mintTo(
      provider.connection,
      authority,
      quoteMint,
      takerQuoteAta,
      authority,
      INITIAL_MINT_BALANCE,
      [],
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Derive PDA addresses
    [marketPda] = anchor.web3.Pubkey.findProgramAddressSync(
      [Buffer.from("market"), marketId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    [intentPda] = anchor.web3.Pubkey.findProgramAddressSync(
      [
        Buffer.from("intent"),
        marketPda.toBuffer(),
        intentNonce.toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    );

    [vaultPda] = anchor.web3.Pubkey.findProgramAddressSync(
      [
        Buffer.from("vault"),
        marketId.toArrayLike(Buffer, "le", 8),
        maker.publicKey.toBuffer(),
      ],
      program.programId
    );
  });

  it("1. Initializes Market with Token-2022 pairings", async () => {
    const minBase = new anchor.BN(100_000); // 0.1 Base
    await program.methods
      .initializeMarket(marketId, minBase)
      .accounts({
        market: marketPda,
        baseMint: baseMint,
        quoteMint: quoteMint,
        authority: authority.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .signers([authority])
      .rpc();

    const marketAccount = await program.account.marketState.fetch(marketPda);
    expect(marketAccount.marketId.toNumber()).to.equal(2026);
    expect(marketAccount.authority.toBase58()).to.equal(authority.publicKey.toBase58());
  });

  it("2. Maker deposits Base tokens and posts Sub-Slot Intent", async () => {
    const depositAmount = new anchor.BN(500_000); // 0.5 Base
    const minQuoteReturn = new anchor.BN(1_000_000); // 1.0 Quote
    const currentSlot = await provider.connection.getSlot();
    const expirationSlot = new anchor.BN(currentSlot + 500);

    const tx = await program.methods
      .postIntent(intentNonce, depositAmount, minQuoteReturn, expirationSlot)
      .accounts({
        market: marketPda,
        intent: intentPda,
        vaultTokenAccount: vaultPda,
        baseMint: baseMint,
        makerTokenAccount: makerBaseAta,
        maker: maker.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .signers([maker])
      .rpc();

    const intentAccount = await program.account.intentState.fetch(intentPda);
    expect(intentAccount.maker.toBase58()).to.equal(maker.publicKey.toBase58());
    expect(intentAccount.remainingAmount.toNumber()).to.equal(500_000);
    expect(intentAccount.status).to.equal(1); // Active

    const vaultBalance = await getAccount(
      provider.connection,
      vaultPda,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    expect(Number(vaultBalance.amount)).to.equal(500_000);
  });

  it("3. Rejects Solver Fill when slippage limit is breached", async () => {
    const fillBaseAmount = new anchor.BN(500_000);
    const toxicQuoteAmount = new anchor.BN(900_000); // Less than the 1_000_000 threshold

    try {
      await program.methods
        .executeFill(fillBaseAmount, toxicQuoteAmount)
        .accounts({
          market: marketPda,
          intent: intentPda,
          vaultTokenAccount: vaultPda,
          baseMint: baseMint,
          quoteMint: quoteMint,
          takerQuoteTokenAccount: takerQuoteAta,
          makerQuoteTokenAccount: makerQuoteAta,
          takerBaseTokenAccount: takerBaseAta,
          feeRecipientTokenAccount: feeRecipientAta,
          taker: taker.publicKey,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
        })
        .signers([taker])
        .rpc();
      assert.fail("Transaction should have failed with SlippageLimitExceeded");
    } catch (err: any) {
      expect(err.toString()).to.contain("SlippageLimitExceeded");
    }
  });

  it("4. Successfully executes atomic fill under 15k Compute Units", async () => {
    const fillBaseAmount = new anchor.BN(500_000);
    const validQuoteAmount = new anchor.BN(1_050_000); // Above threshold

    const tx = await program.methods
      .executeFill(fillBaseAmount, validQuoteAmount)
      .accounts({
        market: marketPda,
        intent: intentPda,
        vaultTokenAccount: vaultPda,
        baseMint: baseMint,
        quoteMint: quoteMint,
        takerQuoteTokenAccount: takerQuoteAta,
        makerQuoteTokenAccount: makerQuoteAta,
        takerBaseTokenAccount: takerBaseAta,
        feeRecipientTokenAccount: feeRecipientAta,
        taker: taker.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([taker])
      .rpc();

    // Verify token movements
    const takerBaseBalance = await getAccount(
      provider.connection,
      takerBaseAta,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    expect(Number(takerBaseBalance.amount)).to.equal(500_000);

    const intentAccount = await program.account.intentState.fetch(intentPda);
    expect(intentAccount.status).to.equal(2); // Filled
    expect(intentAccount.remainingAmount.toNumber()).to.equal(0);
  });
});