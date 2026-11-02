use anchor_lang::{
    prelude::*,
    solana_program::{
        instruction::Instruction,
        program::invoke_signed,
        sysvar::instructions as ix_sysvar,
    },
};
use anchor_spl::{
    associated_token::AssociatedToken,
    token_2022::{self, Token2022},
    token_interface::{Mint, TokenAccount},
};

declare_id!("StrMP26V6K4vXp1uGvG4uQx8N5XyN9zD7fK5bWcR2a1");

pub const EXTRA_ACCOUNT_METAS_SEED: &[u8] = b"extra-account-metas";
pub const CONFIG_SEED: &[u8] = b"config";
pub const CHANNEL_SEED: &[u8] = b"channel";
pub const VAULT_SEED: &[u8] = b"vault";

#[program]
pub mod streampoint_core {
    use super::*;

    /// Initializes global protocol parameters.
    pub fn initialize_protocol(
        ctx: Context<InitializeProtocol>,
        fee_basis_points: u16,
        operator: Pubkey,
    ) -> Result<()> {
        require!(fee_basis_points <= 1000, StreamPointError::FeeExceedsLimit); // Max 10%
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.operator = operator;
        config.fee_basis_points = fee_basis_points;
        config.total_channels = 0;
        config.bump = ctx.bumps.config;
        emit!(ProtocolInitializedEvent {
            admin: config.admin,
            operator,
            fee_basis_points,
        });
        Ok(())
    }

    /// Initializes a streaming content channel with rate limiting and micro-vesting.
    pub fn create_channel(
        ctx: Context<CreateChannel>,
        channel_id: [u8; 32],
        rate_per_second: u64,
        max_capacity: u64,
    ) -> Result<()> {
        require!(rate_per_second > 0, StreamPointError::InvalidStreamRate);
        require!(max_capacity > 0, StreamPointError::InvalidCapacity);

        let channel = &mut ctx.accounts.channel;
        channel.owner = ctx.accounts.owner.key();
        channel.mint = ctx.accounts.mint.key();
        channel.vault = ctx.accounts.vault.key();
        channel.channel_id = channel_id;
        channel.rate_per_second = rate_per_second;
        channel.deposited_amount = 0;
        channel.claimed_amount = 0;
        channel.last_update_ts = Clock::get()?.unix_timestamp;
        channel.is_active = true;
        channel.bump = ctx.bumps.channel;
        channel.vault_bump = ctx.bumps.vault;

        let config = &mut ctx.accounts.config;
        config.total_channels = config.total_channels.checked_add(1).ok_or(StreamPointError::MathOverflow)?;

        emit!(ChannelCreatedEvent {
            channel_id,
            owner: channel.owner,
            rate_per_second,
        });
        Ok(())
    }

