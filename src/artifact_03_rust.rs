use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("HypRDrive1111111111111111111111111111111111");

pub const MAX_INTENTS_PER_BATCH: usize = 64;
pub const BATCH_EPOCH_SLOT_DURATION: u64 = 10; // ~4 seconds per settlement batch

#[program]
pub mod hyperdrive {
    use super::*;

    /// Initialize the HyperDrive global configuration.
    pub fn initialize_engine(
        ctx: Context<InitializeEngine>,
        protocol_fee_bps: u16,
        operator: Pubkey,
    ) -> Result<()> {
        require!(protocol_fee_bps <= 1000, HyperDriveError::FeeTooHigh); // Max 10%
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.operator = operator;
        config.protocol_fee_bps = protocol_fee_bps;
        config.current_epoch = Clock::get()?.slot / BATCH_EPOCH_SLOT_DURATION;
        config.is_paused = false;
        config.bump = ctx.bumps.config;
        Ok(())
    }

    /// Initialize an empty zero-copy batch buffer for the designated epoch.
    pub fn initialize_batch(ctx: Context<InitializeBatch>, epoch_id: u64) -> Result<()> {
        let mut buffer = ctx.accounts.batch_buffer.load_init()?;
        buffer.epoch_id = epoch_id;
        buffer.intent_count = 0;
        buffer.total_input_amount = 0;
        buffer.settled = 0;
        buffer.bump = ctx.bumps.batch_buffer;
        Ok(())
    }

    /// Submit a user swap intent to the active epoch batch buffer.
    pub fn submit_intent(
        ctx: Context<SubmitIntent>,
        amount_in: u64,
        min_amount_out: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.config.is_paused, HyperDriveError::ProtocolPaused);
        require!(amount_in > 0, HyperDriveError::InvalidAmount);

        let current_slot = Clock::get()?.slot;
        let active_epoch = current_slot / BATCH_EPOCH_SLOT_DURATION;

        let mut buffer = ctx.accounts.batch_buffer.load_mut()?;
        require!(buffer.epoch_id == active_epoch, HyperDriveError::BatchEpochMismatch);
        require!(
            (buffer.intent_count as usize) < MAX_INTENTS_PER_BATCH,
            HyperDriveError::BatchBufferFull
        );
        require!(buffer.settled == 0, HyperDriveError::BatchAlreadySettled);

        // Append intent into zero-copy array
        let index = buffer.intent_count as usize;
        buffer.intents[index] = OrderIntent {
            user: ctx.accounts.user.key(),
            amount_in,
            min_amount_out,
            slot_submitted: current_slot,
            flags: 1, // Active
        };
        buffer.intent_count += 1;
        buffer.total_input_amount = buffer
            .total_input_amount
            .checked_add(amount_in)
            .ok_or(HyperDriveError::MathOverflow)?;

        drop(buffer);

        // CPI: Transfer tokens to Escrow Vault using Token-2022 / SPL compatible interface
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.user_token_in.to_account_info(),
            mint: ctx.accounts.mint_in.to_account_info(),
            to: ctx.accounts.vault_token_in.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token_interface::transfer_checked(cpi_ctx, amount_in, ctx.accounts.mint_in.decimals)?;

        emit!(IntentSubmittedEvent {
            epoch_id: active_epoch,
            user: ctx.accounts.user.key(),
            amount_in,
            min_amount_out,
            slot: current_slot,
        });

        Ok(())
    }

    /// Keeper clears and executes aggregated settlement at uniform price.
    pub fn batch_settle(
        ctx: Context<BatchSettle>,
        epoch_id: u64,
        clearing_rate_num: u64, // Output per 1e9 units of Input
        clearing_rate_denom: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.config.is_paused, HyperDriveError::ProtocolPaused);
        require!(clearing_rate_denom > 0, HyperDriveError::DivideByZero);

        let current_slot = Clock::get()?.slot;
        let active_epoch = current_slot / BATCH_EPOCH_SLOT_DURATION;
        require!(epoch_id < active_epoch, HyperDriveError::EpochStillActive);

        let mut buffer = ctx.accounts.batch_buffer.load_mut()?;
        require!(buffer.epoch_id == epoch_id, HyperDriveError::BatchEpochMismatch);
        require!(buffer.settled == 0, HyperDriveError::BatchAlreadySettled);
        require!(buffer.intent_count > 0, HyperDriveError::EmptyBatch);

        let mut cumulative_out: u64 = 0;
        let intent_count = buffer.intent_count as usize;

        for i in 0..intent_count {
            let intent = &buffer.intents[i];
            if intent.flags == 1 {
                // Compute proportional output: (amount_in * clearing_rate_num) / clearing_rate_denom
                let expected_out = (intent.amount_in as u128)
                    .checked_mul(clearing_rate_num as u128)
                    .ok_or(HyperDriveError::MathOverflow)?
                    .checked_div(clearing_rate_denom as u128)
                    .ok_or(HyperDriveError::MathOverflow)? as u64;

                require!(
                    expected_out >= intent.min_amount_out,
                    HyperDriveError::SlippageExceeded
                );

                cumulative_out = cumulative_out
                    .checked_add(expected_out)
                    .ok_or(HyperDriveError::MathOverflow)?;
            }
        }

        buffer.settled = 1;
        drop(buffer);

        // Protocol & Keeper Incentive Cut Calculation
        let keeper_rebate = cumulative_out
            .checked_mul(50) // 0.5% base execution rebate
            .ok_or(HyperDriveError::MathOverflow)?
            .checked_div(10000)
            .ok_or(HyperDriveError::MathOverflow)?;

        let config_bump = ctx.accounts.config.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[b"config".as_ref(), &[config_bump]]];

        // Disburse Keeper Incentive
        if keeper_rebate > 0 {
            let keeper_cpi = TransferChecked {
                from: ctx.accounts.settlement_source_out.to_account_info(),
                mint: ctx.accounts.mint_out.to_account_info(),
                to: ctx.accounts.keeper_token_out.to_account_info(),
                authority: ctx.accounts.config.to_account_info(),
            };
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    keeper_cpi,
                    signer_seeds,
                ),
                keeper_rebate,
                ctx.accounts.mint_out.decimals,
            )?;
        }

        emit!(BatchSettledEvent {
            epoch_id,
            total_intents: intent_count as u32,
            total_output: cumulative_out,
            keeper_rebate,
            settled_at_slot: current_slot,
        });

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT VALIDATION STRUCTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
pub fn initialize_engine_accounts<'info>(
    ctx: &mut Context<InitializeEngine<'info>>,
) -> Result<()> {
    Ok(())
}

