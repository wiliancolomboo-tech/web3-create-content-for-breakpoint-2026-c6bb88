use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, Token2022, TokenAccount, TransferChecked};

declare_id!("ChronoStrike11111111111111111111111111111111");

pub const MARKET_SEED: &[u8] = b"market";
pub const VAULT_SEED: &[u8] = b"vault";
pub const INTENT_SEED: &[u8] = b"intent";

pub const MAX_INTENTS_PER_BATCH: usize = 8;
pub const PROTOCOL_FEE_BPS: u64 = 15; // 0.15% fee
pub const BPS_DENOMINATOR: u64 = 10_000;

#[program]
pub mod chrono_strike {
    use super::*;

    /// Initializes a new market instance with dedicated Base & Quote mints (Token-2022 native).
    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        market_id: u64,
        min_base_amount: u64,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        market.authority = ctx.accounts.authority.key();
        market.market_id = market_id;
        market.base_mint = ctx.accounts.base_mint.key();
        market.quote_mint = ctx.accounts.quote_mint.key();
        market.min_base_amount = min_base_amount;
        market.total_volume = 0;
        market.bump = ctx.bumps.market;
        market.is_paused = 0;

        emit!(MarketInitializedEvent {
            market_id,
            authority: market.authority,
            base_mint: market.base_mint,
            quote_mint: market.quote_mint,
        });