    /// Deposits funds into a streaming channel.
    pub fn fund_channel(ctx: Context<FundChannel>, amount: u64) -> Result<()> {
        require!(amount > 0, StreamPointError::ZeroDeposit);
        let channel = &mut ctx.accounts.channel;
        require!(channel.is_active, StreamPointError::ChannelInactive);

        // SPL Token-2022 transfer CPI
        token_2022::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token_2022::TransferChecked {
                    from: ctx.accounts.funder_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.funder.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        channel.deposited_amount = channel.deposited_amount.checked_add(amount).ok_or(StreamPointError::MathOverflow)?;
        
        emit!(ChannelFundedEvent {
            channel_id: channel.channel_id,
            funder: ctx.accounts.funder.key(),
            amount,
        });
        Ok(())
    }

    /// Settles the active content stream, calculating protocol fees and payout.
    pub fn settle_stream(ctx: Context<SettleStream>) -> Result<()> {
        let channel = &mut ctx.accounts.channel;
        let config = &ctx.accounts.config;
        let current_ts = Clock::get()?.unix_timestamp;

        require!(channel.is_active, StreamPointError::ChannelInactive);
        require!(current_ts > channel.last_update_ts, StreamPointError::InvalidTimestamp);

        let elapsed = (current_ts - channel.last_update_ts) as u64;
        let accrued_amount = elapsed.checked_mul(channel.rate_per_second).ok_or(StreamPointError::MathOverflow)?;

        let unreserved = channel.deposited_amount.checked_sub(channel.claimed_amount).ok_or(StreamPointError::InsufficientEscrow)?;
        let payout = accrued_amount.min(unreserved);

        if payout == 0 {
            return Ok(());
        }

        let protocol_fee = (payout as u128)
            .checked_mul(config.fee_basis_points as u128)
            .ok_or(StreamPointError::MathOverflow)?
            .checked_div(10000)
            .ok_or(StreamPointError::MathOverflow)? as u64;

        let creator_amount = payout.checked_sub(protocol_fee).ok_or(StreamPointError::MathOverflow)?;

        let channel_id_ref = channel.channel_id;
        let owner_ref = channel.owner;
        let vault_bump = channel.vault_bump;

        let seeds = &[
            VAULT_SEED,
            channel_id_ref.as_ref(),
            &[vault_bump],
        ];
        let signer = &[&seeds[..]];

        // Transfer Creator Amount
        token_2022::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                token_2022::TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.creator_token_account.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer,
            ),
            creator_amount,
            ctx.accounts.mint.decimals,
        )?;

        // Transfer Protocol Fee
        if protocol_fee > 0 {
            token_2022::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    token_2022::TransferChecked {
                        from: ctx.accounts.vault.to_account_info(),
                        mint: ctx.accounts.mint.to_account_info(),
                        to: ctx.accounts.fee_treasury_token_account.to_account_info(),
                        authority: ctx.accounts.vault.to_account_info(),
                    },
                    signer,
                ),
                protocol_fee,
                ctx.accounts.mint.decimals,
            )?;
        }

        channel.claimed_amount = channel.claimed_amount.checked_add(payout).ok_or(StreamPointError::MathOverflow)?;
        channel.last_update_ts = current_ts;

        if channel.claimed_amount >= channel.deposited_amount {
            channel.is_active = false;
        }

        emit!(StreamSettledEvent {
            channel_id: channel.channel_id,
            creator_received: creator_amount,
            protocol_fee,
            timestamp: current_ts,
        });

        Ok(())
    }

    /// Token-2022 Transfer Hook verification entrypoint.
    pub fn transfer_hook(ctx: Context<TransferHookContext>, amount: u64) -> Result<()> {
        let channel = &ctx.accounts.channel;
        require!(channel.is_active, StreamPointError::ChannelInactive);
        require!(amount > 0, StreamPointError::ZeroTransferHookAmount);

        // Dynamic integrity check: Enforce that recipient matches configured vault
        require_keys_eq!(
            ctx.accounts.destination.key(),
            channel.vault,
            StreamPointError::InvalidHookDestination
        );

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT STRUCTS & CONTEXTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeProtocol<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + ProtocolConfig::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, ProtocolConfig>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(channel_id: [u8; 32])]
pub struct CreateChannel<'info> {
    #[account(
        init,
        payer = owner,
        space = 8 + ContentStreamChannel::INIT_SPACE,
        seeds = [CHANNEL_SEED, channel_id.as_ref(), owner.key().as_ref()],
        bump
    )]
    pub channel: Account<'info, ContentStreamChannel>,
    #[account(
        init,
        payer = owner,
        seeds = [VAULT_SEED, channel_id.as_ref()],
        bump,
        token::mint = mint,
        token::authority = vault,
        token::token_program = token_program
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,
    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct FundChannel<'info> {
    #[account(
        mut,
        seeds = [CHANNEL_SEED, channel.channel_id.as_ref(), channel.owner.as_ref()],
        bump = channel.bump,
        has_one = vault,
        has_one = mint
    )]
    pub channel: Account<'info, ContentStreamChannel>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        seeds = [VAULT_SEED, channel.channel_id.as_ref()],
        bump = channel.vault_bump
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = funder,
        token::token_program = token_program
    )]
    pub funder_token_account: InterfaceAccount<'info, TokenAccount>,
    pub funder: Signer<'info>,
    pub token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct SettleStream<'info> {
    #[account(
        mut,
        seeds = [CHANNEL_SEED, channel.channel_id.as_ref(), channel.owner.as_ref()],
        bump = channel.bump,
        has_one = vault,
        has_one = mint
    )]
    pub channel: Account<'info, ContentStreamChannel>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        seeds = [VAULT_SEED, channel.channel_id.as_ref()],
        bump = channel.vault_bump
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = channel.owner)]
    pub creator: SystemAccount<'info>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = creator,
        token::token_program = token_program
    )]
    pub creator_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        token::mint = mint,
        token::token_program = token_program
    )]
    pub fee_treasury_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,
    pub keeper: Signer<'info>,
    pub token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct TransferHookContext<'info> {
    #[account(
        seeds = [CHANNEL_SEED, channel.channel_id.as_ref(), channel.owner.as_ref()],
        bump = channel.bump
    )]
    pub channel: Account<'info, ContentStreamChannel>,
    pub source: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub destination: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: Transfer Hook authority
    pub authority: AccountInfo<'info>,
    /// CHECK: ExtraAccountMetaList PDA
    #[account(
        seeds = [EXTRA_ACCOUNT_METAS_SEED, mint.key().as_ref()],
        bump
    )]
    pub extra_account_metas: AccountInfo<'info>,
}