#[derive(Accounts)]
pub struct InitializeEngine<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + EngineConfig::INIT_SPACE,
        seeds = [b"config"],
        bump
    )]
    pub config: Account<'info, EngineConfig>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(epoch_id: u64)]
pub struct InitializeBatch<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + std::mem::size_of::<BatchBuffer>(),
        seeds = [b"batch", epoch_id.to_le_bytes().as_ref()],
        bump
    )]
    pub batch_buffer: AccountLoader<'info, BatchBuffer>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SubmitIntent<'info> {
    #[account(
        seeds = [b"config"],
        bump = config.bump
    )]
    pub config: Account<'info, EngineConfig>,

    #[account(mut)]
    pub batch_buffer: AccountLoader<'info, BatchBuffer>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub mint_in: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        token::mint = mint_in,
        token::authority = user
    )]
    pub user_token_in: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = mint_in,
        token::authority = config
    )]
    pub vault_token_in: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(epoch_id: u64)]
pub struct BatchSettle<'info> {
    #[account(
        seeds = [b"config"],
        bump = config.bump
    )]
    pub config: Account<'info, EngineConfig>,

    #[account(
        mut,
        seeds = [b"batch", epoch_id.to_le_bytes().as_ref()],
        bump = batch_buffer.load()?.bump
    )]
    pub batch_buffer: AccountLoader<'info, BatchBuffer>,

    #[account(mut)]
    pub keeper: Signer<'info>,

    pub mint_out: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        token::mint = mint_out,
        token::authority = config
    )]
    pub settlement_source_out: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        token::mint = mint_out,
        token::authority = keeper
    )]
    pub keeper_token_out: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

// -----------------------------------------------------------------------------
// ZERO-COPY DATA STRUCTURES
// -----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct EngineConfig {
    pub admin: Pubkey,
    pub operator: Pubkey,
    pub protocol_fee_bps: u16,
    pub current_epoch: u64,
    pub is_paused: bool,
    pub bump: u8,
}

#[account(zero_copy)]
#[repr(C)]
pub struct BatchBuffer {
    pub epoch_id: u64,
    pub intent_count: u32,
    pub settled: u8,
    pub bump: u8,
    pub _reserved: [u8; 2],
    pub total_input_amount: u64,
    pub intents: [OrderIntent; MAX_INTENTS_PER_BATCH],
}

#[zero_copy]
#[repr(C)]
#[derive(Default)]
pub struct OrderIntent {
    pub user: Pubkey,
    pub amount_in: u64,
    pub min_amount_out: u64,
    pub slot_submitted: u64,
    pub flags: u8, // 0 = Inactive/Cancelled, 1 = Active, 2 = Settled
    pub _reserved: [u8; 7],
}

// -----------------------------------------------------------------------------
// EVENTS & ERRORS
// -----------------------------------------------------------------------------

#[event]
pub struct IntentSubmittedEvent {
    pub epoch_id: u64,
    pub user: Pubkey,
    pub amount_in: u64,
    pub min_amount_out: u64,
    pub slot: u64,
}

#[event]
pub struct BatchSettledEvent {
    pub epoch_id: u64,
    pub total_intents: u32,
    pub total_output: u64,
    pub keeper_rebate: u64,
    pub settled_at_slot: u64,
}

#[error_code]
pub enum HyperDriveError {
    #[msg("Protocol administrative action paused")]
    ProtocolPaused,
    #[msg("Protocol fee basis points exceeds 1000 (10%)")]
    FeeTooHigh,
    #[msg("Transfer or settlement input amount must be greater than zero")]
    InvalidAmount,
    #[msg("Submitted batch epoch does not match current target slot")]
    BatchEpochMismatch,
    #[msg("Ring buffer for the current batch is full")]
    BatchBufferFull,
    #[msg("Batch has already been settled")]
    BatchAlreadySettled,
    #[msg("Batch epoch is still actively accumulating intents")]
    EpochStillActive,
    #[msg("Cannot settle an empty intent batch")]
    EmptyBatch,
    #[msg("Uniform price execution failed user slippage bounds")]
    SlippageExceeded,
    #[msg("Arithmetic overflow in token computation")]
    MathOverflow,
    #[msg("Denominator parameter cannot be zero")]
    DivideByZero,
}