        Ok(())
    }

    /// Deposits base tokens and registers a cryptographic intent to sell at a limit price.
    pub fn post_intent(
        ctx: Context<PostIntent>,
        intent_nonce: u64,
        amount_in: u64,
        min_amount_out: u64,
        expiration_slot: u64,
    ) -> Result<()> {
        require!(ctx.accounts.market.is_paused == 0, ChronoError::MarketPaused);
        require!(
            amount_in >= ctx.accounts.market.min_base_amount,
            ChronoError::AmountBelowMinimum
        );
        require!(
            expiration_slot > Clock::get()?.slot,
            ChronoError::IntentAlreadyExpired
        );

        // Populate Intent PDA data structure
        let intent = &mut ctx.accounts.intent;
        intent.maker = ctx.accounts.maker.key();
        intent.market = ctx.accounts.market.key();
        intent.intent_nonce = intent_nonce;
        intent.amount_in = amount_in;
        intent.remaining_amount = amount_in;
        intent.min_amount_out = min_amount_out;
        intent.expiration_slot = expiration_slot;
        intent.bump = ctx.bumps.intent;
        intent.status = IntentStatus::Active as u8;

        // Execute CPI to SPL Token-2022 program to safely escrow maker tokens
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.maker_token_account.to_account_info(),
            mint: ctx.accounts.base_mint.to_account_info(),
            to: ctx.accounts.vault_token_account.to_account_info(),
            authority: ctx.accounts.maker.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token_interface::transfer_checked(cpi_ctx, amount_in, ctx.accounts.base_mint.decimals)?;

        emit!(IntentPostedEvent {
            market_id: ctx.accounts.market.market_id,
            maker: intent.maker,
            intent_nonce,
            amount_in,
            min_amount_out,
            expiration_slot,
        });

        Ok(())
    }

    /// Solver fills an active intent. Performs atomic transfers of quote and base tokens.
    pub fn execute_fill(
        ctx: Context<ExecuteFill>,
        taker_fill_base_amount: u64,
        taker_offered_quote_amount: u64,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(market.is_paused == 0, ChronoError::MarketPaused);

        let intent = &mut ctx.accounts.intent;
        let current_slot = Clock::get()?.slot;

        require!(
            intent.status == IntentStatus::Active as u8,
            ChronoError::IntentNotActive
        );
        require!(
            current_slot <= intent.expiration_slot,
            ChronoError::IntentExpired
        );
        require!(
            taker_fill_base_amount <= intent.remaining_amount,
            ChronoError::ExcessiveFillAmount
        );

        // Verify minimum acceptable price: (offered_quote / base_amount) >= (min_amount_out / total_amount_in)
        // Expressed with cross-multiplication: offered_quote * intent.amount_in >= min_amount_out * base_amount
        let quote_lhs = (taker_offered_quote_amount as u128)
            .checked_mul(intent.amount_in as u128)
            .ok_or(ChronoError::MathOverflow)?;
        let quote_rhs = (intent.min_amount_out as u128)
            .checked_mul(taker_fill_base_amount as u128)
            .ok_or(ChronoError::MathOverflow)?;
        require!(quote_lhs >= quote_rhs, ChronoError::SlippageLimitExceeded);

        // Deduct remaining allocation
        intent.remaining_amount = intent
            .remaining_amount
            .checked_sub(taker_fill_base_amount)
            .ok_or(ChronoError::MathUnderflow)?;

        if intent.remaining_amount == 0 {
            intent.status = IntentStatus::Filled as u8;
        }

        // Calculate fee
        let fee_amount = taker_offered_quote_amount
            .checked_mul(PROTOCOL_FEE_BPS)
            .ok_or(ChronoError::MathOverflow)?
            / BPS_DENOMINATOR;
        let net_quote_to_maker = taker_offered_quote_amount
            .checked_sub(fee_amount)
            .ok_or(ChronoError::MathUnderflow)?;

        // 1. Taker transfers Quote token to Maker
        let cpi_taker_to_maker = TransferChecked {
            from: ctx.accounts.taker_quote_token_account.to_account_info(),
            mint: ctx.accounts.quote_mint.to_account_info(),
            to: ctx.accounts.maker_quote_token_account.to_account_info(),
            authority: ctx.accounts.taker.to_account_info(),
        };
        token_interface::transfer_checked(
            CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_taker_to_maker),
            net_quote_to_maker,
            ctx.accounts.quote_mint.decimals,
        )?;

        // 2. Taker pays Quote fee to Market Fee Receiver
        if fee_amount > 0 {
            let cpi_fee = TransferChecked {
                from: ctx.accounts.taker_quote_token_account.to_account_info(),
                mint: ctx.accounts.quote_mint.to_account_info(),
                to: ctx.accounts.fee_recipient_token_account.to_account_info(),
                authority: ctx.accounts.taker.to_account_info(),
            };
            token_interface::transfer_checked(
                CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_fee),
                fee_amount,
                ctx.accounts.quote_mint.decimals,
            )?;
        }

        // 3. Vault releases Escrowed Base token to Taker using Market Seeds
        let market_id_bytes = market.market_id.to_le_bytes();
        let maker_key_ref = intent.maker;
        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            market_id_bytes.as_ref(),
            maker_key_ref.as_ref(),
            &[ctx.bumps.vault_token_account],
        ];
        let signer = &[&seeds[..]];

        let cpi_escrow_to_taker = TransferChecked {
            from: ctx.accounts.vault_token_account.to_account_info(),
            mint: ctx.accounts.base_mint.to_account_info(),
            to: ctx.accounts.taker_base_token_account.to_account_info(),
            authority: ctx.accounts.vault_token_account.to_account_info(),
        };
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_escrow_to_taker,
                signer,
            ),
            taker_fill_base_amount,
            ctx.accounts.base_mint.decimals,
        )?;

        // Update protocol metrics
        market.total_volume = market
            .total_volume
            .checked_add(taker_offered_quote_amount)
            .ok_or(ChronoError::MathOverflow)?;

        emit!(FillExecutedEvent {
            market_id: market.market_id,
            intent_nonce: intent.intent_nonce,
            taker: ctx.accounts.taker.key(),
            filled_base: taker_fill_base_amount,
            quote_paid: taker_offered_quote_amount,
            remaining_base: intent.remaining_amount,
        });

        Ok(())
    }

    /// Cancel an expired or partially filled intent and refund remaining escrow to Maker.
    pub fn cancel_intent(ctx: Context<CancelIntent>) -> Result<()> {
        let intent = &mut ctx.accounts.intent;
        let current_slot = Clock::get()?.slot;

        require!(
            intent.status == IntentStatus::Active as u8,
            ChronoError::IntentNotActive
        );
        require!(
            current_slot > intent.expiration_slot || ctx.accounts.maker.key() == intent.maker,
            ChronoError::UnauthorizedCancel
        );

        let refund_amount = intent.remaining_amount;
        require!(refund_amount > 0, ChronoError::ZeroRefundBalance);

        intent.remaining_amount = 0;
        intent.status = IntentStatus::Cancelled as u8;

        let market_id_bytes = ctx.accounts.market.market_id.to_le_bytes();
        let maker_key = intent.maker;
        let seeds: &[&[u8]] = &[
            VAULT_SEED,
            market_id_bytes.as_ref(),
            maker_key.as_ref(),
            &[ctx.bumps.vault_token_account],
        ];
        let signer = &[&seeds[..]];

        let cpi_refund = TransferChecked {
            from: ctx.accounts.vault_token_account.to_account_info(),
            mint: ctx.accounts.base_mint.to_account_info(),
            to: ctx.accounts.maker_base_token_account.to_account_info(),
            authority: ctx.accounts.vault_token_account.to_account_info(),
        };
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_refund,
                signer,
            ),
            refund_amount,
            ctx.accounts.base_mint.decimals,
        )?;

        emit!(IntentCancelledEvent {
            intent_nonce: intent.intent_nonce,
            maker: intent.maker,
            refunded_amount: refund_amount,
        });

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT STRUCTS & ZERO-COPY DEFINITIONS
// -----------------------------------------------------------------------------

