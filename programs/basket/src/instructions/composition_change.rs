use anchor_lang::{prelude::*, system_program};
use anchor_spl::token;

use super::rebalance_intent::{load_components_in_order, load_writable_pages_in_order};
use crate::{
    constants::{
        COMPOSITION_CHANGE_APPLY_WINDOW_SECONDS, COMPOSITION_CHANGE_DELAY_SECONDS,
        COMPOSITION_CHANGE_SEED, LARGE_BASKET_COMPONENT_PAGE_SEED,
        MAX_LARGE_BASKET_COMPONENTS_PER_PAGE, VAULT_AUTHORITY_SEED,
    },
    errors::BasketError,
    events::{
        CompositionChangeApplied, CompositionChangeCancelled, CompositionChangeDelayed,
        CompositionChangeProposed,
    },
    state::{
        ComponentAddition, CompositionChange, IndexKind, IndexState, LargeBasketComponent,
        LargeBasketComponentPage,
    },
    utils::{
        associated_token_address_with_token_program,
        create_associated_token_account_idempotent_for_token_program, load_interface_mint,
        new_page_for_additions, total_redeem_fee_bps, validate_composition_change,
        ASSOCIATED_TOKEN_ID,
    },
};

// A fixed-weight basket's composition changes in two steps. The authority proposes new
// target weights (zero removes a component) and/or new components; after
// COMPOSITION_CHANGE_DELAY_SECONDS the authority or keeper applies it, and the next
// rebalance trades the basket onto the new targets. The notice period lets holders who
// disagree redeem first, so applying also needs redemptions open at fees no higher than
// when it was proposed, and a due change lapses if not applied within
// COMPOSITION_CHANGE_APPLY_WINDOW_SECONDS. Applying needs no open intents, so the keeper
// requests a rebalance (holding new intents back), applies, then opens the rebalance.
//
// New components must be classic SPL tokens: Token-2022 extensions (transfer fees and
// hooks, permanent delegates, pausing, scaled UI amounts) break the vault accounting or let
// a third party block or drain the basket.

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ProposeCompositionChangeArgs {
    /// New target weight for each existing component, in global component order.
    pub target_weights_bps: Vec<u16>,
    pub additions: Vec<ComponentAddition>,
}

#[derive(Accounts)]
#[instruction(args: ProposeCompositionChangeArgs)]
pub struct ProposeCompositionChange<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(has_one = authority @ BasketError::UnauthorizedAuthority)]
    pub index: Account<'info, IndexState>,
    #[account(
        init,
        payer = authority,
        space = 8 + CompositionChange::space(args.target_weights_bps.len(), args.additions.len()),
        seeds = [COMPOSITION_CHANGE_SEED, index.key().as_ref()],
        bump
    )]
    pub composition_change: Account<'info, CompositionChange>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelCompositionChange<'info> {
    pub authority: Signer<'info>,
    #[account(has_one = authority @ BasketError::UnauthorizedAuthority)]
    pub index: Account<'info, IndexState>,
    #[account(
        mut,
        has_one = index @ BasketError::InvalidCompositionChange,
        has_one = proposer @ BasketError::InvalidCompositionChange,
        seeds = [COMPOSITION_CHANGE_SEED, index.key().as_ref()],
        bump = composition_change.bump,
        close = proposer
    )]
    pub composition_change: Account<'info, CompositionChange>,
    /// CHECK: Receives the proposal's rent; matched by has_one.
    #[account(mut)]
    pub proposer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ApplyCompositionChange<'info> {
    /// The authority or its rebalance keeper; pays for new vaults and any new page.
    #[account(mut)]
    pub operator: Signer<'info>,
    #[account(mut)]
    pub index: Account<'info, IndexState>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(
        mut,
        has_one = index @ BasketError::InvalidCompositionChange,
        has_one = proposer @ BasketError::InvalidCompositionChange,
        seeds = [COMPOSITION_CHANGE_SEED, index.key().as_ref()],
        bump = composition_change.bump,
        close = proposer
    )]
    pub composition_change: Account<'info, CompositionChange>,
    /// CHECK: Receives the proposal's rent; matched by has_one.
    #[account(mut)]
    pub proposer: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

