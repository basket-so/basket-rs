use anchor_lang::prelude::*;

use super::{large_basket_config::validate_symbol_claim_key, update_config::emit_config_updated};
use crate::{
    constants::{COMMUNITY_CONFIG_SEED, PROTOCOL_CONFIG_SEED, SYMBOL_CLAIM_SEED},
    errors::BasketError,
    events::{CommunityBasketUpdated, CommunityConfigUpdated, IndexSymbolClaimed},
    state::{CommunityConfig, IndexState, ProtocolConfig, SymbolClaim},
    utils::validate_total_index_fee_bps,
};

// The protocol authority's controls over community baskets: the terms new ones get, and the
// two things it keeps for existing ones (where the protocol's fee share goes, and which
// keeper runs rebalances). It also registers the symbols of baskets made before symbols
// were claimed.

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SetCommunityConfigArgs {
    pub treasury: Pubkey,
    pub keeper: Pubkey,
    pub creation_fee_lamports: u64,
    /// One fee step and its split, e.g. 4 + 1 + 5 = 10 bps split 40/10/50.
    pub protocol_step_bps: u16,
    pub creator_step_bps: u16,
    pub staking_step_bps: u16,
}

#[derive(Accounts)]
pub struct SetCommunityConfig<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        has_one = authority @ BasketError::UnauthorizedAuthority,
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + CommunityConfig::SPACE,
        seeds = [COMMUNITY_CONFIG_SEED],
        bump
    )]
    pub community_config: Account<'info, CommunityConfig>,
    pub system_program: Program<'info, System>,
}

/// The parts of a community basket the protocol keeps: where its fee share goes, which keeper
/// runs rebalances, and the supply cap and pauses (an emergency stop for mints or rebalances).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateCommunityBasketArgs {
    pub fee_recipient: Pubkey,
    pub keeper: Pubkey,
    pub max_supply: u64,
    pub minting_paused: bool,
    pub rebalancing_paused: bool,
}

#[derive(Accounts)]
pub struct UpdateCommunityBasket<'info> {
    pub authority: Signer<'info>,
    #[account(
        has_one = authority @ BasketError::UnauthorizedAuthority,
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    #[account(mut)]
    pub index: Account<'info, IndexState>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ClaimIndexSymbolArgs {
    /// The basket's symbol as its claim is seeded: symbol_claim_key(symbol).
    pub symbol_claim_key: String,
}

#[derive(Accounts)]
#[instruction(args: ClaimIndexSymbolArgs)]
pub struct ClaimIndexSymbol<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        has_one = authority @ BasketError::UnauthorizedAuthority,
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    pub index: Account<'info, IndexState>,
    #[account(
        init,
        payer = payer,
        space = 8 + SymbolClaim::SPACE,
        seeds = [SYMBOL_CLAIM_SEED, args.symbol_claim_key.as_bytes()],
        bump
    )]
    pub symbol_claim: Account<'info, SymbolClaim>,
    pub system_program: Program<'info, System>,
}

impl<'info> SetCommunityConfig<'info> {
    pub fn handle(ctx: Context<Self>, args: SetCommunityConfigArgs) -> Result<()> {
        require!(
            args.treasury != Pubkey::default() && args.keeper != Pubkey::default(),
            BasketError::InvalidCommunityConfig
        );
        // A step must fit under the fee cap, so every basket can charge at least one.
        validate_total_index_fee_bps(args.protocol_step_bps, args.creator_step_bps, args.staking_step_bps)?;
        require!(
            args.protocol_step_bps
                .saturating_add(args.creator_step_bps)
                .saturating_add(args.staking_step_bps)
                > 0,
            BasketError::InvalidCommunityConfig
        );

        let config = &mut ctx.accounts.community_config;
        config.treasury = args.treasury;
        config.keeper = args.keeper;
        config.creation_fee_lamports = args.creation_fee_lamports;
        config.protocol_step_bps = args.protocol_step_bps;
        config.creator_step_bps = args.creator_step_bps;
        config.staking_step_bps = args.staking_step_bps;
        config.bump = ctx.bumps.community_config;

        emit!(CommunityConfigUpdated {
            treasury: config.treasury,
            keeper: config.keeper,
            creation_fee_lamports: config.creation_fee_lamports,
            protocol_step_bps: config.protocol_step_bps,
            creator_step_bps: config.creator_step_bps,
            staking_step_bps: config.staking_step_bps,
        });
        Ok(())
    }
}

impl<'info> UpdateCommunityBasket<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateCommunityBasketArgs) -> Result<()> {
        require!(ctx.accounts.index.community, BasketError::CommunityBasketRestricted);
        require!(
            args.fee_recipient != Pubkey::default() && args.keeper != Pubkey::default(),
            BasketError::InvalidCommunityConfig
        );
        let index = &mut ctx.accounts.index;
        index.fee_recipient = args.fee_recipient;
        index.rebalance_keeper = args.keeper;
        index.max_supply = args.max_supply;
        index.minting_paused = args.minting_paused;
        index.rebalancing_paused = args.rebalancing_paused;
        emit_config_updated(index, ctx.accounts.authority.key());
        emit!(CommunityBasketUpdated {
            index: index.key(),
            fee_recipient: index.fee_recipient,
            keeper: index.rebalance_keeper,
        });
        Ok(())
    }
}

impl<'info> ClaimIndexSymbol<'info> {
    pub fn handle(ctx: Context<Self>, args: ClaimIndexSymbolArgs) -> Result<()> {
        validate_symbol_claim_key(&ctx.accounts.index.symbol, &args.symbol_claim_key)?;
        let claim = &mut ctx.accounts.symbol_claim;
        claim.index = ctx.accounts.index.key();
        claim.bump = ctx.bumps.symbol_claim;
        emit!(IndexSymbolClaimed {
            index: claim.index,
            symbol: ctx.accounts.index.symbol.clone(),
        });
        Ok(())
    }
}