#[repr(u8)]
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum IntentStatus {
    Uninitialized = 0,
    Active = 1,
    Filled = 2,
    Cancelled = 3,
}

#[account]
#[derive(InitSpace)]
pub struct MarketState {
    pub authority: Pubkey,       // 32
    pub base_mint: Pubkey,       // 32
    pub quote_mint: Pubkey,      // 32
    pub market_id: u64,          // 8
    pub min_base_amount: u64,    // 8
    pub total_volume: u64,       // 8
    pub is_paused: u8,           // 1
    pub bump: u8,                // 1
}

#[account]
#[derive(InitSpace)]
pub struct IntentState {
    pub maker: Pubkey,           // 32
    pub market: Pubkey,          // 32
    pub intent_nonce: u64,       // 8
    pub amount_in: u64,          // 8
    pub remaining_amount: u64,   // 8
    pub min_amount_out: u64,     // 8
    pub expiration_slot: u64,    // 8
    pub status: u8,              // 1
    pub bump: u8,                // 1
}

// -----------------------------------------------------------------------------
// VALIDATION CONTEXTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct InitializeMarket<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + MarketState::INIT_SPACE,
        seeds = [MARKET_SEED, &market_id.to_le_bytes()],
        bump
    )]
    pub market: Account<'info, MarketState>,

    pub base_mint: InterfaceAccount<'info, Mint>,
    pub quote_mint: InterfaceAccount<'info, Mint>,

    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(intent_nonce: u64)]
pub struct PostIntent<'info> {
    #[account(
        mut,
        seeds = [MARKET_SEED, &market.market_id.to_le_bytes()],
        bump = market.bump
    )]
    pub market: Account<'info, MarketState>,

    #[account(
        init,
        payer = maker,
        space = 8 + IntentState::INIT_SPACE,
        seeds = [INTENT_SEED, market.key().as_ref(), &intent_nonce.to_le_bytes()],
        bump
    )]
    pub intent: Account<'info, IntentState>,

    #[account(
        init_if_needed,
        payer = maker,
        seeds = [VAULT_SEED, &market.market_id.to_le_bytes(), maker.key().as_ref()],
        bump,
        token::mint = base_mint,
        token::authority = vault_token_account,
        token::token_program = token_program
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut, address = market.base_mint)]
    pub base_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = maker_token_account.mint == market.base_mint @ ChronoError::InvalidMint,
        constraint = maker_token_account.owner == maker.key() @ ChronoError::InvalidAccountOwner
    )]
    pub maker_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub maker: Signer<'info>,

    pub token_program: Interface<'info, Token2022>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ExecuteFill<'info> {
    #[account(
        mut,
        seeds = [MARKET_SEED, &market.market_id.to_le_bytes()],
        bump = market.bump
    )]
    pub market: Account<'info, MarketState>,

    #[account(
        mut,
        seeds = [INTENT_SEED, market.key().as_ref(), &intent.intent_nonce.to_le_bytes()],
        bump = intent.bump,
        constraint = intent.market == market.key() @ ChronoError::MarketMismatch
    )]
    pub intent: Account<'info, IntentState>,

    #[account(
        mut,
        seeds = [VAULT_SEED, &market.market_id.to_le_bytes(), intent.maker.as_ref()],
        bump
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(address = market.base_mint)]
    pub base_mint: InterfaceAccount<'info, Mint>,

    #[account(address = market.quote_mint)]
    pub quote_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = taker_quote_token_account.mint == market.quote_mint @ ChronoError::InvalidMint,
        constraint = taker_quote_token_account.owner == taker.key() @ ChronoError::InvalidAccountOwner
    )]
    pub taker_quote_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = maker_quote_token_account.mint == market.quote_mint @ ChronoError::InvalidMint,
        constraint = maker_quote_token_account.owner == intent.maker @ ChronoError::InvalidAccountOwner
    )]
    pub maker_quote_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = taker_base_token_account.mint == market.base_mint @ ChronoError::InvalidMint,
        constraint = taker_base_token_account.owner == taker.key() @ ChronoError::InvalidAccountOwner
    )]
    pub taker_base_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = fee_recipient_token_account.mint == market.quote_mint @ ChronoError::InvalidMint
    )]
    pub fee_recipient_token_account: InterfaceAccount<'info, TokenAccount>,

    pub taker: Signer<'info>,
    pub token_program: Interface<'info, Token2022>,
}