impl<'info> ProposeCompositionChange<'info> {
    // remaining_accounts: every component page in page-index order, then each addition's mint.
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ProposeCompositionChangeArgs,
    ) -> Result<()> {
        let index = &ctx.accounts.index;
        require!(
            index.kind == IndexKind::FixedWeights && index.large_basket_configured,
            BasketError::InvalidFixedWeightConfig
        );
        let page_count = usize::from(index.large_basket_page_count);
        require!(
            ctx.remaining_accounts.len() == page_count + args.additions.len(),
            BasketError::InvalidRemainingAccounts
        );
        let (page_infos, mint_infos) = ctx.remaining_accounts.split_at(page_count);
        let components = load_components_in_order(&index.key(), ctx.program_id, index, page_infos)?;
        validate_composition_change(
            &components,
            &index.index_mint,
            &args.target_weights_bps,
            &args.additions,
        )?;
        for (addition, mint_info) in args.additions.iter().zip(mint_infos) {
            require_keys_eq!(mint_info.key(), addition.mint, BasketError::InvalidComponentMint);
            require_keys_eq!(*mint_info.owner, token::ID, BasketError::InvalidTokenProgram);
            load_interface_mint(mint_info)?;
        }

        let now = Clock::get()?.unix_timestamp;
        let effective_at = now
            .checked_add(COMPOSITION_CHANGE_DELAY_SECONDS)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        let change = &mut ctx.accounts.composition_change;
        change.index = index.key();
        change.proposer = ctx.accounts.authority.key();
        change.proposed_at = now;
        change.effective_at = effective_at;
        change.bump = ctx.bumps.composition_change;
        change.redeem_fee_bps = total_redeem_fee_bps(index);
        change.target_weights_bps = args.target_weights_bps.clone();
        change.additions = args.additions.clone();

        emit!(CompositionChangeProposed {
            index: index.key(),
            proposer: change.proposer,
            effective_at,
            redeem_fee_bps: change.redeem_fee_bps,
            target_weights_bps: args.target_weights_bps,
            additions: args.additions,
        });
        Ok(())
    }
}

impl<'info> CancelCompositionChange<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        emit!(CompositionChangeCancelled {
            index: ctx.accounts.index.key(),
            authority: ctx.accounts.authority.key(),
        });
        Ok(())
    }
}

impl<'info> ApplyCompositionChange<'info> {
    // remaining_accounts: every component page in page-index order (writable); then the
    // next page's PDA if the additions overflow the last page (writable); then
    // (mint, vault, token program) for each addition, in order.
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let index = &ctx.accounts.index;
        require!(
            index.is_rebalance_operator(&ctx.accounts.operator.key()),
            BasketError::NotRebalanceOperator
        );
        require!(
            index.kind == IndexKind::FixedWeights && index.large_basket_configured,
            BasketError::InvalidFixedWeightConfig
        );
        require!(
            !index.large_basket_operation_in_progress,
            BasketError::InvalidLargeBasketIntent
        );
        // Open intents settle against the current weights and component layout.
        require!(index.open_intent_count == 0, BasketError::IntentsStillOpen);

        let change = &ctx.accounts.composition_change;
        require!(now >= change.effective_at, BasketError::CompositionChangeNotReady);
        let expires_at = change
            .effective_at
            .checked_add(COMPOSITION_CHANGE_APPLY_WINDOW_SECONDS)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        require!(now <= expires_at, BasketError::CompositionChangeExpired);
        require!(
            !index.redeeming_paused && total_redeem_fee_bps(index) <= change.redeem_fee_bps,
            BasketError::CompositionChangeExitRestricted
        );
        let component_count = usize::from(index.large_basket_component_count);
        let page_count = usize::from(index.large_basket_page_count);
        // A component registered since the proposal shifts the weight list out of step.
        require!(
            change.target_weights_bps.len() == component_count,
            BasketError::CompositionChangeStale
        );
        let target_weights = change.target_weights_bps.clone();
        let additions = change.additions.clone();
        let new_page_index = new_page_for_additions(component_count, additions.len())?;
        let new_pages = usize::from(new_page_index.is_some());
        require!(
            ctx.remaining_accounts.len() == page_count + new_pages + 3 * additions.len(),
            BasketError::InvalidRemainingAccounts
        );
        let (page_infos, rest) = ctx.remaining_accounts.split_at(page_count);
        let (new_page_infos, addition_infos) = rest.split_at(new_pages);

