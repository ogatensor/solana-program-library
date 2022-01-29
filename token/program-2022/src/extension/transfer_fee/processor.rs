use {
    crate::{
        check_program_account,
        error::TokenError,
        extension::{
            transfer_fee::{
                instruction::TransferFeeInstruction, TransferFee, TransferFeeAmount,
                TransferFeeConfig, MAX_FEE_BASIS_POINTS,
            },
            StateWithExtensionsMut,
        },
        processor::Processor,
        state::{Account, Mint},
    },
    solana_program::{
        account_info::{next_account_info, AccountInfo},
        clock::Clock,
        entrypoint::ProgramResult,
        msg,
        program_option::COption,
        pubkey::Pubkey,
        sysvar::Sysvar,
    },
    std::convert::TryInto,
};

fn process_initialize_transfer_fee_config(
    accounts: &[AccountInfo],
    transfer_fee_config_authority: COption<Pubkey>,
    withdraw_withheld_authority: COption<Pubkey>,
    transfer_fee_basis_points: u16,
    maximum_fee: u64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let mint_account_info = next_account_info(account_info_iter)?;

    let mut mint_data = mint_account_info.data.borrow_mut();
    let mut mint = StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut mint_data)?;
    let extension = mint.init_extension::<TransferFeeConfig>()?;
    extension.transfer_fee_config_authority = transfer_fee_config_authority.try_into()?;
    extension.withdraw_withheld_authority = withdraw_withheld_authority.try_into()?;
    extension.withheld_amount = 0u64.into();

    if transfer_fee_basis_points > MAX_FEE_BASIS_POINTS {
        return Err(TokenError::TransferFeeExceedsMaximum.into());
    }
    // To be safe, set newer and older transfer fees to the same thing on init,
    // but only newer will actually be used
    let epoch = Clock::get()?.epoch;
    let transfer_fee = TransferFee {
        epoch: epoch.into(),
        transfer_fee_basis_points: transfer_fee_basis_points.into(),
        maximum_fee: maximum_fee.into(),
    };
    extension.older_transfer_fee = transfer_fee;
    extension.newer_transfer_fee = transfer_fee;

    Ok(())
}

fn process_set_transfer_fee(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    transfer_fee_basis_points: u16,
    maximum_fee: u64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let mint_account_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;
    let authority_info_data_len = authority_info.data_len();

    let mut mint_data = mint_account_info.data.borrow_mut();
    let mut mint = StateWithExtensionsMut::<Mint>::unpack(&mut mint_data)?;
    let extension = mint.get_extension_mut::<TransferFeeConfig>()?;

    let transfer_fee_config_authority =
        Option::<Pubkey>::from(extension.transfer_fee_config_authority)
            .ok_or(TokenError::NoAuthorityExists)?;
    Processor::validate_owner(
        program_id,
        &transfer_fee_config_authority,
        authority_info,
        authority_info_data_len,
        account_info_iter.as_slice(),
    )?;

    if transfer_fee_basis_points > MAX_FEE_BASIS_POINTS {
        return Err(TokenError::TransferFeeExceedsMaximum.into());
    }

    // When setting the transfer fee, we have two situations:
    // * newer transfer fee epoch <= current epoch:
    //     newer transfer fee is the active one, so overwrite older transfer fee with newer, then overwrite newer transfer fee
    // * newer transfer fee epoch == next epoch:
    //     it was never used, so just overwrite next transfer fee
    let epoch = Clock::get()?.epoch;
    let next_epoch = epoch.saturating_add(1);
    if u64::from(extension.newer_transfer_fee.epoch) <= epoch {
        extension.older_transfer_fee = extension.newer_transfer_fee;
    }
    let transfer_fee = TransferFee {
        epoch: next_epoch.into(),
        transfer_fee_basis_points: transfer_fee_basis_points.into(),
        maximum_fee: maximum_fee.into(),
    };
    extension.newer_transfer_fee = transfer_fee;

    Ok(())
}

fn harvest_from_account<'a, 'b>(
    mint_key: &'b Pubkey,
    mint_extension: &'b mut TransferFeeConfig,
    token_account_info: &'b AccountInfo<'a>,
) -> Result<(), TokenError> {
    let mut token_account_data = token_account_info.data.borrow_mut();
    let mut token_account = StateWithExtensionsMut::<Account>::unpack(&mut token_account_data)
        .map_err(|_| TokenError::InvalidState)?;
    if token_account.base.mint != *mint_key {
        return Err(TokenError::MintMismatch);
    }
    check_program_account(token_account_info.owner).map_err(|_| TokenError::InvalidState)?;
    let token_account_extension = token_account
        .get_extension_mut::<TransferFeeAmount>()
        .map_err(|_| TokenError::InvalidState)?;
    let account_withheld_amount = u64::from(token_account_extension.withheld_amount);
    let mint_withheld_amount = u64::from(mint_extension.withheld_amount);
    mint_extension.withheld_amount = mint_withheld_amount
        .checked_add(account_withheld_amount)
        .ok_or(TokenError::Overflow)?
        .into();
    token_account_extension.withheld_amount = 0.into();
    Ok(())
}

fn process_harvest_withheld_tokens_to_mint(accounts: &[AccountInfo]) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let mint_account_info = next_account_info(account_info_iter)?;
    let token_account_infos = account_info_iter.as_slice();

    let mut mint_data = mint_account_info.data.borrow_mut();
    let mut mint = StateWithExtensionsMut::<Mint>::unpack(&mut mint_data)?;
    let mint_extension = mint.get_extension_mut::<TransferFeeConfig>()?;

    for token_account_info in token_account_infos {
        match harvest_from_account(mint_account_info.key, mint_extension, token_account_info) {
            // Shouldn't ever happen, but if it does, we don't want to propagate any half-done changes
            Err(TokenError::Overflow) => return Err(TokenError::Overflow.into()),
            Err(e) => {
                msg!("Error harvesting from {}: {}", token_account_info.key, e);
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

pub(crate) fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction: TransferFeeInstruction,
) -> ProgramResult {
    check_program_account(program_id)?;

    match instruction {
        TransferFeeInstruction::InitializeTransferFeeConfig {
            transfer_fee_config_authority,
            withdraw_withheld_authority,
            transfer_fee_basis_points,
            maximum_fee,
        } => process_initialize_transfer_fee_config(
            accounts,
            transfer_fee_config_authority,
            withdraw_withheld_authority,
            transfer_fee_basis_points,
            maximum_fee,
        ),
        TransferFeeInstruction::TransferCheckedWithFee {
            amount,
            decimals,
            fee,
        } => {
            msg!("TransferFeeInstruction: TransferCheckedWithFee");
            Processor::process_transfer(program_id, accounts, amount, Some(decimals), Some(fee))
        }
        TransferFeeInstruction::WithdrawWithheldTokensFromMint => {
            unimplemented!();
        }
        TransferFeeInstruction::WithdrawWithheldTokensFromAccounts => {
            unimplemented!();
        }
        TransferFeeInstruction::HarvestWithheldTokensToMint => {
            msg!("TransferFeeInstruction: HarvestWithheldTokensToMint");
            process_harvest_withheld_tokens_to_mint(accounts)
        }
        TransferFeeInstruction::SetTransferFee {
            transfer_fee_basis_points,
            maximum_fee,
        } => {
            msg!("TransferFeeInstruction: SetTransferFee");
            process_set_transfer_fee(program_id, accounts, transfer_fee_basis_points, maximum_fee)
        }
    }
}