#[derive(Accounts)]
pub struct CancelIntent<'info> {
    #[account(
        seeds = [MARKET_SEED, &market.market_id.to_le_bytes()],
        bump = market.bump
    )]
    pub market: Account<'info, MarketState>,

    #[account(
        mut,
        seeds = [INTENT_SEED, market.key().as_ref(), &intent.intent_nonce.to_le_bytes()],
        bump = intent.bump,
        constraint = intent.market == market.key() @ ChronoError::MarketMismatch
    )]
    pub intent: Account<'info, IntentState>,

    #[account(
        mut,
        seeds = [VAULT_SEED, &market.market_id.to_le_bytes(), intent.maker.as_ref()],
        bump
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(address = market.base_mint)]
    pub base_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = maker_base_token_account.mint == market.base_mint @ ChronoError::InvalidMint,
        constraint = maker_base_token_account.owner == intent.maker @ ChronoError::InvalidAccountOwner
    )]
    pub maker_base_token_account: InterfaceAccount<'info, TokenAccount>,

    pub maker: Signer<'info>,
    pub token_program: Interface<'info, Token2022>,
}

// -----------------------------------------------------------------------------
// EVENTS & ERRORS
// -----------------------------------------------------------------------------

#[event]
pub struct MarketInitializedEvent {
    pub market_id: u64,
    pub authority: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
}

#[event]
pub struct IntentPostedEvent {
    pub market_id: u64,
    pub maker: Pubkey,
    pub intent_nonce: u64,
    pub amount_in: u64,
    pub min_amount_out: u64,
    pub expiration_slot: u64,
}

#[event]
pub struct FillExecutedEvent {
    pub market_id: u64,
    pub intent_nonce: u64,
    pub taker: Pubkey,
    pub filled_base: u64,
    pub quote_paid: u64,
    pub remaining_base: u64,
}

#[event]
pub struct IntentCancelledEvent {
    pub intent_nonce: u64,
    pub maker: Pubkey,
    pub refunded_amount: u64,
}

#[error_code]
pub enum ChronoError {
    #[msg("Market is currently paused by authority.")]
    MarketPaused,
    #[msg("Base deposit is lower than configured market minimum.")]
    AmountBelowMinimum,
    #[msg("Target expiration slot is already in the past.")]
    IntentAlreadyExpired,
    #[msg("Intent is not in Active status.")]
    IntentNotActive,
    #[msg("Expiration slot has elapsed.")]
    IntentExpired,
    #[msg("Requested fill exceeds available intent remaining amount.")]
    ExcessiveFillAmount,
    #[msg("Offered quote amount breaches strict slippage boundary.")]
    SlippageLimitExceeded,
    #[msg("Arithmetic overflow during rate calculation.")]
    MathOverflow,
    #[msg("Arithmetic underflow during balance decrement.")]
    MathUnderflow,
    #[msg("Caller unauthorized to cancel active intent before expiry.")]
    UnauthorizedCancel,
    #[msg("Intent vault has zero remaining balance.")]
    ZeroRefundBalance,
    #[msg("Account mint does not correspond to registered market mint.")]
    InvalidMint,
    #[msg("Account owner does not match required key.")]
    InvalidAccountOwner,
    #[msg("Intent state does not match provided market.")]
    MarketMismatch,
}