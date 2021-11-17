use borsh::BorshDeserialize;
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

use lido::{state::Lido, token::StLamports};

use crate::logic::{create_account, initialize_reserve_account};
use crate::state::{ExchangeRate, ANKER_LEN};
use crate::{
    error::AnkerError,
    find_instance_address, find_mint_authority, find_reserve_account, find_reserve_authority,
    instruction::{
        AnkerInstruction, DepositAccountsInfo, InitializeAccountsInfo, WithdrawAccountsInfo,
    },
    logic::{burn_b_sol, deserialize_anker, mint_b_sol_to},
    state::Anker,
    token::BLamports,
    ANKER_RESERVE_ACCOUNT, ANKER_RESERVE_AUTHORITY,
};

fn process_initialize(program_id: &Pubkey, accounts_raw: &[AccountInfo]) -> ProgramResult {
    let accounts = InitializeAccountsInfo::try_from_slice(accounts_raw)?;
    let rent = Rent::from_account_info(accounts.sysvar_rent)?;

    let (anker_address, anker_bump_seed) = find_instance_address(program_id, accounts.solido.key);

    if anker_address != *accounts.anker.key {
        msg!(
            "Expected to initialize instance at {}, but {} was provided.",
            anker_address,
            accounts.anker.key,
        );
        return Err(AnkerError::InvalidDerivedAccount.into());
    }

    let solido = Lido::deserialize_lido(accounts.solido_program.key, accounts.solido)?;

    // We generate these addresses here, and then at the end after constructing
    // the Anker instance, we check that these addresses match the provided ones.
    // This way we can re-use the existing checks.
    let (mint_authority, mint_bump_seed) = find_mint_authority(program_id, &anker_address);
    let (_reserve_authority, reserve_authority_bump_seed) =
        find_reserve_authority(program_id, &anker_address);
    let (_reserve_account, reserve_account_bump_seed) =
        find_reserve_account(program_id, &anker_address);

    // Create an account for the Anker instance.
    let anker_seeds = [accounts.solido.key.as_ref(), &[anker_bump_seed]];
    create_account(
        program_id,
        &accounts,
        accounts.anker,
        &rent,
        ANKER_LEN,
        &anker_seeds,
    )?;

    // Create and initialize an stSOL SPL token account for the reserve.
    let reserve_account_seeds = [
        anker_address.as_ref(),
        ANKER_RESERVE_ACCOUNT,
        &[reserve_account_bump_seed],
    ];
    create_account(
        &spl_token::ID,
        &accounts,
        accounts.reserve_account,
        &rent,
        spl_token::state::Account::LEN,
        &reserve_account_seeds,
    )?;
    initialize_reserve_account(&accounts, &reserve_account_seeds)?;

    let anker = Anker {
        b_sol_mint: *accounts.b_sol_mint.key,
        solido_program_id: *accounts.solido_program.key,
        solido: *accounts.solido.key,
        self_bump_seed: anker_bump_seed,
        mint_authority_bump_seed: mint_bump_seed,
        reserve_authority_bump_seed,
        reserve_account_bump_seed,
    };

    anker.check_mint(accounts.b_sol_mint)?;
    anker.check_reserve_address(program_id, &anker_address, accounts.reserve_account)?;
    anker.check_reserve_authority(program_id, &anker_address, accounts.reserve_authority)?;
    anker.check_is_st_sol_account(&solido, accounts.reserve_account)?;

    match spl_token::state::Mint::unpack_from_slice(&accounts.b_sol_mint.data.borrow()) {
        Ok(mint) if mint.mint_authority == COption::Some(mint_authority) => {
            // Ok, we control this mint.
        }
        _ => {
            msg!(
                "Mint authority of bSOL mint {} is not the expected {}.",
                accounts.b_sol_mint.key,
                mint_authority,
            );
            return Err(AnkerError::InvalidTokenMint.into());
        }
    }

    anker.save(accounts.anker)
}

/// Deposit an amount of StLamports and get bSol in return.
fn process_deposit(
    program_id: &Pubkey,
    accounts_raw: &[AccountInfo],
    amount: StLamports,
) -> ProgramResult {
    let accounts = DepositAccountsInfo::try_from_slice(accounts_raw)?;

    if amount == StLamports(0) {
        msg!("Amount must be greater than zero");
        return Err(ProgramError::InvalidArgument);
    }

    let (solido, anker) = deserialize_anker(
        program_id,
        accounts.anker,
        accounts.solido,
        accounts.to_reserve_account,
    )?;

    // Transfer `amount` StLamports to the reserve.
    invoke(
        &spl_token::instruction::transfer(
            &spl_token::id(),
            accounts.from_account.key,
            accounts.to_reserve_account.key,
            accounts.user_authority.key,
            &[],
            amount.0,
        )?,
        &[
            accounts.from_account.clone(),
            accounts.to_reserve_account.clone(),
            accounts.user_authority.clone(),
            accounts.spl_token.clone(),
        ],
    )?;

    // Use Lido's exchange rate (`sol_balance / sol_supply`) to compute the
    // amount of BLamports to mint.
    let exchange_rate = ExchangeRate::from_solido_pegged(&solido);
    let b_sol_amount = exchange_rate.exchange_st_sol(amount)?;

    mint_b_sol_to(program_id, &anker, &accounts, b_sol_amount)?;

    msg!(
        "Anker: Deposited {}, minted {} in return.",
        amount,
        b_sol_amount,
    );

    Ok(())
}