        let index_key = index.key();
        let index_mint = index.index_mint;
        let mut pages =
            load_writable_pages_in_order(&index_key, ctx.program_id, component_count, page_infos)?;
        let existing: Vec<LargeBasketComponent> = pages
            .iter()
            .flat_map(|page| page.components.iter().cloned())
            .collect();
        validate_composition_change(&existing, &index_mint, &target_weights, &additions)?;

        let mut global = 0usize;
        for page in pages.iter_mut() {
            for component in page.components.iter_mut() {
                component.target_weight_bps = target_weights[global];
                global += 1;
            }
        }

        let vault_authority = ctx.accounts.vault_authority.key();
        let mut appended = Vec::with_capacity(additions.len());
        for (addition, accounts) in additions.iter().zip(addition_infos.chunks(3)) {
            let (mint_info, vault_info, token_program_info) = (&accounts[0], &accounts[1], &accounts[2]);
            require_keys_eq!(mint_info.key(), addition.mint, BasketError::InvalidComponentMint);
            require_keys_eq!(token_program_info.key(), token::ID, BasketError::InvalidTokenProgram);
            require_keys_eq!(
                *mint_info.owner,
                token_program_info.key(),
                BasketError::InvalidTokenMint
            );
            let expected_vault = associated_token_address_with_token_program(
                &vault_authority,
                &addition.mint,
                token_program_info.key,
            );
            require_keys_eq!(vault_info.key(), expected_vault, BasketError::InvalidVaultAccount);
            create_associated_token_account_idempotent_for_token_program(
                ctx.accounts.associated_token_program.to_account_info(),
                ctx.accounts.operator.to_account_info(),
                vault_info.clone(),
                ctx.accounts.vault_authority.to_account_info(),
                mint_info.clone(),
                ctx.accounts.system_program.to_account_info(),
                token_program_info.clone(),
            )?;
            let mint = load_interface_mint(mint_info)?;
            // Holds nothing until the next rebalance buys it in; mints and redeems move a
            // zero amount of it meanwhile.
            appended.push(LargeBasketComponent {
                mint: addition.mint,
                units_per_index: 0,
                target_weight_bps: addition.target_weight_bps,
                oracle_pair: addition.oracle_pair,
                token_program: token_program_info.key(),
                vault: vault_info.key(),
                accounted_reserve: 0,
                decimals: mint.decimals,
            });
        }

        // Fill the last page first, then a new page with the rest.
        let mut appended = appended.into_iter();
        if let Some(last) = pages.last_mut() {
            while last.components.len() < MAX_LARGE_BASKET_COMPONENTS_PER_PAGE {
                match appended.next() {
                    Some(component) => last.components.push(component),
                    None => break,
                }
            }
            last.component_count = last.components.len() as u16;
        }
        let overflow: Vec<LargeBasketComponent> = appended.collect();
        match new_page_index {
            Some(page_index) => {
                require!(
                    !overflow.is_empty() && usize::from(page_index) == page_count,
                    BasketError::InvalidLargeBasketComponentPage
                );
                create_component_page(
                    &ctx.accounts.operator.to_account_info(),
                    &new_page_infos[0],
                    &ctx.accounts.system_program.to_account_info(),
                    ctx.program_id,
                    index_key,
                    page_index,
                    overflow,
                )?;
            }
            None => require!(overflow.is_empty(), BasketError::InvalidLargeBasketComponentPage),
        }
        for page in pages.iter() {
            page.exit(ctx.program_id)?;
        }

        let index = &mut ctx.accounts.index;
        index.large_basket_component_count = u8::try_from(component_count + additions.len())
            .map_err(|_| error!(BasketError::ArithmeticOverflow))?;
        index.large_basket_page_count = u8::try_from(page_count + new_pages)
            .map_err(|_| error!(BasketError::ArithmeticOverflow))?;
        index.page_generation = index
            .page_generation
            .checked_add(1)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        index.composition_rebalance_due = true;

        emit!(CompositionChangeApplied {
            index: index_key,
            operator: ctx.accounts.operator.key(),
            component_count: index.large_basket_component_count,
        });
        Ok(())
    }
}

