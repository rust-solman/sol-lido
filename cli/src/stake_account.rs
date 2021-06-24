//! Utilities for dealing with stake accounts.

use std::iter::Sum;
use std::ops::Add;

use lido::token::Lamports;
use solana_sdk::clock::Clock;
use solana_sdk::stake_history::StakeHistory;
use spl_stake_pool::stake_program::Delegation;

/// The balance of a stake account, split into the four states that stake can be in.
///
/// The sum of the four fields is equal to the SOL balance of the stake account.
/// Note that a stake account can have a portion in `inactive` and a portion in
/// `active`, with zero being activating or deactivating.
#[derive(Copy, Clone)]
pub struct StakeBalance {
    pub inactive: Lamports,
    pub activating: Lamports,
    pub active: Lamports,
    pub deactivating: Lamports,
}

impl StakeBalance {
    pub fn zero() -> StakeBalance {
        StakeBalance {
            inactive: Lamports(0),
            activating: Lamports(0),
            active: Lamports(0),
            deactivating: Lamports(0),
        }
    }

    /// Extract the stake balance from a delegated stake account.
    pub fn from_delegated_account(
        account_lamports: Lamports,
        delegation: &Delegation,
        clock: &Clock,
        stake_history: &StakeHistory,
    ) -> StakeBalance {
        let target_epoch = clock.epoch;
        let history = Some(stake_history);

        // This toggle is a historical quirk in Solana and should always be set
        // to true. See also https://github.com/ChorusOne/solido/issues/184#issuecomment-861653316.
        let fix_stake_deactivate = true;

        let (active_lamports, activating_lamports, deactivating_lamports) = delegation
            .stake_activating_and_deactivating(target_epoch, history, fix_stake_deactivate);

        let inactive_lamports = account_lamports.0
            .checked_sub(active_lamports)
            .expect("Active stake cannot be larger than stake account balance.")
            .checked_sub(activating_lamports)
            .expect("Activating stake cannot be larger than stake account balance - active.")
            .checked_sub(deactivating_lamports)
            .expect("Deactivating stake cannot be larger than stake account balance - active - activating.");

        StakeBalance {
            inactive: Lamports(inactive_lamports),
            activating: Lamports(activating_lamports),
            active: Lamports(active_lamports),
            deactivating: Lamports(deactivating_lamports),
        }
    }
}

impl Add for StakeBalance {
    type Output = Option<StakeBalance>;

    fn add(self, other: StakeBalance) -> Option<StakeBalance> {
        let result = StakeBalance {
            inactive: (self.inactive + other.inactive)?,
            activating: (self.activating + other.activating)?,
            active: (self.active + other.active)?,
            deactivating: (self.deactivating + other.deactivating)?,
        };
        Some(result)
    }
}

// Ideally we would implement this for Option<StakeBalance>, but it isn't allowed
// due to orphan impl rules. Curiously, it does work in our `impl_token!` macro.
// But in any case, overflow should not happen on mainnet, so we can make it
// panic for now. It will make it harder to fuzz later though.
impl Sum for StakeBalance {
    fn sum<I: Iterator<Item = StakeBalance>>(iter: I) -> Self {
        let mut accumulator = StakeBalance::zero();
        for x in iter {
            accumulator = (accumulator + x).expect(
                "Overflow when adding stake balances, this should not happen \
                because there is not that much SOL in the ecosystem.",
            )
        }
        accumulator
    }
}
