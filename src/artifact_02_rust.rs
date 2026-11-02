use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("BP26SettleEngine111111111111111111111111111111");

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"settlement_vault";
pub const RECEIPT_SEED: &[u8] = b"receipt";
pub const MAX_BATCH_FILLS: usize = 8;

#[program]
pub mod breakpoint_settler {
    use super::*;

    /// Initializes the Aegis global settlement state.
    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        max_slippage_bps: u16,
        protocol_fee_bps: u16,
    ) -> Result<()> {
        require!(max_slippage_bps <= 1_000, SettlementError::ExcessiveSlippage); // Max 10%
        require!(protocol_fee_bps <= 500, SettlementError::ExcessiveProtocolFee); // Max 5%

        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.settlement_authority = ctx.accounts.settlement_authority.key();
        config.fee_recipient = ctx.accounts.fee_recipient.key();
        config.max_slippage_bps = max_slippage_bps;
        config.protocol_fee_bps = protocol_fee_bps;
        config.total_settled_batches = 0;
        config.bump = ctx.bumps.config;

        emit!(MarketInitializedEvent {
            admin: config.admin,
            settlement_authority: config.settlement_authority,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Deposits liquidity into the escrow vault bound to user authority and intent nonce.
    pub fn escrow_deposit(
        ctx: Context<EscrowDeposit>,
        nonce: u64,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, SettlementError::ZeroDepositAmount);

        let vault = &mut ctx.accounts.escrow_vault;
        vault.owner = ctx.accounts.depositor.key();
        vault.mint = ctx.accounts.mint.key();
        vault.nonce = nonce;
        vault.escrow_amount = vault.escrow_amount.checked_add(amount).ok_or(SettlementError::NumericalOverflow)?;
        vault.created_at = Clock::get()?.unix_timestamp;
        vault.bump = ctx.bumps.escrow_vault;

        // Execute CPI to Token-2022 / Standard Token Program
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.depositor_token_account.to_account_info(),
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.vault_token_account.to_account_info(),
            authority: ctx.accounts.depositor.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token_interface::transfer_checked(cpi_ctx, amount, ctx.accounts.mint.decimals)?;

        emit!(EscrowDepositedEvent {
            depositor: ctx.accounts.depositor.key(),
            nonce,
            amount,
        });

        Ok(())
    }

    /// Settles a batch of off-chain intents against deposited liquidity via Merkle proof validation.
    pub fn settle_batch_intents(
        ctx: Context<SettleBatchIntents>,
        batch_id: u64,
        root: [u8; 32],
        proofs: Vec<[u8; 32]>,
        fills: Vec<IntentFill>,
    ) -> Result<()> {
        require!(!fills.is_empty(), SettlementError::EmptyBatch);
        require!(fills.len() <= MAX_BATCH_FILLS, SettlementError::BatchTooLarge);
        require!(proofs.len() <= 16, SettlementError::ProofTooLarge);

        let config = &ctx.accounts.config;
        let receipt = &mut ctx.accounts.fill_receipt;
        
        // Prevent batch replay attacks
        require!(!receipt.is_processed, SettlementError::BatchAlreadyProcessed);
        receipt.is_processed = true;
        receipt.batch_id = batch_id;
        receipt.settled_at = Clock::get()?.unix_timestamp;
        receipt.bump = ctx.bumps.fill_receipt;

        // Verify cryptographic intent commitments
        let mut computed_hash = hash_intent_fill(&fills[0]);
        for proof_element in proofs.iter() {
            computed_hash = if computed_hash <= *proof_element {
                anchor_lang::solana_program::keccak::hashv(&[&computed_hash, proof_element]).0
            } else {
                anchor_lang::solana_program::keccak::hashv(&[proof_element, &computed_hash]).0
            };
        }
        require!(computed_hash == root, SettlementError::InvalidMerkleProof);

        // Execute batch fills atomically under escrow PDA authority
        let auth_key = config.settlement_authority;
        let nonce_bytes = ctx.accounts.escrow_vault.nonce.to_le_bytes();
        let seeds = &[
            VAULT_SEED,
            auth_key.as_ref(),
            nonce_bytes.as_ref(),
            &[ctx.accounts.escrow_vault.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        let mut total_transferred: u64 = 0;

        for fill in fills.iter() {
            require!(fill.amount_in > 0, SettlementError::InvalidFillAmount);
            
            // Check protocol fee calculation
            let fee = (fill.amount_in as u128)
                .checked_mul(config.protocol_fee_bps as u128)
                .ok_or(SettlementError::NumericalOverflow)?
                .checked_div(10_000)
                .ok_or(SettlementError::NumericalOverflow)? as u64;

            let net_payout = fill.amount_in.checked_sub(fee).ok_or(SettlementError::NumericalOverflow)?;

            // CPI transfer from Escrow Token Account to Taker Token Account
            let transfer_accounts = TransferChecked {
                from: ctx.accounts.vault_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.recipient_token_account.to_account_info(),
                authority: ctx.accounts.escrow_vault.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                transfer_accounts,
                signer_seeds,
            );
            token_interface::transfer_checked(cpi_ctx, net_payout, ctx.accounts.mint.decimals)?;

            // Fee transfer if non-zero
            if fee > 0 {
                let fee_accounts = TransferChecked {
                    from: ctx.accounts.vault_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.fee_token_account.to_account_info(),
                    authority: ctx.accounts.escrow_vault.to_account_info(),
                };
                let fee_cpi_ctx = CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    fee_accounts,
                    signer_seeds,
                );
                token_interface::transfer_checked(fee_cpi_ctx, fee, ctx.accounts.mint.decimals)?;
            }

            total_transferred = total_transferred
                .checked_add(fill.amount_in)
                .ok_or(SettlementError::NumericalOverflow)?;
        }

        // Decrement escrow vault balance
        let vault = &mut ctx.accounts.escrow_vault;
        vault.escrow_amount = vault.escrow_amount
            .checked_sub(total_transferred)
            .ok_or(SettlementError::InsufficientEscrowBalance)?;

        // Increment total processed batches
        let config_mut = &mut ctx.accounts.config;
        config_mut.total_settled_batches = config_mut.total_settled_batches
            .checked_add(1)
            .ok_or(SettlementError::NumericalOverflow)?;

        emit!(BatchSettledEvent {
            batch_id,
            total_transferred,
            settled_by: ctx.accounts.settlement_authority.key(),
        });

        Ok(())
    }

    /// Emergency reclaiming of funds after escrow lock expiration (anti-rug protection).
    pub fn reclaim_expired(ctx: Context<ReclaimExpired>) -> Result<()> {
        let vault = &ctx.accounts.escrow_vault;
        let clock = Clock::get()?;
        let expiry_time = vault.created_at.checked_add(86_400).ok_or(SettlementError::NumericalOverflow)?; // 24hr TTL

        require!(clock.unix_timestamp >= expiry_time, SettlementError::EscrowNotExpired);
        let reclaim_amount = vault.escrow_amount;
        require!(reclaim_amount > 0, SettlementError::ZeroDepositAmount);

        let auth_key = ctx.accounts.config.settlement_authority;
        let nonce_bytes = vault.nonce.to_le_bytes();
        let seeds = &[
            VAULT_SEED,
            auth_key.as_ref(),
            nonce_bytes.as_ref(),
            &[vault.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        let transfer_accounts = TransferChecked {
            from: ctx.accounts.vault_token_account.to_account_info(),
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.depositor_token_account.to_account_info(),
            authority: ctx.accounts.escrow_vault.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            transfer_accounts,
            signer_seeds,
        );
        token_interface::transfer_checked(cpi_ctx, reclaim_amount, ctx.accounts.mint.decimals)?;

        let vault_mut = &mut ctx.accounts.escrow_vault;
        vault_mut.escrow_amount = 0;

        emit!(EscrowReclaimedEvent {
            owner: vault_mut.owner,
            amount: reclaim_amount,
        });

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT VALIDATION STRUCTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeMarket<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + GlobalSettlementConfig::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, GlobalSettlementConfig>,
    /// CHECK: Validated as settlement authority key in config
    pub settlement_authority: Signer<'info>,
    /// CHECK: Arbitrary fee receiver verified by admin
    pub fee_recipient: AccountInfo<'info>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(nonce: u64)]
pub struct EscrowDeposit<'info> {
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump
    )]
    pub config: Account<'info, GlobalSettlementConfig>,
    #[account(
        init_if_needed,
        payer = depositor,
        space = 8 + EscrowVault::INIT_SPACE,
        seeds = [VAULT_SEED, config.settlement_authority.key().as_ref(), nonce.to_le_bytes().as_ref()],
        bump
    )]
    pub escrow_vault: Account<'info, EscrowVault>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = depositor,
        token::token_program = token_program,
    )]
    pub depositor_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = escrow_vault,
        token::token_program = token_program,
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub depositor: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(batch_id: u64)]
pub struct SettleBatchIntents<'info> {
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        has_one = settlement_authority @ SettlementError::UnauthorizedSettler
    )]
    pub config: Account<'info, GlobalSettlementConfig>,
    pub settlement_authority: Signer<'info>,
    #[account(
        mut,
        seeds = [VAULT_SEED, config.settlement_authority.key().as_ref(), escrow_vault.nonce.to_le_bytes().as_ref()],
        bump = escrow_vault.bump,
    )]
    pub escrow_vault: Account<'info, EscrowVault>,
    #[account(
        init,
        payer = settlement_authority,
        space = 8 + FillReceipt::INIT_SPACE,
        seeds = [RECEIPT_SEED, batch_id.to_le_bytes().as_ref()],
        bump
    )]
    pub fill_receipt: Account<'info, FillReceipt>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = escrow_vault,
        token::token_program = token_program,
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = mint,
        token::token_program = token_program,
    )]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = config.fee_recipient,
        token::token_program = token_program,
    )]
    pub fee_token_account: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ReclaimExpired<'info> {
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump
    )]
    pub config: Account<'info, GlobalSettlementConfig>,
    #[account(
        mut,
        seeds = [VAULT_SEED, config.settlement_authority.key().as_ref(), escrow_vault.nonce.to_le_bytes().as_ref()],
        bump = escrow_vault.bump,
        has_one = owner @ SettlementError::UnauthorizedReclaim
    )]
    pub escrow_vault: Account<'info, EscrowVault>,
    pub owner: Signer<'info>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = escrow_vault,
        token::token_program = token_program,
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = owner,
        token::token_program = token_program,
    )]
    pub depositor_token_account: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

