//! A stand-in for Jupiter on a local validator. It swaps at whatever amounts the instruction
//! names, against token accounts its reserve PDA owns, so a test can quote and fill swaps the
//! way Jupiter's API and program would without cloning real pools.
//!
//! Data: [0u8, in_amount u64 LE, out_amount u64 LE].
//! Accounts: [user authority (signer), user source (w), user destination (w),
//! reserve source (w), reserve destination (w), reserve authority, input mint, output mint,
//! input token program, output token program].
use solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
};

pub const RESERVE_SEED: &[u8] = b"reserve";

entrypoint!(process);

fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [authority, user_source, user_destination, reserve_source, reserve_destination, reserve, input_mint, output_mint, input_token_program, output_token_program, ..] =
        accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if data.len() != 17 || data[0] != 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let in_amount = u64::from_le_bytes(data[1..9].try_into().unwrap());
    let out_amount = u64::from_le_bytes(data[9..17].try_into().unwrap());
    let (reserve_key, bump) = Pubkey::find_program_address(&[RESERVE_SEED], program_id);
    if *reserve.key != reserve_key {
        return Err(ProgramError::InvalidSeeds);
    }
    invoke(
        &transfer_checked(input_token_program.key, user_source.key, input_mint, reserve_source.key, authority.key, in_amount)?,
        &[user_source.clone(), input_mint.clone(), reserve_source.clone(), authority.clone(), input_token_program.clone()],
    )?;
    invoke_signed(
        &transfer_checked(output_token_program.key, reserve_destination.key, output_mint, user_destination.key, reserve.key, out_amount)?,
        &[reserve_destination.clone(), output_mint.clone(), user_destination.clone(), reserve.clone(), output_token_program.clone()],
        &[&[RESERVE_SEED, &[bump]]],
    )
}

// SPL Token's TransferChecked (12), which both token programs share.
fn transfer_checked(
    token_program: &Pubkey,
    source: &Pubkey,
    mint: &AccountInfo,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
) -> Result<Instruction, ProgramError> {
    let decimals = *mint.try_borrow_data()?.get(44).ok_or(ProgramError::InvalidAccountData)?;
    let mut data = vec![12];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Ok(Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    })
}