/// Return some bSOL and get back the underlying stSOL.
fn process_withdraw(
    program_id: &Pubkey,
    accounts_raw: &[AccountInfo],
    amount: BLamports,
) -> ProgramResult {
    let accounts = WithdrawAccountsInfo::try_from_slice(accounts_raw)?;

    let (solido, anker) = deserialize_anker(
        program_id,
        accounts.anker,
        accounts.solido,
        accounts.reserve_account,
    )?;

    anker.check_mint(accounts.b_sol_mint)?;
    anker.check_is_st_sol_account(&solido, accounts.reserve_account)?;
    anker.check_reserve_address(program_id, accounts.anker.key, accounts.reserve_account)?;
    anker.check_reserve_authority(program_id, accounts.anker.key, accounts.reserve_authority)?;

    let mint = match spl_token::state::Mint::unpack_from_slice(&accounts.b_sol_mint.data.borrow()) {
        Ok(mint) => mint,
        _ => {
            msg!("Failed to read the bSOL mint.");
            return Err(AnkerError::InvalidTokenMint.into());
        }
    };

    let reserve =
        match spl_token::state::Account::unpack_from_slice(&accounts.reserve_account.data.borrow())
        {
            Ok(reserve) => reserve,
            _ => {
                msg!("Failed to read the reserve stSOL account.");
                return Err(AnkerError::InvalidReserveAccount.into());
            }
        };

    let b_sol_supply = BLamports(mint.supply);
    let reserve_balance = StLamports(reserve.amount);

    // We have two ways of computing the exchange rate:
    //
    // 1. The inverse exchange rate of what Solido uses.
    // 2. Based on the bSOL supply and stSOL reserve.
    //
    // Option 1 enforces a 1 bSOL = 1 SOL peg, but if for some reason the value
    // of stSOL drops (which is impossible at the time of writing because there
    // is no slashing on Solana, but Solana might introduce this in the future
    // when we are in no position to upgrade this program quickly, so we want to
    // be prepared), then there may not be enough stSOL in the reserve to cover
    // all existing bSOL at a 1 bSOL = 1 SOL rate. This is where the Anker
    // exchange rate comes in: we treat 1 bSOL as a share of 1/supply of the
    // reserve. This ensures that all stSOL can be withdrawn, and it socializes
    // the loss among withdrawers until the 1 bSOL = 1 SOL peg is restored.
    let exchange_rate_solido = ExchangeRate::from_solido_pegged(&solido);
    let exchange_rate_anker = ExchangeRate::from_anker_unpegged(b_sol_supply, reserve_balance);
    let st_sol_solido = exchange_rate_solido.exchange_b_sol(amount)?;
    let st_sol_anker = exchange_rate_anker.exchange_b_sol(amount)?;
    let st_sol_amount = std::cmp::min(st_sol_solido, st_sol_anker);

    // Transfer the stSOL back to the user.
    let reserve_seeds = [
        accounts.anker.key.as_ref(),
        ANKER_RESERVE_AUTHORITY,
        &[anker.reserve_authority_bump_seed],
    ];
    invoke_signed(
        &spl_token::instruction::transfer(
            &spl_token::id(),
            accounts.reserve_account.key,
            accounts.to_st_sol_account.key,
            accounts.reserve_authority.key,
            &[],
            st_sol_amount.0,
        )?,
        &[
            accounts.reserve_account.clone(),
            accounts.to_st_sol_account.clone(),
            accounts.reserve_authority.clone(),
            accounts.spl_token.clone(),
        ],
        &[&reserve_seeds[..]],
    )?;

    burn_b_sol(
        &anker,
        accounts.spl_token,
        accounts.b_sol_mint,
        accounts.from_b_sol_account,
        accounts.from_b_sol_authority,
        amount,
    )?;

    msg!("Anker: Withdrew {} for {}.", amount, st_sol_amount,);

    Ok(())
}

/// Processes [Instruction](enum.Instruction.html).
pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], input: &[u8]) -> ProgramResult {
    let instruction = AnkerInstruction::try_from_slice(input)?;
    match instruction {
        AnkerInstruction::Initialize => process_initialize(program_id, accounts),
        AnkerInstruction::Deposit { amount } => process_deposit(program_id, accounts, amount),
        AnkerInstruction::Withdraw { amount } => process_withdraw(program_id, accounts, amount),
        AnkerInstruction::ClaimRewards => todo!(),
    }
}