// -----------------------------------------------------------------------------
// STATE STRUCTS
// -----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct GlobalSettlementConfig {
    pub admin: Pubkey,
    pub settlement_authority: Pubkey,
    pub fee_recipient: Pubkey,
    pub max_slippage_bps: u16,
    pub protocol_fee_bps: u16,
    pub total_settled_batches: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct EscrowVault {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub nonce: u64,
    pub escrow_amount: u64,
    pub created_at: i64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct FillReceipt {
    pub batch_id: u64,
    pub settled_at: i64,
    pub is_processed: bool,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct IntentFill {
    pub maker: Pubkey,
    pub taker: Pubkey,
    pub amount_in: u64,
    pub min_amount_out: u64,
    pub expiry: i64,
}

// -----------------------------------------------------------------------------
// HELPER FUNCTIONS & ERROR CODES
// -----------------------------------------------------------------------------

pub fn hash_intent_fill(fill: &IntentFill) -> [u8; 32] {
    anchor_lang::solana_program::keccak::hashv(&[
        fill.maker.as_ref(),
        fill.taker.as_ref(),
        &fill.amount_in.to_le_bytes(),
        &fill.min_amount_out.to_le_bytes(),
        &fill.expiry.to_le_bytes(),
    ]).0
}

#[event]
pub struct MarketInitializedEvent {
    pub admin: Pubkey,
    pub settlement_authority: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct EscrowDepositedEvent {
    pub depositor: Pubkey,
    pub nonce: u64,
    pub amount: u64,
}

#[event]
pub struct BatchSettledEvent {
    pub batch_id: u64,
    pub total_transferred: u64,
    pub settled_by: Pubkey,
}

#[event]
pub struct EscrowReclaimedEvent {
    pub owner: Pubkey,
    pub amount: u64,
}

#[error_code]
pub enum SettlementError {
    #[msg("Max slippage bps exceeded permissible boundary (1000 bps)")]
    ExcessiveSlippage,
    #[msg("Protocol fee bps exceeded permissible boundary (500 bps)")]
    ExcessiveProtocolFee,
    #[msg("Deposit amount must be greater than zero")]
    ZeroDepositAmount,
    #[msg("Arithmetic calculation resulted in numerical overflow")]
    NumericalOverflow,
    #[msg("Provided batch is empty")]
    EmptyBatch,
    #[msg("Batch fill count exceeds MAX_BATCH_FILLS limit")]
    BatchTooLarge,
    #[msg("Merkle proof verification exceeded max tree depth")]
    ProofTooLarge,
    #[msg("Batch receipt already recorded: replay attack prevented")]
    BatchAlreadyProcessed,
    #[msg("Cryptographic Merkle proof mismatch for submitted intent fill")]
    InvalidMerkleProof,
    #[msg("Fill amount cannot be zero")]
    InvalidFillAmount,
    #[msg("Insufficient escrow vault balance to satisfy batch fill")]
    InsufficientEscrowBalance,
    #[msg("Escrow lock time has not yet expired for emergency reclaim")]
    EscrowNotExpired,
    #[msg("Caller signature does not match designated settlement authority")]
    UnauthorizedSettler,
    #[msg("Caller is not the designated escrow owner")]
    UnauthorizedReclaim,
}