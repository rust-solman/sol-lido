// SPDX-FileCopyrightText: 2021 Chorus One AG
// SPDX-License-Identifier: GPL-3.0

#![cfg(feature = "test-bpf")]

use crate::context::{id, Context, StakeDeposit};
use crate::{assert_error_code, assert_solido_error};

use lido::error::LidoError;
use lido::token::Lamports;
use solana_program_test::tokio;
use solana_sdk::signer::Signer;

pub const TEST_DEPOSIT_AMOUNT: Lamports = Lamports(100_000_000_000);
pub const TEST_STAKE_DEPOSIT_AMOUNT: Lamports = Lamports(10_000_000_000);

#[tokio::test]
async fn test_stake_deposit_append() {
    let mut context = Context::new_with_maintainer().await;
    let validator = context.add_validator().await;

    // Sanity check before we start: the validator should have zero balance in zero stake accounts.
    let solido_before = context.get_solido().await;
    let validator_before = &solido_before.validators.entries[0].entry;
    assert_eq!(validator_before.stake_accounts_balance, Lamports(0));
    assert_eq!(validator_before.stake_accounts_seed_begin, 0);
    assert_eq!(validator_before.stake_accounts_seed_end, 0);

    // Now we make a deposit, and then delegate part of it.
    context.deposit(TEST_DEPOSIT_AMOUNT).await;

    let stake_account = context
        .stake_deposit(
            validator.vote_account,
            StakeDeposit::Append,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;

    // The amount that we staked, should now be in the stake account.
    assert_eq!(
        context.get_sol_balance(stake_account).await,
        TEST_STAKE_DEPOSIT_AMOUNT
    );

    // We should also have recorded in the Solido state that this validator now
    // has balance in a stake account.
    let solido_after = context.get_solido().await;

    let validator_after = &solido_after.validators.entries[0].entry;
    assert_eq!(
        validator_after.stake_accounts_balance,
        TEST_STAKE_DEPOSIT_AMOUNT
    );

    // This was also the first deposit, so that should have created one stake account.
    assert_eq!(validator_after.stake_accounts_seed_begin, 0);
    assert_eq!(validator_after.stake_accounts_seed_end, 1);
}

#[tokio::test]
async fn test_stake_deposit_merge() {
    let mut context = Context::new_with_maintainer().await;
    let validator = context.add_validator().await;

    context.deposit(TEST_DEPOSIT_AMOUNT).await;

    // Try to do a stake-merge. This should fail, because there are no stake
    // accounts yet to merge into.
    let result = context
        .try_stake_deposit(
            validator.vote_account,
            StakeDeposit::Merge,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;
    assert_solido_error!(result, LidoError::InvalidStakeAccount);

    // We can stake to a new account though.
    context
        .stake_deposit(
            validator.vote_account,
            StakeDeposit::Append,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;

    // And when that one exists, we can do a stake-merge.
    context
        .stake_deposit(
            validator.vote_account,
            StakeDeposit::Merge,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;

    // We should also have recorded in the Solido state that this validator now
    // has balance in a stake account.
    let solido_after = context.get_solido().await;
    let validator_after = &solido_after.validators.entries[0].entry;
    assert_eq!(
        validator_after.stake_accounts_balance,
        (TEST_STAKE_DEPOSIT_AMOUNT * 2).unwrap(),
    );

    // We merged, so only seed 0 should be consumed at this point.
    assert_eq!(validator_after.stake_accounts_seed_begin, 0);
    assert_eq!(validator_after.stake_accounts_seed_end, 1);

    // Next, we will try to merge stake accounts created in different epochs,
    // which should fail.
    let epoch_schedule = context.context.genesis_config().epoch_schedule;
    let start_slot = epoch_schedule.first_normal_slot;
    context.context.warp_to_slot(start_slot).unwrap();

    let result = context
        .try_stake_deposit(
            validator.vote_account,
            StakeDeposit::Merge,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;
    // The stake program returns error code 6 when it fails to merge stake accounts.
    assert_error_code!(result, 0x06);

    // Confirm that it was really the merge that was problematic, and that we can
    // still create a new stake account this epoch. And after there is a stake
    // account for this epoch, we *can* merge again.
    context
        .stake_deposit(
            validator.vote_account,
            StakeDeposit::Append,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;
    context
        .stake_deposit(
            validator.vote_account,
            StakeDeposit::Merge,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;
}

#[tokio::test]
async fn test_stake_deposit_succeeds_despite_donation() {
    let mut context = Context::new_with_maintainer().await;
    let validator = context.add_validator().await;

    let solido_before = context.get_solido().await;
    let validator_before = &solido_before.validators.entries[0];

    // Figure out what the next stake account is going to be.
    let (stake_account_addr, _) =
        validator_before.find_stake_account_address(&id(), &context.solido.pubkey(), 0);

    // Put some SOL in that account, so it is no longer non-existent.
    context
        .fund(stake_account_addr, Lamports(107_000_000))
        .await;

    // Now we make a deposit and stake it. Despite the stake account already
    // existing (with SOL, but empty data), this should not fail.
    context.deposit(TEST_DEPOSIT_AMOUNT).await;
    context
        .stake_deposit(
            validator.vote_account,
            StakeDeposit::Append,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;

    // The state does not record the additional balance yet though.
    let solido = context.get_solido().await;
    let validator_entry = &solido.validators.entries[0].entry;
    assert_eq!(
        validator_entry.stake_accounts_balance,
        TEST_STAKE_DEPOSIT_AMOUNT
    );

    // After we update the balance, it should.
    context
        .withdraw_inactive_stake(validator.vote_account)
        .await;
    let solido = context.get_solido().await;
    let validator_entry = &solido.validators.entries[0].entry;
    assert_eq!(
        validator_entry.stake_accounts_balance,
        (TEST_STAKE_DEPOSIT_AMOUNT + Lamports(107_000_000)).unwrap()
    );
}

#[tokio::test]
async fn test_stake_deposit_fails_for_inactive_validator() {
    let mut context = Context::new_with_maintainer().await;
    let validator = context.add_validator().await;

    context.deactivate_validator(validator.vote_account).await;

    // Try to stake to the inactive validator. This should fail.
    context.deposit(TEST_DEPOSIT_AMOUNT).await;
    let result = context
        .try_stake_deposit(
            validator.vote_account,
            StakeDeposit::Append,
            TEST_STAKE_DEPOSIT_AMOUNT,
        )
        .await;

    assert_solido_error!(result, LidoError::StakeToInactiveValidator);
}