/// Restarts a pending composition change's notice. For fixed-weight baskets, update_config
/// calls it when redemptions are paused or unpaused and update_fees when the redeem fee
/// changes, so holders always get
/// COMPOSITION_CHANGE_DELAY_SECONDS of unchanged exit terms before a change applies. The
/// caller passes the ["composition-change", index] PDA as its first remaining account
/// (writable), whether or not a change is pending.
pub fn restart_composition_notice(
    index: &Pubkey,
    program_id: &Pubkey,
    remaining_accounts: &[AccountInfo],
) -> Result<()> {
    let info = remaining_accounts
        .first()
        .ok_or_else(|| error!(BasketError::InvalidRemainingAccounts))?;
    let (expected, _) =
        Pubkey::find_program_address(&[COMPOSITION_CHANGE_SEED, index.as_ref()], program_id);
    require_keys_eq!(info.key(), expected, BasketError::InvalidRemainingAccounts);
    if info.owner != program_id || info.data_is_empty() {
        return Ok(()); // nothing pending
    }
    require!(info.is_writable, BasketError::InvalidRemainingAccounts);
    let mut data = info.try_borrow_mut_data()?;
    let mut change = CompositionChange::try_deserialize(&mut &data[..])?;
    let effective_at = Clock::get()?
        .unix_timestamp
        .checked_add(COMPOSITION_CHANGE_DELAY_SECONDS)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    change.effective_at = change.effective_at.max(effective_at);
    change.try_serialize(&mut &mut data[..])?;
    emit!(CompositionChangeDelayed { index: *index, effective_at: change.effective_at });
    Ok(())
}

fn create_component_page<'info>(
    payer: &AccountInfo<'info>,
    page_info: &AccountInfo<'info>,
    system_program_info: &AccountInfo<'info>,
    program_id: &Pubkey,
    index: Pubkey,
    page_index: u8,
    components: Vec<LargeBasketComponent>,
) -> Result<()> {
    let (expected, bump) = Pubkey::find_program_address(
        &[LARGE_BASKET_COMPONENT_PAGE_SEED, index.as_ref(), &[page_index]],
        program_id,
    );
    require_keys_eq!(page_info.key(), expected, BasketError::InvalidLargeBasketComponentPage);
    require!(
        page_info.is_writable
            && page_info.data_is_empty()
            && *page_info.owner == system_program::ID,
        BasketError::InvalidLargeBasketComponentPage
    );

    let space = 8 + LargeBasketComponentPage::SPACE;
    let rent = Rent::get()?.minimum_balance(space);
    let seeds: &[&[u8]] = &[LARGE_BASKET_COMPONENT_PAGE_SEED, index.as_ref(), &[page_index], &[bump]];
    let current = page_info.lamports();
    if current == 0 {
        system_program::create_account(
            CpiContext::new_with_signer(
                system_program_info.clone(),
                system_program::CreateAccount { from: payer.clone(), to: page_info.clone() },
                &[seeds],
            ),
            rent,
            space as u64,
            program_id,
        )?;
    } else {
        // Anyone can send lamports to the page address first, which CreateAccount refuses.
        if current < rent {
            system_program::transfer(
                CpiContext::new(
                    system_program_info.clone(),
                    system_program::Transfer { from: payer.clone(), to: page_info.clone() },
                ),
                rent - current,
            )?;
        }
        system_program::allocate(
            CpiContext::new_with_signer(
                system_program_info.clone(),
                system_program::Allocate { account_to_allocate: page_info.clone() },
                &[seeds],
            ),
            space as u64,
        )?;
        system_program::assign(
            CpiContext::new_with_signer(
                system_program_info.clone(),
                system_program::Assign { account_to_assign: page_info.clone() },
                &[seeds],
            ),
            program_id,
        )?;
    }

    let start = usize::from(page_index)
        .checked_mul(MAX_LARGE_BASKET_COMPONENTS_PER_PAGE)
        .and_then(|start| u16::try_from(start).ok())
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    let page = LargeBasketComponentPage {
        index,
        page_index,
        start_component_index: start,
        component_count: components.len() as u16,
        bump,
        finalized: true,
        reserved: [0; 32],
        components,
    };
    let mut data = page_info.try_borrow_mut_data()?;
    page.try_serialize(&mut &mut data[..])?;
    Ok(())
}