// -----------------------------------------------------------------------------
// DATA STATE
// -----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct ProtocolConfig {
    pub admin: Pubkey,
    pub operator: Pubkey,
    pub fee_basis_points: u16,
    pub total_channels: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct ContentStreamChannel {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub channel_id: [u8; 32],
    pub rate_per_second: u64,
    pub deposited_amount: u64,
    pub claimed_amount: u64,
    pub last_update_ts: i64,
    pub is_active: bool,
    pub bump: u8,
    pub vault_bump: u8,
}

// -----------------------------------------------------------------------------
// EVENTS & ERRORS
// -----------------------------------------------------------------------------

#[event]
pub struct ProtocolInitializedEvent {
    pub admin: Pubkey,
    pub operator: Pubkey,
    pub fee_basis_points: u16,
}

#[event]
pub struct ChannelCreatedEvent {
    pub channel_id: [u8; 32],
    pub owner: Pubkey,
    pub rate_per_second: u64,
}

#[event]
pub struct ChannelFundedEvent {
    pub channel_id: [u8; 32],
    pub funder: Pubkey,
    pub amount: u64,
}

#[event]
pub struct StreamSettledEvent {
    pub channel_id: [u8; 32],
    pub creator_received: u64,
    pub protocol_fee: u64,
    pub timestamp: i64,
}

#[error_code]
pub enum StreamPointError {
    #[msg("Fee basis points exceed maximum threshold of 1000 (10%)")]
    FeeExceedsLimit,
    #[msg("Streaming rate per second must be greater than zero")]
    InvalidStreamRate,
    #[msg("Channel max capacity must be non-zero")]
    InvalidCapacity,
    #[msg("Arithmetic calculation resulted in an overflow or underflow")]
    MathOverflow,
    #[msg("Stream channel is currently suspended or exhausted")]
    ChannelInactive,
    #[msg("Cannot deposit zero tokens")]
    ZeroDeposit,
    #[msg("Calculated settlement timestamp is older than last recorded tick")]
    InvalidTimestamp,
    #[msg("Insufficient escrow balance to fulfill settlement rate")]
    InsufficientEscrow,
    #[msg("Transfer hook amount must be positive")]
    ZeroTransferHookAmount,
    #[msg("Destination account fails transfer hook routing policy")]
    InvalidHookDestination,
}