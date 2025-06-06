// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for the module.

use super::{ConfigOp, Event, *};
use crate::{asset, ledger::StakingLedgerInspect};
use frame_election_provider_support::{
	bounds::{DataProviderBounds, ElectionBoundsBuilder},
	ElectionProvider, SortedListProvider, Support,
};
use frame_support::{
	assert_noop, assert_ok, assert_storage_noop,
	dispatch::{extract_actual_weight, GetDispatchInfo, WithPostDispatchInfo},
	hypothetically,
	pallet_prelude::*,
	traits::{
		fungible::Inspect, Currency, Get, InspectLockableCurrency, LockableCurrency,
		ReservableCurrency, WithdrawReasons,
	},
};
use mock::*;
use pallet_balances::Error as BalancesError;
use pallet_session::{disabling::UpToLimitWithReEnablingDisablingStrategy, Event as SessionEvent};
use sp_runtime::{
	assert_eq_error_rate, bounded_vec,
	traits::{BadOrigin, Dispatchable},
	Perbill, Percent, Perquintill, Rounding, TokenError,
};
use sp_staking::{
	offence::{OffenceDetails, OnOffenceHandler},
	SessionIndex,
};
use substrate_test_utils::assert_eq_uvec;

#[test]
fn set_staking_configs_works() {
	ExtBuilder::default().build_and_execute(|| {
		// setting works
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Set(1_500),
			ConfigOp::Set(2_000),
			ConfigOp::Set(10),
			ConfigOp::Set(20),
			ConfigOp::Set(Percent::from_percent(75)),
			ConfigOp::Set(Zero::zero()),
			ConfigOp::Set(Zero::zero())
		));
		assert_eq!(MinNominatorBond::<Test>::get(), 1_500);
		assert_eq!(MinValidatorBond::<Test>::get(), 2_000);
		assert_eq!(MaxNominatorsCount::<Test>::get(), Some(10));
		assert_eq!(MaxValidatorsCount::<Test>::get(), Some(20));
		assert_eq!(ChillThreshold::<Test>::get(), Some(Percent::from_percent(75)));
		assert_eq!(MinCommission::<Test>::get(), Perbill::from_percent(0));
		assert_eq!(MaxStakedRewards::<Test>::get(), Some(Percent::from_percent(0)));

		// noop does nothing
		assert_storage_noop!(assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop
		)));

		// removing works
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove
		));
		assert_eq!(MinNominatorBond::<Test>::get(), 0);
		assert_eq!(MinValidatorBond::<Test>::get(), 0);
		assert_eq!(MaxNominatorsCount::<Test>::get(), None);
		assert_eq!(MaxValidatorsCount::<Test>::get(), None);
		assert_eq!(ChillThreshold::<Test>::get(), None);
		assert_eq!(MinCommission::<Test>::get(), Perbill::from_percent(0));
		assert_eq!(MaxStakedRewards::<Test>::get(), None);
	});
}

#[test]
fn force_unstake_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Account 11 (also controller) is stashed and locked
		assert_eq!(Staking::bonded(&11), Some(11));
		// Adds 2 slashing spans
		add_slash(&11);
		// Cant transfer
		assert_noop!(
			Balances::transfer_allow_death(RuntimeOrigin::signed(11), 1, 10),
			TokenError::FundsUnavailable,
		);
		// Force unstake requires root.
		assert_noop!(Staking::force_unstake(RuntimeOrigin::signed(11), 11, 2), BadOrigin);
		// Force unstake needs correct number of slashing spans (for weight calculation)
		assert_noop!(
			Staking::force_unstake(RuntimeOrigin::root(), 11, 0),
			Error::<Test>::IncorrectSlashingSpans
		);
		// We now force them to unstake
		assert_ok!(Staking::force_unstake(RuntimeOrigin::root(), 11, 2));
		// No longer bonded.
		assert_eq!(Staking::bonded(&11), None);
		// Transfer works.
		assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(11), 1, 10));
	});
}

#[test]
fn kill_stash_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Account 11 (also controller) is stashed and locked
		assert_eq!(Staking::bonded(&11), Some(11));
		// Adds 2 slashing spans
		add_slash(&11);
		// Only can kill a stash account
		assert_noop!(Staking::kill_stash(&12, 0), Error::<Test>::NotStash);
		// Respects slashing span count
		assert_noop!(Staking::kill_stash(&11, 0), Error::<Test>::IncorrectSlashingSpans);
		// Correct inputs, everything works
		assert_ok!(Staking::kill_stash(&11, 2));
		// No longer bonded.
		assert_eq!(Staking::bonded(&11), None);
	});
}

#[test]
fn basic_setup_works() {
	// Verifies initial conditions of mock
	ExtBuilder::default().build_and_execute(|| {
		// Account 11 is stashed and locked, and is the controller
		assert_eq!(Staking::bonded(&11), Some(11));
		// Account 21 is stashed and locked and is the controller
		assert_eq!(Staking::bonded(&21), Some(21));
		// Account 1 is not a stashed
		assert_eq!(Staking::bonded(&1), None);

		// Account 11 controls its own stash, which is 100 * balance_factor units
		assert_eq!(
			Ledger::get(&11).unwrap(),
			StakingLedgerInspect::<Test> {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		// Account 21 controls its own stash, which is 200 * balance_factor units
		assert_eq!(
			Ledger::get(&21).unwrap(),
			StakingLedgerInspect::<Test> {
				stash: 21,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		// Account 1 does not control any stash
		assert!(Staking::ledger(1.into()).is_err());

		// ValidatorPrefs are default
		assert_eq_uvec!(
			<Validators<Test>>::iter().collect::<Vec<_>>(),
			vec![
				(31, ValidatorPrefs::default()),
				(21, ValidatorPrefs::default()),
				(11, ValidatorPrefs::default())
			]
		);

		assert_eq!(
			Staking::ledger(101.into()).unwrap(),
			StakingLedgerInspect {
				stash: 101,
				total: 500,
				active: 500,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

		assert_eq!(
			Staking::eras_stakers(active_era(), &11),
			Exposure {
				total: 1125,
				own: 1000,
				others: vec![IndividualExposure { who: 101, value: 125 }]
			},
		);
		assert_eq!(
			Staking::eras_stakers(active_era(), &21),
			Exposure {
				total: 1375,
				own: 1000,
				others: vec![IndividualExposure { who: 101, value: 375 }]
			},
		);

		// initial total stake = 1125 + 1375
		assert_eq!(ErasTotalStake::<Test>::get(active_era()), 2500);

		// The number of validators required.
		assert_eq!(ValidatorCount::<Test>::get(), 2);

		// Initial Era and session
		assert_eq!(active_era(), 0);

		// Account 10 has `balance_factor` free balance
		assert_eq!(Balances::balance(&10), 1);

		// New era is not being forced
		assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
	});
}

#[test]
fn change_controller_works() {
	ExtBuilder::default().build_and_execute(|| {
		let (stash, controller) = testing_utils::create_unique_stash_controller::<Test>(
			0,
			100,
			RewardDestination::Staked,
			false,
		)
		.unwrap();

		// ensure `stash` and `controller` are bonded as stash controller pair.
		assert_eq!(Staking::bonded(&stash), Some(controller));

		// `controller` can control `stash` who is initially a validator.
		assert_ok!(Staking::chill(RuntimeOrigin::signed(controller)));

		// sets controller back to `stash`.
		assert_ok!(Staking::set_controller(RuntimeOrigin::signed(stash)));
		assert_eq!(Staking::bonded(&stash), Some(stash));
		mock::start_active_era(1);

		// fetch the ledger from storage and check if the controller is correct.
		let ledger = Staking::ledger(StakingAccount::Stash(stash)).unwrap();
		assert_eq!(ledger.controller(), Some(stash));

		// same if we fetch the ledger by controller.
		let ledger = Staking::ledger(StakingAccount::Controller(stash)).unwrap();
		assert_eq!(ledger.controller, Some(stash));
		assert_eq!(ledger.controller(), Some(stash));

		// the raw storage ledger's controller is always `None`. however, we can still fetch the
		// correct controller with `ledger.controller()`.
		let raw_ledger = <Ledger<Test>>::get(&stash).unwrap();
		assert_eq!(raw_ledger.controller, None);

		// `controller` is no longer in control. `stash` is now controller.
		assert_noop!(
			Staking::validate(RuntimeOrigin::signed(controller), ValidatorPrefs::default()),
			Error::<Test>::NotController,
		);
		assert_ok!(Staking::validate(RuntimeOrigin::signed(stash), ValidatorPrefs::default()));
	})
}

#[test]
fn change_controller_already_paired_once_stash() {
	ExtBuilder::default().build_and_execute(|| {
		// 11 and 11 are bonded as controller and stash respectively.
		assert_eq!(Staking::bonded(&11), Some(11));

		// 11 is initially a validator.
		assert_ok!(Staking::chill(RuntimeOrigin::signed(11)));

		// Controller cannot change once matching with stash.
		assert_noop!(
			Staking::set_controller(RuntimeOrigin::signed(11)),
			Error::<Test>::AlreadyPaired
		);
		assert_eq!(Staking::bonded(&11), Some(11));
		mock::start_active_era(1);

		// 10 is no longer in control.
		assert_noop!(
			Staking::validate(RuntimeOrigin::signed(10), ValidatorPrefs::default()),
			Error::<Test>::NotController,
		);
		assert_ok!(Staking::validate(RuntimeOrigin::signed(11), ValidatorPrefs::default()));
	})
}

#[test]
fn rewards_should_work() {
	ExtBuilder::default().nominate(true).session_per_era(3).build_and_execute(|| {
		let init_balance_11 = asset::total_balance::<Test>(&11);
		let init_balance_21 = asset::total_balance::<Test>(&21);
		let init_balance_101 = asset::total_balance::<Test>(&101);

		// Set payees
		Payee::<Test>::insert(11, RewardDestination::Account(11));
		Payee::<Test>::insert(21, RewardDestination::Account(21));
		Payee::<Test>::insert(101, RewardDestination::Account(101));

		Pallet::<Test>::reward_by_ids(vec![(11, 50)]);
		Pallet::<Test>::reward_by_ids(vec![(11, 50)]);
		// This is the second validator of the current elected set.
		Pallet::<Test>::reward_by_ids(vec![(21, 50)]);

		// Compute total payout now for whole duration of the session.
		let total_payout_0 = current_total_payout_for_duration(reward_time_per_era());
		let maximum_payout = maximum_payout_for_duration(reward_time_per_era());

		start_session(1);
		assert_eq_uvec!(Session::validators(), vec![11, 21]);

		assert_eq!(asset::total_balance::<Test>(&11), init_balance_11);
		assert_eq!(asset::total_balance::<Test>(&21), init_balance_21);
		assert_eq!(asset::total_balance::<Test>(&101), init_balance_101);
		assert_eq!(
			ErasRewardPoints::<Test>::get(active_era()),
			EraRewardPoints {
				total: 50 * 3,
				individual: vec![(11, 100), (21, 50)].into_iter().collect(),
			}
		);
		let part_for_11 = Perbill::from_rational::<u32>(1000, 1125);
		let part_for_21 = Perbill::from_rational::<u32>(1000, 1375);
		let part_for_101_from_11 = Perbill::from_rational::<u32>(125, 1125);
		let part_for_101_from_21 = Perbill::from_rational::<u32>(375, 1375);

		start_session(2);
		start_session(3);

		assert_eq!(active_era(), 1);
		assert_eq!(mock::RewardRemainderUnbalanced::get(), maximum_payout - total_payout_0,);
		assert_eq!(
			*mock::staking_events().last().unwrap(),
			Event::EraPaid {
				era_index: 0,
				validator_payout: total_payout_0,
				remainder: maximum_payout - total_payout_0
			}
		);

		// make note of total issuance before rewards.
		let total_issuance_0 = asset::total_issuance::<Test>();

		mock::make_all_reward_payment(0);

		// total issuance should have increased
		let total_issuance_1 = asset::total_issuance::<Test>();
		assert_eq!(total_issuance_1, total_issuance_0 + total_payout_0);

		assert_eq_error_rate!(
			asset::total_balance::<Test>(&11),
			init_balance_11 + part_for_11 * total_payout_0 * 2 / 3,
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<Test>(&21),
			init_balance_21 + part_for_21 * total_payout_0 * 1 / 3,
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<Test>(&101),
			init_balance_101 +
				part_for_101_from_11 * total_payout_0 * 2 / 3 +
				part_for_101_from_21 * total_payout_0 * 1 / 3,
			2
		);

		assert_eq_uvec!(Session::validators(), vec![11, 21]);
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);

		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_1 = current_total_payout_for_duration(reward_time_per_era());

		mock::start_active_era(2);
		assert_eq!(
			mock::RewardRemainderUnbalanced::get(),
			maximum_payout * 2 - total_payout_0 - total_payout_1,
		);
		assert_eq!(
			*mock::staking_events().last().unwrap(),
			Event::EraPaid {
				era_index: 1,
				validator_payout: total_payout_1,
				remainder: maximum_payout - total_payout_1
			}
		);
		mock::make_all_reward_payment(1);

		assert_eq!(asset::total_issuance::<Test>(), total_issuance_1 + total_payout_1);
		assert_eq_error_rate!(
			asset::total_balance::<Test>(&11),
			init_balance_11 + part_for_11 * (total_payout_0 * 2 / 3 + total_payout_1),
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<Test>(&21),
			init_balance_21 + part_for_21 * total_payout_0 * 1 / 3,
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<Test>(&101),
			init_balance_101 +
				part_for_101_from_11 * (total_payout_0 * 2 / 3 + total_payout_1) +
				part_for_101_from_21 * total_payout_0 * 1 / 3,
			2
		);
	});
}

#[test]
fn staking_should_work() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// remember + compare this along with the test.
		assert_eq_uvec!(validator_controllers(), vec![21, 11]);

		// put some money in account that we'll use.
		for i in 1..5 {
			let _ = asset::set_stakeable_balance::<Test>(&i, 2000);
		}

		// --- Block 2:
		start_session(2);
		// add a new candidate for being a validator. account 3 controlled by 4.
		assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 1500, RewardDestination::Account(3)));
		assert_ok!(Staking::validate(RuntimeOrigin::signed(3), ValidatorPrefs::default()));
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(3),
			SessionKeys { other: 4.into() },
			vec![]
		));

		// No effects will be seen so far.
		assert_eq_uvec!(validator_controllers(), vec![21, 11]);

		// --- Block 3:
		start_session(3);

		// No effects will be seen so far. Era has not been yet triggered.
		assert_eq_uvec!(validator_controllers(), vec![21, 11]);

		// --- Block 4: the validators will now be queued.
		start_session(4);
		assert_eq!(active_era(), 1);

		// --- Block 5: the validators are still in queue.
		start_session(5);

		// --- Block 6: the validators will now be changed.
		start_session(6);

		assert_eq_uvec!(validator_controllers(), vec![21, 3]);
		// --- Block 6: Unstake 4 as a validator, freeing up the balance stashed in 3
		// 4 will chill
		Staking::chill(RuntimeOrigin::signed(3)).unwrap();

		// --- Block 7: nothing. 3 is still there.
		start_session(7);
		assert_eq_uvec!(validator_controllers(), vec![21, 3]);

		// --- Block 8:
		start_session(8);

		// --- Block 9: 4 will not be a validator.
		start_session(9);
		assert_eq_uvec!(validator_controllers(), vec![21, 11]);

		// Note: the stashed value of 4 is still lock
		assert_eq!(
			Staking::ledger(3.into()).unwrap(),
			StakingLedgerInspect {
				stash: 3,
				total: 1500,
				active: 1500,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		// e.g. it cannot reserve more than 500 that it has free from the total 2000
		assert_noop!(Balances::reserve(&3, 501), DispatchError::ConsumerRemaining);
		assert_ok!(Balances::reserve(&3, 409));
	});
}

#[test]
fn blocking_and_kicking_works() {
	ExtBuilder::default()
		.minimum_validator_count(1)
		.validator_count(4)
		.nominate(true)
		.build_and_execute(|| {
			// block validator 10/11
			assert_ok!(Staking::validate(
				RuntimeOrigin::signed(11),
				ValidatorPrefs { blocked: true, ..Default::default() }
			));
			// attempt to nominate from 100/101...
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(101), vec![11]));
			// should have worked since we're already nominated them
			assert_eq!(Nominators::<Test>::get(&101).unwrap().targets, vec![11]);
			// kick the nominator
			assert_ok!(Staking::kick(RuntimeOrigin::signed(11), vec![101]));
			// should have been kicked now
			assert!(Nominators::<Test>::get(&101).unwrap().targets.is_empty());
			// attempt to nominate from 100/101...
			assert_noop!(
				Staking::nominate(RuntimeOrigin::signed(101), vec![11]),
				Error::<Test>::BadTarget
			);
		});
}

#[test]
fn less_than_needed_candidates_works() {
	ExtBuilder::default()
		.minimum_validator_count(1)
		.validator_count(4)
		.nominate(false)
		.build_and_execute(|| {
			assert_eq!(ValidatorCount::<Test>::get(), 4);
			assert_eq!(MinimumValidatorCount::<Test>::get(), 1);
			assert_eq_uvec!(validator_controllers(), vec![31, 21, 11]);

			mock::start_active_era(1);

			// Previous set is selected. NO election algorithm is even executed.
			assert_eq_uvec!(validator_controllers(), vec![31, 21, 11]);

			// But the exposure is updated in a simple way. No external votes exists.
			// This is purely self-vote.
			assert!(ErasStakersPaged::<Test>::iter_prefix_values((active_era(),))
				.all(|exposure| exposure.others.is_empty()));
		});
}

#[test]
fn no_candidate_emergency_condition() {
	ExtBuilder::default()
		.minimum_validator_count(1)
		.validator_count(15)
		.set_status(41, StakerStatus::Validator)
		.nominate(false)
		.build_and_execute(|| {
			// initial validators
			assert_eq_uvec!(validator_controllers(), vec![11, 21, 31, 41]);
			let prefs = ValidatorPrefs { commission: Perbill::one(), ..Default::default() };
			Validators::<Test>::insert(11, prefs.clone());

			// set the minimum validator count.
			MinimumValidatorCount::<Test>::put(11);

			// try to chill
			let res = Staking::chill(RuntimeOrigin::signed(11));
			assert_ok!(res);

			let current_era = CurrentEra::<Test>::get();

			// try trigger new era
			mock::run_to_block(21);
			assert_eq!(*staking_events().last().unwrap(), Event::StakingElectionFailed);
			// No new era is created
			assert_eq!(current_era, CurrentEra::<Test>::get());

			// Go to far further session to see if validator have changed
			mock::run_to_block(100);

			// Previous ones are elected. chill is not effective in active era (as era hasn't
			// changed)
			assert_eq_uvec!(validator_controllers(), vec![11, 21, 31, 41]);
			// The chill is still pending.
			assert!(!Validators::<Test>::contains_key(11));
			// No new era is created.
			assert_eq!(current_era, CurrentEra::<Test>::get());
		});
}

#[test]
fn nominating_and_rewards_should_work() {
	ExtBuilder::default()
		.nominate(false)
		.set_status(41, StakerStatus::Validator)
		.set_status(11, StakerStatus::Idle)
		.set_status(31, StakerStatus::Idle)
		.build_and_execute(|| {
			// initial validators.
			assert_eq_uvec!(validator_controllers(), vec![41, 21]);

			// re-validate with 11 and 31.
			assert_ok!(Staking::validate(RuntimeOrigin::signed(11), Default::default()));
			assert_ok!(Staking::validate(RuntimeOrigin::signed(31), Default::default()));

			// Set payee to controller.
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(21), RewardDestination::Stash));
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(31), RewardDestination::Stash));
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(41), RewardDestination::Stash));

			// give the man some money
			let initial_balance = 1000;
			for i in [1, 3, 5, 11, 21].iter() {
				let _ = asset::set_stakeable_balance::<Test>(&i, initial_balance);
			}

			// bond two account pairs and state interest in nomination.
			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(1),
				1000,
				RewardDestination::Account(1)
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(1), vec![11, 21, 31]));

			// the second nominator is virtual.
			bond_virtual_nominator(3, 333, 1000, vec![11, 21, 41]);

			// the total reward for era 0
			let total_payout_0 = current_total_payout_for_duration(reward_time_per_era());
			Pallet::<Test>::reward_by_ids(vec![(41, 1)]);
			Pallet::<Test>::reward_by_ids(vec![(21, 1)]);

			mock::start_active_era(1);

			// 10 and 20 have more votes, they will be chosen.
			assert_eq_uvec!(validator_controllers(), vec![21, 11]);

			// old validators must have already received some rewards.
			let initial_balance_41 = asset::total_balance::<Test>(&41);
			let mut initial_balance_21 = asset::total_balance::<Test>(&21);
			mock::make_all_reward_payment(0);
			assert_eq!(asset::total_balance::<Test>(&41), initial_balance_41 + total_payout_0 / 2);
			assert_eq!(asset::total_balance::<Test>(&21), initial_balance_21 + total_payout_0 / 2);
			initial_balance_21 = asset::total_balance::<Test>(&21);

			assert_eq!(ErasStakersPaged::<Test>::iter_prefix_values((active_era(),)).count(), 2);
			assert_eq!(
				Staking::eras_stakers(active_era(), &11),
				Exposure {
					total: 1000 + 800,
					own: 1000,
					others: vec![
						IndividualExposure { who: 1, value: 400 },
						IndividualExposure { who: 3, value: 400 },
					]
				},
			);
			assert_eq!(
				Staking::eras_stakers(active_era(), &21),
				Exposure {
					total: 1000 + 1200,
					own: 1000,
					others: vec![
						IndividualExposure { who: 1, value: 600 },
						IndividualExposure { who: 3, value: 600 },
					]
				},
			);

			// the total reward for era 1
			let total_payout_1 = current_total_payout_for_duration(reward_time_per_era());
			Pallet::<Test>::reward_by_ids(vec![(21, 2)]);
			Pallet::<Test>::reward_by_ids(vec![(11, 1)]);

			mock::start_active_era(2);

			// nothing else will happen, era ends and rewards are paid again, it is expected that
			// nominators will also be paid. See below

			mock::make_all_reward_payment(1);
			let payout_for_11 = total_payout_1 / 3;
			let payout_for_21 = 2 * total_payout_1 / 3;
			// Nominator 2: has [400/1800 ~ 2/9 from 10] + [600/2200 ~ 3/11 from 21]'s reward. ==>
			// 2/9 + 3/11
			assert_eq_error_rate!(
				asset::total_balance::<Test>(&1),
				initial_balance + (2 * payout_for_11 / 9 + 3 * payout_for_21 / 11),
				2,
			);
			// Nominator 3: has [400/1800 ~ 2/9 from 10] + [600/2200 ~ 3/11 from 21]'s reward. ==>
			// 2/9 + 3/11
			assert_eq!(asset::stakeable_balance::<Test>(&3), initial_balance);
			// 333 is the reward destination for 3.
			assert_eq_error_rate!(
				asset::total_balance::<Test>(&333),
				2 * payout_for_11 / 9 + 3 * payout_for_21 / 11,
				2
			);

			// Validator 11: got 800 / 1800 external stake => 8/18 =? 4/9 => Validator's share = 5/9
			assert_eq_error_rate!(
				asset::total_balance::<Test>(&11),
				initial_balance + 5 * payout_for_11 / 9,
				2,
			);
			// Validator 21: got 1200 / 2200 external stake => 12/22 =? 6/11 => Validator's share =
			// 5/11
			assert_eq_error_rate!(
				asset::total_balance::<Test>(&21),
				initial_balance_21 + 5 * payout_for_21 / 11,
				2,
			);
		});
}

#[test]
fn nominators_also_get_slashed_pro_rata() {
	ExtBuilder::default()
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.build_and_execute(|| {
			mock::start_active_era(1);
			let slash_percent = Perbill::from_percent(5);
			let initial_exposure = Staking::eras_stakers(active_era(), &11);
			// 101 is a nominator for 11
			assert_eq!(initial_exposure.others.first().unwrap().who, 101);

			// staked values;
			let nominator_stake = Staking::ledger(101.into()).unwrap().active;
			let nominator_balance = balances(&101).0;
			let validator_stake = Staking::ledger(11.into()).unwrap().active;
			let validator_balance = balances(&11).0;
			let exposed_stake = initial_exposure.total;
			let exposed_validator = initial_exposure.own;
			let exposed_nominator = initial_exposure.others.first().unwrap().value;

			// 11 goes offline
			on_offence_now(&[offence_from(11, None)], &[slash_percent]);

			// both stakes must have been decreased.
			assert!(Staking::ledger(101.into()).unwrap().active < nominator_stake);
			assert!(Staking::ledger(11.into()).unwrap().active < validator_stake);

			let slash_amount = slash_percent * exposed_stake;
			let validator_share =
				Perbill::from_rational(exposed_validator, exposed_stake) * slash_amount;
			let nominator_share =
				Perbill::from_rational(exposed_nominator, exposed_stake) * slash_amount;

			// both slash amounts need to be positive for the test to make sense.
			assert!(validator_share > 0);
			assert!(nominator_share > 0);

			// both stakes must have been decreased pro-rata.
			assert_eq!(
				Staking::ledger(101.into()).unwrap().active,
				nominator_stake - nominator_share
			);
			assert_eq!(
				Staking::ledger(11.into()).unwrap().active,
				validator_stake - validator_share
			);
			assert_eq!(
				balances(&101).0, // free balance
				nominator_balance - nominator_share,
			);
			assert_eq!(
				balances(&11).0, // free balance
				validator_balance - validator_share,
			);
		});
}

#[test]
fn double_staking_should_fail() {
	// should test (in the same order):
	// * an account already bonded as stash cannot be stashed again.
	// * an account already bonded as stash cannot nominate.
	// * an account already bonded as controller can nominate.
	ExtBuilder::default().try_state(false).build_and_execute(|| {
		let arbitrary_value = 5;
		let (stash, controller) = testing_utils::create_unique_stash_controller::<Test>(
			0,
			arbitrary_value,
			RewardDestination::Staked,
			false,
		)
		.unwrap();

		// 4 = not used so far,  stash => not allowed.
		assert_noop!(
			Staking::bond(
				RuntimeOrigin::signed(stash),
				arbitrary_value.into(),
				RewardDestination::Staked,
			),
			Error::<Test>::AlreadyBonded,
		);
		// stash => attempting to nominate should fail.
		assert_noop!(
			Staking::nominate(RuntimeOrigin::signed(stash), vec![1]),
			Error::<Test>::NotController
		);
		// controller => nominating should work.
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(controller), vec![1]));
	});
}

#[test]
fn double_controlling_attempt_should_fail() {
	// should test (in the same order):
	// * an account already bonded as controller CANNOT be reused as the controller of another
	//   account.
	ExtBuilder::default().try_state(false).build_and_execute(|| {
		let arbitrary_value = 5;
		let (stash, _) = testing_utils::create_unique_stash_controller::<Test>(
			0,
			arbitrary_value,
			RewardDestination::Staked,
			false,
		)
		.unwrap();

		// Note that controller (same as stash) is reused => no-op.
		assert_noop!(
			Staking::bond(
				RuntimeOrigin::signed(stash),
				arbitrary_value.into(),
				RewardDestination::Staked,
			),
			Error::<Test>::AlreadyBonded,
		);
	});
}

#[test]
fn session_and_eras_work_simple() {
	ExtBuilder::default().period(1).build_and_execute(|| {
		assert_eq!(active_era(), 0);
		assert_eq!(current_era(), 0);
		assert_eq!(Session::current_index(), 1);
		assert_eq!(System::block_number(), 1);

		// Session 1: this is basically a noop. This has already been started.
		start_session(1);
		assert_eq!(Session::current_index(), 1);
		assert_eq!(active_era(), 0);
		assert_eq!(System::block_number(), 1);

		// Session 2: No change.
		start_session(2);
		assert_eq!(Session::current_index(), 2);
		assert_eq!(active_era(), 0);
		assert_eq!(System::block_number(), 2);

		// Session 3: Era increment.
		start_session(3);
		assert_eq!(Session::current_index(), 3);
		assert_eq!(active_era(), 1);
		assert_eq!(System::block_number(), 3);

		// Session 4: No change.
		start_session(4);
		assert_eq!(Session::current_index(), 4);
		assert_eq!(active_era(), 1);
		assert_eq!(System::block_number(), 4);

		// Session 5: No change.
		start_session(5);
		assert_eq!(Session::current_index(), 5);
		assert_eq!(active_era(), 1);
		assert_eq!(System::block_number(), 5);

		// Session 6: Era increment.
		start_session(6);
		assert_eq!(Session::current_index(), 6);
		assert_eq!(active_era(), 2);
		assert_eq!(System::block_number(), 6);
	});
}

#[test]
fn session_and_eras_work_complex() {
	ExtBuilder::default().period(5).build_and_execute(|| {
		assert_eq!(active_era(), 0);
		assert_eq!(Session::current_index(), 0);
		assert_eq!(System::block_number(), 1);

		start_session(1);
		assert_eq!(Session::current_index(), 1);
		assert_eq!(active_era(), 0);
		assert_eq!(System::block_number(), 5);

		start_session(2);
		assert_eq!(Session::current_index(), 2);
		assert_eq!(active_era(), 0);
		assert_eq!(System::block_number(), 10);

		start_session(3);
		assert_eq!(Session::current_index(), 3);
		assert_eq!(active_era(), 1);
		assert_eq!(System::block_number(), 15);

		start_session(4);
		assert_eq!(Session::current_index(), 4);
		assert_eq!(active_era(), 1);
		assert_eq!(System::block_number(), 20);

		start_session(5);
		assert_eq!(Session::current_index(), 5);
		assert_eq!(active_era(), 1);
		assert_eq!(System::block_number(), 25);

		start_session(6);
		assert_eq!(Session::current_index(), 6);
		assert_eq!(active_era(), 2);
		assert_eq!(System::block_number(), 30);
	});
}

#[test]
fn forcing_new_era_works() {
	ExtBuilder::default().build_and_execute(|| {
		// normal flow of session.
		start_session(1);
		assert_eq!(active_era(), 0);

		start_session(2);
		assert_eq!(active_era(), 0);

		start_session(3);
		assert_eq!(active_era(), 1);

		// no era change.
		Staking::set_force_era(Forcing::ForceNone);

		start_session(4);
		assert_eq!(active_era(), 1);

		start_session(5);
		assert_eq!(active_era(), 1);

		start_session(6);
		assert_eq!(active_era(), 1);

		start_session(7);
		assert_eq!(active_era(), 1);

		// back to normal.
		// this immediately starts a new session.
		Staking::set_force_era(Forcing::NotForcing);

		start_session(8);
		assert_eq!(active_era(), 1);

		start_session(9);
		assert_eq!(active_era(), 2);
		// forceful change
		Staking::set_force_era(Forcing::ForceAlways);

		start_session(10);
		assert_eq!(active_era(), 2);

		start_session(11);
		assert_eq!(active_era(), 3);

		start_session(12);
		assert_eq!(active_era(), 4);

		// just one forceful change
		Staking::set_force_era(Forcing::ForceNew);
		start_session(13);
		assert_eq!(active_era(), 5);
		assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);

		start_session(14);
		assert_eq!(active_era(), 6);

		start_session(15);
		assert_eq!(active_era(), 6);
	});
}

#[test]
fn cannot_transfer_staked_balance() {
	// Tests that a stash account cannot transfer funds
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Confirm account 11 is stashed
		assert_eq!(Staking::bonded(&11), Some(11));
		// Confirm account 11 has some stakeable balance
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		// Confirm account 11 is totally staked
		assert_eq!(Staking::eras_stakers(active_era(), &11).total, 1000);
		// Confirm account 11 cannot transfer as a result
		assert_noop!(
			Balances::transfer_allow_death(RuntimeOrigin::signed(11), 21, 1),
			TokenError::Frozen,
		);

		// Give account 11 extra free balance
		let _ = asset::set_stakeable_balance::<Test>(&11, 10000);
		// Confirm that account 11 can now transfer some balance
		assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(11), 21, 1));
	});
}

#[test]
fn cannot_transfer_staked_balance_2() {
	// Tests that a stash account cannot transfer funds
	// Same test as above but with 20, and more accurate.
	// 21 has 2000 free balance but 1000 at stake
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Confirm account 21 is stashed
		assert_eq!(Staking::bonded(&21), Some(21));
		// Confirm account 21 has some free balance
		assert_eq!(asset::stakeable_balance::<Test>(&21), 2000);
		// Confirm account 21 (via controller) is totally staked
		assert_eq!(Staking::eras_stakers(active_era(), &21).total, 1000);
		// Confirm account 21 cannot transfer more than 1000
		assert_noop!(
			Balances::transfer_allow_death(RuntimeOrigin::signed(21), 21, 1001),
			TokenError::Frozen,
		);
		// Confirm account 21 needs to leave at least ED in free balance to be able to transfer
		assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(21), 21, 1000));
	});
}

#[test]
fn cannot_reserve_staked_balance() {
	// Checks that a bonded account cannot reserve balance from free balance
	ExtBuilder::default().build_and_execute(|| {
		// Confirm account 11 is stashed
		assert_eq!(Staking::bonded(&11), Some(11));
		// Confirm account 11 is totally staked
		assert_eq!(asset::staked::<Test>(&11), 1000);

		// Confirm account 11 cannot reserve as a result
		assert_noop!(Balances::reserve(&11, 2), BalancesError::<Test, _>::InsufficientBalance);
		assert_noop!(Balances::reserve(&11, 1), DispatchError::ConsumerRemaining);

		// Give account 11 extra free balance
		let _ = asset::set_stakeable_balance::<Test>(&11, 1000 + 1000);
		assert_eq!(asset::free_to_stake::<Test>(&11), 1000);

		// Confirm account 11 can now reserve balance
		assert_ok!(Balances::reserve(&11, 500));

		// free to stake balance has reduced
		assert_eq!(asset::free_to_stake::<Test>(&11), 500);
	});
}

#[test]
fn locked_balance_can_be_staked() {
	// Checks that a bonded account cannot reserve balance from free balance
	ExtBuilder::default().build_and_execute(|| {
		// Confirm account 11 is stashed
		assert_eq!(Staking::bonded(&11), Some(11));
		assert_eq!(asset::staked::<Test>(&11), 1000);
		assert_eq!(asset::free_to_stake::<Test>(&11), 0);

		// add some staking balance to 11
		let _ = asset::set_stakeable_balance::<Test>(&11, 1000 + 1000);
		// free to stake is 1000
		assert_eq!(asset::free_to_stake::<Test>(&11), 1000);

		// lock some balance
		Balances::set_lock(*b"somelock", &11, 500, WithdrawReasons::all());

		// locked balance still available for staking
		assert_eq!(asset::free_to_stake::<Test>(&11), 1000);

		// can stake free balance
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), 500));
		assert_eq!(asset::staked::<Test>(&11), 1500);

		// Can stake the locked balance
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), 500));
		assert_eq!(asset::staked::<Test>(&11), 2000);
		// no balance left to stake
		assert_eq!(asset::free_to_stake::<Test>(&11), 0);

		// this does not fail if someone tries to stake more than free balance but just stakes
		// whatever is available. (not sure if that is the best way, but we keep it backward
		// compatible)
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), 10));
		// no extra balance staked.
		assert_eq!(asset::staked::<Test>(&11), 2000);
	});
}

#[test]
fn reward_destination_works() {
	// Rewards go to the correct destination as determined in Payee
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Check that account 11 is a validator
		assert!(Session::validators().contains(&11));
		// Check the balance of the validator account
		assert_eq!(asset::total_balance::<Test>(&10), 1);
		// Check the balance of the stash account
		assert_eq!(asset::total_balance::<Test>(&11), 1001);
		// Check how much is at stake
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_0 = current_total_payout_for_duration(reward_time_per_era());
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);

		mock::start_active_era(1);
		mock::make_all_reward_payment(0);

		// Check that RewardDestination is Staked
		assert_eq!(Staking::payee(11.into()), Some(RewardDestination::Staked));
		// Check that reward went to the stash account of validator
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000 + total_payout_0);
		// Check that amount at stake increased accordingly
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + total_payout_0,
				active: 1000 + total_payout_0,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// (era 0, page 0) is claimed
		assert_eq!(ClaimedRewards::<Test>::get(0, &11), vec![0]);

		// Change RewardDestination to Stash
		<Payee<Test>>::insert(&11, RewardDestination::Stash);

		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_1 = current_total_payout_for_duration(reward_time_per_era());
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);

		mock::start_active_era(2);
		mock::make_all_reward_payment(1);

		// Check that RewardDestination is Stash
		assert_eq!(Staking::payee(11.into()), Some(RewardDestination::Stash));
		// Check that reward went to the stash account
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000 + total_payout_0 + total_payout_1);
		// Record this value
		let recorded_stash_balance = 1000 + total_payout_0 + total_payout_1;
		// Check that amount at stake is NOT increased
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + total_payout_0,
				active: 1000 + total_payout_0,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// (era 1, page 0) is claimed
		assert_eq!(ClaimedRewards::<Test>::get(1, &11), vec![0]);

		// Change RewardDestination to Account
		<Payee<Test>>::insert(&11, RewardDestination::Account(11));

		// Check controller balance
		assert_eq!(asset::stakeable_balance::<Test>(&11), 23150);

		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_2 = current_total_payout_for_duration(reward_time_per_era());
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);

		mock::start_active_era(3);
		mock::make_all_reward_payment(2);

		// Check that RewardDestination is Account(11)
		assert_eq!(Staking::payee(11.into()), Some(RewardDestination::Account(11)));
		// Check that reward went to the controller account
		assert_eq!(asset::stakeable_balance::<Test>(&11), recorded_stash_balance + total_payout_2);
		// Check that amount at stake is NOT increased
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + total_payout_0,
				active: 1000 + total_payout_0,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// (era 2, page 0) is claimed
		assert_eq!(ClaimedRewards::<Test>::get(2, &11), vec![0]);
	});
}

#[test]
fn validator_payment_prefs_work() {
	// Test that validator preferences are correctly honored
	// Note: unstake threshold is being directly tested in slashing tests.
	// This test will focus on validator payment.
	ExtBuilder::default().build_and_execute(|| {
		let commission = Perbill::from_percent(40);
		<Validators<Test>>::insert(&11, ValidatorPrefs { commission, ..Default::default() });

		// Reward stash so staked ratio doesn't change.
		<Payee<Test>>::insert(&11, RewardDestination::Stash);
		<Payee<Test>>::insert(&101, RewardDestination::Stash);

		mock::start_active_era(1);
		mock::make_all_reward_payment(0);

		let balance_era_1_11 = asset::total_balance::<Test>(&11);
		let balance_era_1_101 = asset::total_balance::<Test>(&101);

		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_1 = current_total_payout_for_duration(reward_time_per_era());
		let exposure_1 = Staking::eras_stakers(active_era(), &11);
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);

		mock::start_active_era(2);
		mock::make_all_reward_payment(1);

		let taken_cut = commission * total_payout_1;
		let shared_cut = total_payout_1 - taken_cut;
		let reward_of_10 = shared_cut * exposure_1.own / exposure_1.total + taken_cut;
		let reward_of_100 = shared_cut * exposure_1.others[0].value / exposure_1.total;
		assert_eq_error_rate!(
			asset::total_balance::<Test>(&11),
			balance_era_1_11 + reward_of_10,
			2
		);
		assert_eq_error_rate!(
			asset::total_balance::<Test>(&101),
			balance_era_1_101 + reward_of_100,
			2
		);
	});
}

#[test]
fn bond_extra_works() {
	// Tests that extra `free_balance` in the stash can be added to stake
	// NOTE: this tests only verifies `StakingLedger` for correct updates
	// See `bond_extra_and_withdraw_unbonded_works` for more details and updates on `Exposure`.
	ExtBuilder::default().build_and_execute(|| {
		// Check that account 10 is a validator
		assert!(<Validators<Test>>::contains_key(11));
		// Check that account 10 is bonded to account 11
		assert_eq!(Staking::bonded(&11), Some(11));
		// Check how much is at stake
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Give account 11 some large free balance greater than total
		let _ = asset::set_stakeable_balance::<Test>(&11, 1000000);

		// Call the bond_extra function from controller, add only 100
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), 100));
		// There should be 100 more `total` and `active` in the ledger
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 1000 + 100,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Call the bond_extra function with a large number, should handle it
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), Balance::max_value()));
		// The full amount of the funds should now be in the total and active
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000000,
				active: 1000000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
	});
}

#[test]
fn bond_extra_controller_bad_state_works() {
	ExtBuilder::default().try_state(false).build_and_execute(|| {
		assert_eq!(StakingLedger::<Test>::get(StakingAccount::Stash(31)).unwrap().stash, 31);

		// simulate ledger in bad state: the controller 41 is associated to the stash 31 and 41.
		Bonded::<Test>::insert(31, 41);

		// we confirm that the ledger is in bad state: 31 has 41 as controller and when fetching
		// the ledger associated with the controller 41, its stash is 41 (and not 31).
		assert_eq!(Ledger::<Test>::get(41).unwrap().stash, 41);

		// if the ledger is in this bad state, the `bond_extra` should fail.
		assert_noop!(Staking::bond_extra(RuntimeOrigin::signed(31), 10), Error::<Test>::BadState);
	})
}

#[test]
fn bond_extra_and_withdraw_unbonded_works() {
	//
	// * Should test
	// * Given an account being bonded [and chosen as a validator](not mandatory)
	// * It can add extra funds to the bonded account.
	// * it can unbond a portion of its funds from the stash account.
	// * Once the unbonding period is done, it can actually take the funds out of the stash.
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Set payee to stash.
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));

		// Give account 11 some large free balance greater than total
		let _ = asset::set_stakeable_balance::<Test>(&11, 1000000);

		// ensure it has the correct balance.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000000);

		// Initial config should be correct
		assert_eq!(active_era(), 0);

		// confirm that 10 is a normal validator and gets paid at the end of the era.
		mock::start_active_era(1);

		// Initial state of 11
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		assert_eq!(
			Staking::eras_stakers(active_era(), &11),
			Exposure { total: 1000, own: 1000, others: vec![] }
		);

		// deposit the extra 100 units
		Staking::bond_extra(RuntimeOrigin::signed(11), 100).unwrap();

		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 1000 + 100,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		// Exposure is a snapshot! only updated after the next era update.
		assert_ne!(
			Staking::eras_stakers(active_era(), &11),
			Exposure { total: 1000 + 100, own: 1000 + 100, others: vec![] }
		);

		// trigger next era.
		mock::start_active_era(2);
		assert_eq!(active_era(), 2);

		// ledger should be the same.
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 1000 + 100,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		// Exposure is now updated.
		assert_eq!(
			Staking::eras_stakers(active_era(), &11),
			Exposure { total: 1000 + 100, own: 1000 + 100, others: vec![] }
		);

		// Unbond almost all of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 1000).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 100,
				unlocking: bounded_vec![UnlockChunk { value: 1000, era: 2 + 3 }],
				legacy_claimed_rewards: bounded_vec![],
			},
		);

		// Attempting to free the balances now will fail. 2 eras need to pass.
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 100,
				unlocking: bounded_vec![UnlockChunk { value: 1000, era: 2 + 3 }],
				legacy_claimed_rewards: bounded_vec![],
			},
		);

		// trigger next era.
		mock::start_active_era(3);

		// nothing yet
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 100,
				unlocking: bounded_vec![UnlockChunk { value: 1000, era: 2 + 3 }],
				legacy_claimed_rewards: bounded_vec![],
			},
		);

		// trigger next era.
		mock::start_active_era(5);

		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		// Now the value is free and the staking ledger is updated.
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 100,
				active: 100,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			},
		);
	})
}

#[test]
fn many_unbond_calls_should_work() {
	ExtBuilder::default().build_and_execute(|| {
		let mut current_era = 0;
		// locked at era MaxUnlockingChunks - 1 until 3

		let max_unlocking_chunks = <<Test as Config>::MaxUnlockingChunks as Get<u32>>::get();

		for i in 0..max_unlocking_chunks - 1 {
			// There is only 1 chunk per era, so we need to be in a new era to create a chunk.
			current_era = i as u32;
			mock::start_active_era(current_era);
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1));
		}

		current_era += 1;
		mock::start_active_era(current_era);

		// This chunk is locked at `current_era` through `current_era + 2` (because
		// `BondingDuration` == 3).
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1));
		assert_eq!(
			Staking::ledger(11.into()).map(|l| l.unlocking.len()).unwrap(),
			<<Test as Config>::MaxUnlockingChunks as Get<u32>>::get() as usize
		);

		// even though the number of unlocked chunks is the same as `MaxUnlockingChunks`,
		// unbonding works as expected.
		for i in current_era..(current_era + max_unlocking_chunks) - 1 {
			// There is only 1 chunk per era, so we need to be in a new era to create a chunk.
			current_era = i as u32;
			mock::start_active_era(current_era);
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1));
		}

		// only slots within last `BondingDuration` are filled.
		assert_eq!(
			Staking::ledger(11.into()).map(|l| l.unlocking.len()).unwrap(),
			<<Test as Config>::BondingDuration>::get() as usize
		);
	})
}

#[test]
fn auto_withdraw_may_not_unlock_all_chunks() {
	ExtBuilder::default().build_and_execute(|| {
		// set `MaxUnlockingChunks` to a low number to test case when the unbonding period
		// is larger than the number of unlocking chunks available, which may result on a
		// `Error::NoMoreChunks`, even when the auto-withdraw tries to release locked chunks.
		MaxUnlockingChunks::set(1);

		let mut current_era = 0;

		// fills the chunking slots for account
		mock::start_active_era(current_era);
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1));

		current_era += 1;
		mock::start_active_era(current_era);

		// unbonding will fail because i) there are no remaining chunks and ii) no filled chunks
		// can be released because current chunk hasn't stay in the queue for at least
		// `BondingDuration`
		assert_noop!(Staking::unbond(RuntimeOrigin::signed(11), 1), Error::<Test>::NoMoreChunks);

		// fast-forward a few eras for unbond to be successful with implicit withdraw
		current_era += 10;
		mock::start_active_era(current_era);
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1));
	})
}

#[test]
fn rebond_works() {
	//
	// * Should test
	// * Given an account being bonded [and chosen as a validator](not mandatory)
	// * it can unbond a portion of its funds from the stash account.
	// * it can re-bond a portion of the funds scheduled to unlock.
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Set payee to stash.
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));

		// Give account 11 some large free balance greater than total
		let _ = asset::set_stakeable_balance::<Test>(&11, 1000000);

		// confirm that 10 is a normal validator and gets paid at the end of the era.
		mock::start_active_era(1);

		// Initial state of 11
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		mock::start_active_era(2);
		assert_eq!(active_era(), 2);

		// Try to rebond some funds. We get an error since no fund is unbonded.
		assert_noop!(Staking::rebond(RuntimeOrigin::signed(11), 500), Error::<Test>::NoUnlockChunk);

		// Unbond almost all of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 900).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 100,
				unlocking: bounded_vec![UnlockChunk { value: 900, era: 2 + 3 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Re-bond all the funds unbonded.
		Staking::rebond(RuntimeOrigin::signed(11), 900).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Unbond almost all of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 900).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 100,
				unlocking: bounded_vec![UnlockChunk { value: 900, era: 5 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Re-bond part of the funds unbonded.
		Staking::rebond(RuntimeOrigin::signed(11), 500).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 600,
				unlocking: bounded_vec![UnlockChunk { value: 400, era: 5 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Re-bond the remainder of the funds unbonded.
		Staking::rebond(RuntimeOrigin::signed(11), 500).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Unbond parts of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
		Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
		Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 100,
				unlocking: bounded_vec![UnlockChunk { value: 900, era: 5 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Re-bond part of the funds unbonded.
		Staking::rebond(RuntimeOrigin::signed(11), 500).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 600,
				unlocking: bounded_vec![UnlockChunk { value: 400, era: 5 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);
	})
}

#[test]
fn rebond_is_fifo() {
	// Rebond should proceed by reversing the most recent bond operations.
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Set payee to stash.
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));

		// Give account 11 some large free balance greater than total
		let _ = asset::set_stakeable_balance::<Test>(&11, 1000000);

		// confirm that 10 is a normal validator and gets paid at the end of the era.
		mock::start_active_era(1);

		// Initial state of 10
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		mock::start_active_era(2);

		// Unbond some of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 400).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 600,
				unlocking: bounded_vec![UnlockChunk { value: 400, era: 2 + 3 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		mock::start_active_era(3);

		// Unbond more of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 300,
				unlocking: bounded_vec![
					UnlockChunk { value: 400, era: 2 + 3 },
					UnlockChunk { value: 300, era: 3 + 3 },
				],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		mock::start_active_era(4);

		// Unbond yet more of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 200).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 100,
				unlocking: bounded_vec![
					UnlockChunk { value: 400, era: 2 + 3 },
					UnlockChunk { value: 300, era: 3 + 3 },
					UnlockChunk { value: 200, era: 4 + 3 },
				],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Re-bond half of the unbonding funds.
		Staking::rebond(RuntimeOrigin::signed(11), 400).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![
					UnlockChunk { value: 400, era: 2 + 3 },
					UnlockChunk { value: 100, era: 3 + 3 },
				],
				legacy_claimed_rewards: bounded_vec![],
			}
		);
	})
}

#[test]
fn rebond_emits_right_value_in_event() {
	// When a user calls rebond with more than can be rebonded, things succeed,
	// and the rebond event emits the actual value rebonded.
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Set payee to stash.
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));

		// Give account 11 some large free balance greater than total
		let _ = asset::set_stakeable_balance::<Test>(&11, 1000000);

		// confirm that 10 is a normal validator and gets paid at the end of the era.
		mock::start_active_era(1);

		// Unbond almost all of the funds in stash.
		Staking::unbond(RuntimeOrigin::signed(11), 900).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 100,
				unlocking: bounded_vec![UnlockChunk { value: 900, era: 1 + 3 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// Re-bond less than the total
		Staking::rebond(RuntimeOrigin::signed(11), 100).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 200,
				unlocking: bounded_vec![UnlockChunk { value: 800, era: 1 + 3 }],
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		// Event emitted should be correct
		assert_eq!(*staking_events().last().unwrap(), Event::Bonded { stash: 11, amount: 100 });

		// Re-bond way more than available
		Staking::rebond(RuntimeOrigin::signed(11), 100_000).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		// Event emitted should be correct, only 800
		assert_eq!(*staking_events().last().unwrap(), Event::Bonded { stash: 11, amount: 800 });
	});
}

#[test]
fn max_staked_rewards_default_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(<MaxStakedRewards<Test>>::get(), None);

		let default_stakers_payout = current_total_payout_for_duration(reward_time_per_era());
		assert!(default_stakers_payout > 0);
		start_active_era(1);

		// the final stakers reward is the same as the reward before applied the cap.
		assert_eq!(ErasValidatorReward::<Test>::get(0).unwrap(), default_stakers_payout);

		// which is the same behaviour if the `MaxStakedRewards` is set to 100%.
		<MaxStakedRewards<Test>>::set(Some(Percent::from_parts(100)));

		let default_stakers_payout = current_total_payout_for_duration(reward_time_per_era());
		assert_eq!(ErasValidatorReward::<Test>::get(0).unwrap(), default_stakers_payout);
	})
}

#[test]
fn max_staked_rewards_works() {
	ExtBuilder::default().nominate(true).build_and_execute(|| {
		let max_staked_rewards = 10;

		// sets new max staked rewards through set_staking_configs.
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Set(Percent::from_percent(max_staked_rewards)),
		));

		assert_eq!(<MaxStakedRewards<Test>>::get(), Some(Percent::from_percent(10)));

		// check validators account state.
		assert_eq!(Session::validators().len(), 2);
		assert!(Session::validators().contains(&11) & Session::validators().contains(&21));
		// balance of the mock treasury account is 0
		assert_eq!(RewardRemainderUnbalanced::get(), 0);

		let max_stakers_payout = current_total_payout_for_duration(reward_time_per_era());

		start_active_era(1);

		let treasury_payout = RewardRemainderUnbalanced::get();
		let validators_payout = ErasValidatorReward::<Test>::get(0).unwrap();
		let total_payout = treasury_payout + validators_payout;

		// max stakers payout (without max staked rewards cap applied) is larger than the final
		// validator rewards. The final payment and remainder should be adjusted by redistributing
		// the era inflation to apply the cap...
		assert!(max_stakers_payout > validators_payout);

		// .. which means that the final validator payout is 10% of the total payout..
		assert_eq!(validators_payout, Percent::from_percent(max_staked_rewards) * total_payout);
		// .. and the remainder 90% goes to the treasury.
		assert_eq!(
			treasury_payout,
			Percent::from_percent(100 - max_staked_rewards) * (treasury_payout + validators_payout)
		);
	})
}

#[test]
fn reward_to_stake_works() {
	ExtBuilder::default()
		.nominate(false)
		.set_status(31, StakerStatus::Idle)
		.set_status(41, StakerStatus::Idle)
		.set_stake(21, 2000)
		.try_state(false)
		.build_and_execute(|| {
			assert_eq!(ValidatorCount::<Test>::get(), 2);
			// Confirm account 10 and 20 are validators
			assert!(<Validators<Test>>::contains_key(&11) && <Validators<Test>>::contains_key(&21));

			assert_eq!(Staking::eras_stakers(active_era(), &11).total, 1000);
			assert_eq!(Staking::eras_stakers(active_era(), &21).total, 2000);

			// Give the man some money.
			let _ = asset::set_stakeable_balance::<Test>(&10, 1000);
			let _ = asset::set_stakeable_balance::<Test>(&20, 1000);

			// Bypass logic and change current exposure
			EraInfo::<Test>::set_exposure(0, &21, Exposure { total: 69, own: 69, others: vec![] });
			<Ledger<Test>>::insert(
				&20,
				StakingLedgerInspect {
					stash: 21,
					total: 69,
					active: 69,
					unlocking: Default::default(),
					legacy_claimed_rewards: bounded_vec![],
				},
			);

			// Compute total payout now for whole duration as other parameter won't change
			let total_payout_0 = current_total_payout_for_duration(reward_time_per_era());
			Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
			Pallet::<Test>::reward_by_ids(vec![(21, 1)]);

			// New era --> rewards are paid --> stakes are changed
			mock::start_active_era(1);
			mock::make_all_reward_payment(0);

			assert_eq!(Staking::eras_stakers(active_era(), &11).total, 1000);
			assert_eq!(Staking::eras_stakers(active_era(), &21).total, 2000);

			let _11_balance = asset::stakeable_balance::<Test>(&11);
			assert_eq!(_11_balance, 1000 + total_payout_0 / 2);

			// Trigger another new era as the info are frozen before the era start.
			mock::start_active_era(2);

			// -- new infos
			assert_eq!(Staking::eras_stakers(active_era(), &11).total, 1000 + total_payout_0 / 2);
			assert_eq!(Staking::eras_stakers(active_era(), &21).total, 2000 + total_payout_0 / 2);
		});
}

#[test]
fn reap_stash_works() {
	ExtBuilder::default()
		.existential_deposit(10)
		.balance_factor(10)
		.build_and_execute(|| {
			// given
			assert_eq!(asset::staked::<Test>(&11), 10 * 1000);
			assert_eq!(Staking::bonded(&11), Some(11));

			assert!(<Ledger<Test>>::contains_key(&11));
			assert!(<Bonded<Test>>::contains_key(&11));
			assert!(<Validators<Test>>::contains_key(&11));
			assert!(<Payee<Test>>::contains_key(&11));

			// stash is not reapable
			assert_noop!(
				Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
				Error::<Test>::FundedTarget
			);

			// no easy way to cause an account to go below ED, we tweak their staking ledger
			// instead.
			Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 5));

			// reap-able
			assert_ok!(Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0));

			// then
			assert!(!<Ledger<Test>>::contains_key(&11));
			assert!(!<Bonded<Test>>::contains_key(&11));
			assert!(!<Validators<Test>>::contains_key(&11));
			assert!(!<Payee<Test>>::contains_key(&11));
			// lock is removed.
			assert_eq!(asset::staked::<Test>(&11), 0);
		});
}

#[test]
fn reap_stash_works_with_existential_deposit_zero() {
	ExtBuilder::default()
		.existential_deposit(0)
		.balance_factor(10)
		.build_and_execute(|| {
			// given
			assert_eq!(asset::staked::<Test>(&11), 10 * 1000);
			assert_eq!(Staking::bonded(&11), Some(11));

			assert!(<Ledger<Test>>::contains_key(&11));
			assert!(<Bonded<Test>>::contains_key(&11));
			assert!(<Validators<Test>>::contains_key(&11));
			assert!(<Payee<Test>>::contains_key(&11));

			// stash is not reapable
			assert_noop!(
				Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
				Error::<Test>::FundedTarget
			);

			// no easy way to cause an account to go below ED, we tweak their staking ledger
			// instead.
			Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 0));

			// reap-able
			assert_ok!(Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0));

			// then
			assert!(!<Ledger<Test>>::contains_key(&11));
			assert!(!<Bonded<Test>>::contains_key(&11));
			assert!(!<Validators<Test>>::contains_key(&11));
			assert!(!<Payee<Test>>::contains_key(&11));
			// lock is removed.
			assert_eq!(asset::staked::<Test>(&11), 0);
		});
}

#[test]
fn switching_roles() {
	// Test that it should be possible to switch between roles (nominator, validator, idle) with
	// minimal overhead.
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Reset reward destination
		for i in &[11, 21] {
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(*i), RewardDestination::Stash));
		}

		assert_eq_uvec!(validator_controllers(), vec![21, 11]);

		// put some money in account that we'll use.
		for i in 1..7 {
			let _ = Balances::deposit_creating(&i, 5000);
		}

		// add 2 nominators
		assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 2000, RewardDestination::Account(1)));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(1), vec![11, 5]));

		assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 500, RewardDestination::Account(3)));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![21, 1]));

		// add a new validator candidate
		assert_ok!(Staking::bond(RuntimeOrigin::signed(5), 1000, RewardDestination::Account(5)));
		assert_ok!(Staking::validate(RuntimeOrigin::signed(5), ValidatorPrefs::default()));
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(5),
			SessionKeys { other: 6.into() },
			vec![]
		));

		mock::start_active_era(1);

		// with current nominators 11 and 5 have the most stake
		assert_eq_uvec!(validator_controllers(), vec![5, 11]);

		// 2 decides to be a validator. Consequences:
		assert_ok!(Staking::validate(RuntimeOrigin::signed(1), ValidatorPrefs::default()));
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(1),
			SessionKeys { other: 2.into() },
			vec![]
		));
		// new stakes:
		// 11: 1000 self vote
		// 21: 1000 self vote + 250 vote
		// 5 : 1000 self vote
		// 1 : 2000 self vote + 250 vote.
		// Winners: 21 and 1

		mock::start_active_era(2);

		assert_eq_uvec!(validator_controllers(), vec![1, 21]);
	});
}

#[test]
fn wrong_vote_is_moot() {
	ExtBuilder::default()
		.add_staker(
			61,
			61,
			500,
			StakerStatus::Nominator(vec![
				11, 21, // good votes
				1, 2, 15, 1000, 25, // crap votes. No effect.
			]),
		)
		.build_and_execute(|| {
			// the genesis validators already reflect the above vote, nonetheless start a new era.
			mock::start_active_era(1);

			// new validators
			assert_eq_uvec!(validator_controllers(), vec![21, 11]);

			// our new voter is taken into account
			assert!(Staking::eras_stakers(active_era(), &11).others.iter().any(|i| i.who == 61));
			assert!(Staking::eras_stakers(active_era(), &21).others.iter().any(|i| i.who == 61));
		});
}

#[test]
fn bond_with_no_staked_value() {
	// Behavior when someone bonds with no staked value.
	// Particularly when they votes and the candidate is elected.
	ExtBuilder::default()
		.validator_count(3)
		.existential_deposit(5)
		.balance_factor(5)
		.nominate(false)
		.minimum_validator_count(1)
		.build_and_execute(|| {
			// Can't bond with 1
			assert_noop!(
				Staking::bond(RuntimeOrigin::signed(1), 1, RewardDestination::Account(1)),
				Error::<Test>::InsufficientBond,
			);
			// bonded with absolute minimum value possible.
			assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 5, RewardDestination::Account(1)));
			assert_eq!(pallet_balances::Holds::<Test>::get(&1)[0].amount, 5);

			// unbonding even 1 will cause all to be unbonded.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(1), 1));
			assert_eq!(
				Staking::ledger(1.into()).unwrap(),
				StakingLedgerInspect {
					stash: 1,
					active: 0,
					total: 5,
					unlocking: bounded_vec![UnlockChunk { value: 5, era: 3 }],
					legacy_claimed_rewards: bounded_vec![],
				}
			);

			mock::start_active_era(1);
			mock::start_active_era(2);

			// not yet removed.
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(1), 0));
			assert!(Staking::ledger(1.into()).is_ok());
			assert_eq!(pallet_balances::Holds::<Test>::get(&1)[0].amount, 5);

			mock::start_active_era(3);

			// poof. Account 1 is removed from the staking system.
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(1), 0));
			assert!(Staking::ledger(1.into()).is_err());
			assert_eq!(pallet_balances::Holds::<Test>::get(&1).len(), 0);
		});
}

#[test]
fn bond_with_little_staked_value_bounded() {
	ExtBuilder::default()
		.validator_count(3)
		.nominate(false)
		.minimum_validator_count(1)
		.build_and_execute(|| {
			// setup
			assert_ok!(Staking::chill(RuntimeOrigin::signed(31)));
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));
			let init_balance_1 = asset::stakeable_balance::<Test>(&1);
			let init_balance_11 = asset::stakeable_balance::<Test>(&11);

			// Stingy validator.
			assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 1, RewardDestination::Account(1)));
			assert_ok!(Staking::validate(RuntimeOrigin::signed(1), ValidatorPrefs::default()));
			assert_ok!(Session::set_keys(
				RuntimeOrigin::signed(1),
				SessionKeys { other: 1.into() },
				vec![]
			));

			// 1 era worth of reward. BUT, we set the timestamp after on_initialize, so outdated by
			// one block.
			let total_payout_0 = current_total_payout_for_duration(reward_time_per_era());

			reward_all_elected();
			mock::start_active_era(1);
			mock::make_all_reward_payment(0);

			// 1 is elected.
			assert_eq_uvec!(validator_controllers(), vec![21, 11, 1]);
			assert_eq!(Staking::eras_stakers(active_era(), &2).total, 0);

			// Old ones are rewarded.
			assert_eq_error_rate!(
				asset::stakeable_balance::<Test>(&11),
				init_balance_11 + total_payout_0 / 3,
				1
			);
			// no rewards paid to 2. This was initial election.
			assert_eq!(asset::stakeable_balance::<Test>(&1), init_balance_1);

			// reward era 2
			let total_payout_1 = current_total_payout_for_duration(reward_time_per_era());
			reward_all_elected();
			mock::start_active_era(2);
			mock::make_all_reward_payment(1);

			assert_eq_uvec!(validator_controllers(), vec![21, 11, 1]);
			assert_eq!(Staking::eras_stakers(active_era(), &2).total, 0);

			// 2 is now rewarded.
			assert_eq_error_rate!(
				asset::stakeable_balance::<Test>(&1),
				init_balance_1 + total_payout_1 / 3,
				1
			);
			assert_eq_error_rate!(
				asset::stakeable_balance::<Test>(&11),
				init_balance_11 + total_payout_0 / 3 + total_payout_1 / 3,
				2,
			);
		});
}

#[test]
fn bond_with_duplicate_vote_should_be_ignored_by_election_provider() {
	ExtBuilder::default()
		.validator_count(2)
		.nominate(false)
		.minimum_validator_count(1)
		.set_stake(31, 1000)
		.build_and_execute(|| {
			// ensure all have equal stake.
			assert_eq!(
				<Validators<Test>>::iter()
					.map(|(v, _)| (v, Staking::ledger(v.into()).unwrap().total))
					.collect::<Vec<_>>(),
				vec![(31, 1000), (21, 1000), (11, 1000)],
			);
			// no nominators shall exist.
			assert!(<Nominators<Test>>::iter().map(|(n, _)| n).collect::<Vec<_>>().is_empty());

			// give the man some money.
			let initial_balance = 1000;
			for i in [1, 2, 3, 4].iter() {
				let _ = asset::set_stakeable_balance::<Test>(&i, initial_balance);
			}

			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(1),
				1000,
				RewardDestination::Account(1)
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(1), vec![11, 11, 11, 21, 31]));

			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(3),
				1000,
				RewardDestination::Account(3)
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![21, 31]));

			// winners should be 21 and 31. Otherwise this election is taking duplicates into
			// account.
			let supports = <Test as Config>::ElectionProvider::elect().unwrap();
			assert_eq!(
				supports,
				vec![
					(21, Support { total: 1800, voters: vec![(21, 1000), (1, 400), (3, 400)] }),
					(31, Support { total: 2200, voters: vec![(31, 1000), (1, 600), (3, 600)] })
				],
			);
		});
}

#[test]
fn bond_with_duplicate_vote_should_be_ignored_by_election_provider_elected() {
	// same as above but ensures that even when the dupe is being elected, everything is sane.
	ExtBuilder::default()
		.validator_count(2)
		.nominate(false)
		.set_stake(31, 1000)
		.minimum_validator_count(1)
		.build_and_execute(|| {
			// ensure all have equal stake.
			assert_eq!(
				<Validators<Test>>::iter()
					.map(|(v, _)| (v, Staking::ledger(v.into()).unwrap().total))
					.collect::<Vec<_>>(),
				vec![(31, 1000), (21, 1000), (11, 1000)],
			);

			// no nominators shall exist.
			assert!(<Nominators<Test>>::iter().collect::<Vec<_>>().is_empty());

			// give the man some money.
			let initial_balance = 1000;
			for i in [1, 2, 3, 4].iter() {
				let _ = asset::set_stakeable_balance::<Test>(&i, initial_balance);
			}

			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(1),
				1000,
				RewardDestination::Account(1)
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(1), vec![11, 11, 11, 21]));

			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(3),
				1000,
				RewardDestination::Account(3)
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![21]));

			// winners should be 21 and 11.
			let supports = <Test as Config>::ElectionProvider::elect().unwrap();
			assert_eq!(
				supports,
				vec![
					(11, Support { total: 1500, voters: vec![(11, 1000), (1, 500)] }),
					(21, Support { total: 2500, voters: vec![(21, 1000), (1, 500), (3, 1000)] })
				],
			);
		});
}

#[test]
fn new_era_elects_correct_number_of_validators() {
	ExtBuilder::default().nominate(true).validator_count(1).build_and_execute(|| {
		assert_eq!(ValidatorCount::<Test>::get(), 1);
		assert_eq!(validator_controllers().len(), 1);

		Session::on_initialize(System::block_number());

		assert_eq!(validator_controllers().len(), 1);
	})
}

#[test]
fn phragmen_should_not_overflow() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// This is the maximum value that we can have as the outcome of CurrencyToVote.
		type Votes = u64;

		let _ = Staking::chill(RuntimeOrigin::signed(10));
		let _ = Staking::chill(RuntimeOrigin::signed(20));

		bond_validator(3, Votes::max_value() as Balance);
		bond_validator(5, Votes::max_value() as Balance);

		bond_nominator(7, Votes::max_value() as Balance, vec![3, 5]);
		bond_nominator(9, Votes::max_value() as Balance, vec![3, 5]);

		mock::start_active_era(1);

		assert_eq_uvec!(validator_controllers(), vec![3, 5]);

		// We can safely convert back to values within [u64, u128].
		assert!(Staking::eras_stakers(active_era(), &3).total > Votes::max_value() as Balance);
		assert!(Staking::eras_stakers(active_era(), &5).total > Votes::max_value() as Balance);
	})
}

#[test]
fn reward_validator_slashing_validator_does_not_overflow() {
	ExtBuilder::default().build_and_execute(|| {
		let stake = u64::MAX as Balance * 2;
		let reward_slash = u64::MAX as Balance * 2;

		// Assert multiplication overflows in balance arithmetic.
		assert!(stake.checked_mul(reward_slash).is_none());

		// Set staker
		let _ = asset::set_stakeable_balance::<Test>(&11, stake);

		let exposure = Exposure::<AccountId, Balance> { total: stake, own: stake, others: vec![] };
		let reward = EraRewardPoints::<AccountId> {
			total: 1,
			individual: vec![(11, 1)].into_iter().collect(),
		};

		// Check reward
		ErasRewardPoints::<Test>::insert(0, reward);
		EraInfo::<Test>::set_exposure(0, &11, exposure);
		ErasValidatorReward::<Test>::insert(0, stake);
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 0, 0));
		assert_eq!(asset::stakeable_balance::<Test>(&11), stake * 2);

		// ensure ledger has `stake` and no more.
		Ledger::<Test>::insert(
			11,
			StakingLedgerInspect {
				stash: 11,
				total: stake,
				active: stake,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![1],
			},
		);
		// Set staker (unsafe, can reduce balance below actual stake)
		let _ = asset::set_stakeable_balance::<Test>(&11, stake);
		let _ = asset::set_stakeable_balance::<Test>(&2, stake);

		// only slashes out of bonded stake are applied. without this line, it is 0.
		Staking::bond(RuntimeOrigin::signed(2), stake - 1, RewardDestination::Staked).unwrap();
		// Override exposure of 11
		EraInfo::<Test>::set_exposure(
			0,
			&11,
			Exposure {
				total: stake,
				own: 1,
				others: vec![IndividualExposure { who: 2, value: stake - 1 }],
			},
		);

		// Check slashing
		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(100)]);

		assert_eq!(asset::stakeable_balance::<Test>(&11), stake - 1);
		assert_eq!(asset::stakeable_balance::<Test>(&2), 1);
	})
}

#[test]
fn reward_from_authorship_event_handler_works() {
	ExtBuilder::default().build_and_execute(|| {
		use pallet_authorship::EventHandler;

		assert_eq!(<pallet_authorship::Pallet<Test>>::author(), Some(11));

		Pallet::<Test>::note_author(11);
		Pallet::<Test>::note_author(11);

		// Not mandatory but must be coherent with rewards
		assert_eq_uvec!(Session::validators(), vec![11, 21]);

		// 21 is rewarded as an uncle producer
		// 11 is rewarded as a block producer and uncle referencer and uncle producer
		assert_eq!(
			ErasRewardPoints::<Test>::get(active_era()),
			EraRewardPoints { individual: vec![(11, 20 * 2)].into_iter().collect(), total: 40 },
		);
	})
}

#[test]
fn add_reward_points_fns_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Not mandatory but must be coherent with rewards
		assert_eq_uvec!(Session::validators(), vec![21, 11]);

		Pallet::<Test>::reward_by_ids(vec![(21, 1), (11, 1), (11, 1)]);

		Pallet::<Test>::reward_by_ids(vec![(21, 1), (11, 1), (11, 1)]);

		assert_eq!(
			ErasRewardPoints::<Test>::get(active_era()),
			EraRewardPoints { individual: vec![(11, 4), (21, 2)].into_iter().collect(), total: 6 },
		);
	})
}

#[test]
fn unbonded_balance_is_not_slashable() {
	ExtBuilder::default().build_and_execute(|| {
		// total amount staked is slashable.
		assert_eq!(Staking::slashable_balance_of(&11), 1000);

		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 800));

		// only the active portion.
		assert_eq!(Staking::slashable_balance_of(&11), 200);
	})
}

#[test]
fn era_is_always_same_length() {
	// This ensures that the sessions is always of the same length if there is no forcing no
	// session changes.
	ExtBuilder::default().build_and_execute(|| {
		let session_per_era = <SessionsPerEra as Get<SessionIndex>>::get();

		mock::start_active_era(1);
		assert_eq!(ErasStartSessionIndex::<Test>::get(current_era()).unwrap(), session_per_era);

		mock::start_active_era(2);
		assert_eq!(
			ErasStartSessionIndex::<Test>::get(current_era()).unwrap(),
			session_per_era * 2u32
		);

		let session = Session::current_index();
		Staking::set_force_era(Forcing::ForceNew);
		advance_session();
		advance_session();
		assert_eq!(current_era(), 3);
		assert_eq!(ErasStartSessionIndex::<Test>::get(current_era()).unwrap(), session + 2);

		mock::start_active_era(4);
		assert_eq!(
			ErasStartSessionIndex::<Test>::get(current_era()).unwrap(),
			session + 2u32 + session_per_era
		);
	});
}

#[test]
fn offence_doesnt_force_new_era() {
	ExtBuilder::default().build_and_execute(|| {
		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(5)]);

		assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
	});
}

#[test]
fn offence_ensures_new_era_without_clobbering() {
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Staking::force_new_era_always(RuntimeOrigin::root()));
		assert_eq!(ForceEra::<Test>::get(), Forcing::ForceAlways);

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(5)]);

		assert_eq!(ForceEra::<Test>::get(), Forcing::ForceAlways);
	});
}

#[test]
fn slashing_performed_according_exposure() {
	// This test checks that slashing is performed according the exposure (or more precisely,
	// historical exposure), not the current balance.
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(Staking::eras_stakers(active_era(), &11).own, 1000);

		// Handle an offence with a historical exposure.
		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(50)]);

		// The stash account should be slashed for 500 (50% of 1000).
		assert_eq!(asset::stakeable_balance::<Test>(&11), 500);
	});
}

#[test]
fn reporters_receive_their_slice() {
	// This test verifies that the reporters of the offence receive their slice from the slashed
	// amount.
	ExtBuilder::default().build_and_execute(|| {
		// The reporters' reward is calculated from the total exposure.
		let initial_balance = 1125;

		assert_eq!(Staking::eras_stakers(active_era(), &11).total, initial_balance);

		on_offence_now(&[offence_from(11, Some(vec![1, 2]))], &[Perbill::from_percent(50)]);

		// F1 * (reward_proportion * slash - 0)
		// 50% * (10% * initial_balance / 2)
		let reward = (initial_balance / 20) / 2;
		let reward_each = reward / 2; // split into two pieces.
		assert_eq!(asset::total_balance::<Test>(&1), 10 + reward_each);
		assert_eq!(asset::total_balance::<Test>(&2), 20 + reward_each);
	});
}

#[test]
fn subsequent_reports_in_same_span_pay_out_less() {
	// This test verifies that the reporters of the offence receive their slice from the slashed
	// amount, but less and less if they submit multiple reports in one span.
	ExtBuilder::default().build_and_execute(|| {
		// The reporters' reward is calculated from the total exposure.
		let initial_balance = 1125;

		assert_eq!(Staking::eras_stakers(active_era(), &11).total, initial_balance);

		on_offence_now(&[offence_from(11, Some(vec![1]))], &[Perbill::from_percent(20)]);

		// F1 * (reward_proportion * slash - 0)
		// 50% * (10% * initial_balance * 20%)
		let reward = (initial_balance / 5) / 20;
		assert_eq!(asset::total_balance::<Test>(&1), 10 + reward);

		on_offence_now(&[offence_from(11, Some(vec![1]))], &[Perbill::from_percent(50)]);

		let prior_payout = reward;

		// F1 * (reward_proportion * slash - prior_payout)
		// 50% * (10% * (initial_balance / 2) - prior_payout)
		let reward = ((initial_balance / 20) - prior_payout) / 2;
		assert_eq!(asset::total_balance::<Test>(&1), 10 + prior_payout + reward);
	});
}

#[test]
fn invulnerables_are_not_slashed() {
	// For invulnerable validators no slashing is performed.
	ExtBuilder::default().invulnerables(vec![11]).build_and_execute(|| {
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&21), 2000);

		let exposure = Staking::eras_stakers(active_era(), &21);
		let initial_balance = Staking::slashable_balance_of(&21);

		let nominator_balances: Vec<_> = exposure
			.others
			.iter()
			.map(|o| asset::stakeable_balance::<Test>(&o.who))
			.collect();

		on_offence_now(
			&[offence_from(11, None), offence_from(21, None)],
			&[Perbill::from_percent(50), Perbill::from_percent(20)],
		);

		// The validator 11 hasn't been slashed, but 21 has been.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		// 2000 - (0.2 * initial_balance)
		assert_eq!(asset::stakeable_balance::<Test>(&21), 2000 - (2 * initial_balance / 10));

		// ensure that nominators were slashed as well.
		for (initial_balance, other) in nominator_balances.into_iter().zip(exposure.others) {
			assert_eq!(
				asset::stakeable_balance::<Test>(&other.who),
				initial_balance - (2 * other.value / 10),
			);
		}
	});
}

#[test]
fn dont_slash_if_fraction_is_zero() {
	// Don't slash if the fraction is zero.
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(0)]);

		// The validator hasn't been slashed. The new era is not forced.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
	});
}

#[test]
fn only_slash_for_max_in_era() {
	// multiple slashes within one era are only applied if it is more than any previous slash in the
	// same era.
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(50)]);

		// The validator has been slashed and has been force-chilled.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 500);
		assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(25)]);

		// The validator has not been slashed additionally.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 500);

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(60)]);

		// The validator got slashed 10% more.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 400);
	})
}

#[test]
fn garbage_collection_after_slashing() {
	// ensures that `SlashingSpans` and `SpanSlash` of an account is removed after reaping.
	ExtBuilder::default()
		.existential_deposit(2)
		.balance_factor(2)
		.build_and_execute(|| {
			assert_eq!(asset::stakeable_balance::<Test>(&11), 2000);

			on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)]);

			assert_eq!(asset::stakeable_balance::<Test>(&11), 2000 - 200);
			assert!(SlashingSpans::<Test>::get(&11).is_some());
			assert_eq!(SpanSlash::<Test>::get(&(11, 0)).amount(), &200);

			on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(100)]);

			// validator and nominator slash in era are garbage-collected by era change,
			// so we don't test those here.

			assert_eq!(asset::stakeable_balance::<Test>(&11), 0);
			// Non staked balance is not touched.
			assert_eq!(asset::total_balance::<Test>(&11), ExistentialDeposit::get());

			let slashing_spans = SlashingSpans::<Test>::get(&11).unwrap();
			assert_eq!(slashing_spans.iter().count(), 2);

			// reap_stash respects num_slashing_spans so that weight is accurate
			assert_noop!(
				Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
				Error::<Test>::IncorrectSlashingSpans
			);
			assert_ok!(Staking::reap_stash(RuntimeOrigin::signed(20), 11, 2));

			assert!(SlashingSpans::<Test>::get(&11).is_none());
			assert_eq!(SpanSlash::<Test>::get(&(11, 0)).amount(), &0);
		})
}

#[test]
fn garbage_collection_on_window_pruning() {
	// ensures that `ValidatorSlashInEra` and `NominatorSlashInEra` are cleared after
	// `BondingDuration`.
	ExtBuilder::default().build_and_execute(|| {
		mock::start_active_era(1);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		let now = active_era();

		let exposure = Staking::eras_stakers(now, &11);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);
		let nominated_value = exposure.others.iter().find(|o| o.who == 101).unwrap().value;

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)]);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 900);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - (nominated_value / 10));

		assert!(ValidatorSlashInEra::<Test>::get(&now, &11).is_some());
		assert!(NominatorSlashInEra::<Test>::get(&now, &101).is_some());

		// + 1 because we have to exit the bonding window.
		for era in (0..(BondingDuration::get() + 1)).map(|offset| offset + now + 1) {
			assert!(ValidatorSlashInEra::<Test>::get(&now, &11).is_some());
			assert!(NominatorSlashInEra::<Test>::get(&now, &101).is_some());

			mock::start_active_era(era);
		}

		assert!(ValidatorSlashInEra::<Test>::get(&now, &11).is_none());
		assert!(NominatorSlashInEra::<Test>::get(&now, &101).is_none());
	})
}

#[test]
fn slashing_nominators_by_span_max() {
	ExtBuilder::default().build_and_execute(|| {
		mock::start_active_era(1);
		mock::start_active_era(2);
		mock::start_active_era(3);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&21), 2000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);
		assert_eq!(Staking::slashable_balance_of(&21), 1000);

		let exposure_11 = Staking::eras_stakers(active_era(), &11);
		let exposure_21 = Staking::eras_stakers(active_era(), &21);
		let nominated_value_11 = exposure_11.others.iter().find(|o| o.who == 101).unwrap().value;
		let nominated_value_21 = exposure_21.others.iter().find(|o| o.who == 101).unwrap().value;

		on_offence_in_era(&[offence_from(11, None)], &[Perbill::from_percent(10)], 2);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 900);

		let slash_1_amount = Perbill::from_percent(10) * nominated_value_11;
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - slash_1_amount);

		let expected_spans = vec![
			slashing::SlashingSpan { index: 1, start: 4, length: None },
			slashing::SlashingSpan { index: 0, start: 0, length: Some(4) },
		];

		let get_span = |account| SlashingSpans::<Test>::get(&account).unwrap();

		assert_eq!(get_span(11).iter().collect::<Vec<_>>(), expected_spans);

		assert_eq!(get_span(101).iter().collect::<Vec<_>>(), expected_spans);

		// second slash: higher era, higher value, same span.
		on_offence_in_era(&[offence_from(21, None)], &[Perbill::from_percent(30)], 3);

		// 11 was not further slashed, but 21 and 101 were.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 900);
		assert_eq!(asset::stakeable_balance::<Test>(&21), 1700);

		let slash_2_amount = Perbill::from_percent(30) * nominated_value_21;
		assert!(slash_2_amount > slash_1_amount);

		// only the maximum slash in a single span is taken.
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - slash_2_amount);

		// third slash: in same era and on same validator as first, higher
		// in-era value, but lower slash value than slash 2.
		on_offence_in_era(&[offence_from(11, None)], &[Perbill::from_percent(20)], 2);

		// 11 was further slashed, but 21 and 101 were not.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 800);
		assert_eq!(asset::stakeable_balance::<Test>(&21), 1700);

		let slash_3_amount = Perbill::from_percent(20) * nominated_value_21;
		assert!(slash_3_amount < slash_2_amount);
		assert!(slash_3_amount > slash_1_amount);

		// only the maximum slash in a single span is taken.
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - slash_2_amount);
	});
}

#[test]
fn slashes_are_summed_across_spans() {
	ExtBuilder::default().build_and_execute(|| {
		mock::start_active_era(1);
		mock::start_active_era(2);
		mock::start_active_era(3);

		assert_eq!(asset::stakeable_balance::<Test>(&21), 2000);
		assert_eq!(Staking::slashable_balance_of(&21), 1000);

		let get_span = |account| SlashingSpans::<Test>::get(&account).unwrap();

		on_offence_now(&[offence_from(21, None)], &[Perbill::from_percent(10)]);

		let expected_spans = vec![
			slashing::SlashingSpan { index: 1, start: 4, length: None },
			slashing::SlashingSpan { index: 0, start: 0, length: Some(4) },
		];

		assert_eq!(get_span(21).iter().collect::<Vec<_>>(), expected_spans);
		assert_eq!(asset::stakeable_balance::<Test>(&21), 1900);

		// 21 has been force-chilled. re-signal intent to validate.
		Staking::validate(RuntimeOrigin::signed(21), Default::default()).unwrap();

		mock::start_active_era(4);

		assert_eq!(Staking::slashable_balance_of(&21), 900);

		on_offence_now(&[offence_from(21, None)], &[Perbill::from_percent(10)]);

		let expected_spans = vec![
			slashing::SlashingSpan { index: 2, start: 5, length: None },
			slashing::SlashingSpan { index: 1, start: 4, length: Some(1) },
			slashing::SlashingSpan { index: 0, start: 0, length: Some(4) },
		];

		assert_eq!(get_span(21).iter().collect::<Vec<_>>(), expected_spans);
		assert_eq!(asset::stakeable_balance::<Test>(&21), 1810);
	});
}

#[test]
fn deferred_slashes_are_deferred() {
	ExtBuilder::default().slash_defer_duration(2).build_and_execute(|| {
		mock::start_active_era(1);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);

		let exposure = Staking::eras_stakers(active_era(), &11);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);
		let nominated_value = exposure.others.iter().find(|o| o.who == 101).unwrap().value;

		System::reset_events();

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)]);

		// nominations are not removed regardless of the deferring.
		assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		mock::start_active_era(2);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		mock::start_active_era(3);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		// at the start of era 4, slashes from era 1 are processed,
		// after being deferred for at least 2 full eras.
		mock::start_active_era(4);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 900);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - (nominated_value / 10));

		assert!(matches!(
			staking_events_since_last_call().as_slice(),
			&[
				Event::SlashReported { validator: 11, slash_era: 1, .. },
				Event::StakersElected,
				..,
				Event::Slashed { staker: 11, amount: 100 },
				Event::Slashed { staker: 101, amount: 12 }
			]
		));
	})
}

#[test]
fn retroactive_deferred_slashes_two_eras_before() {
	ExtBuilder::default().slash_defer_duration(2).build_and_execute(|| {
		assert_eq!(BondingDuration::get(), 3);

		mock::start_active_era(3);

		assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

		System::reset_events();
		on_offence_in_era(
			&[offence_from(11, None)],
			&[Perbill::from_percent(10)],
			1, // should be deferred for two full eras, and applied at the beginning of era 4.
		);

		mock::start_active_era(4);

		assert!(matches!(
			staking_events_since_last_call().as_slice(),
			&[
				Event::SlashReported { validator: 11, slash_era: 1, .. },
				..,
				Event::Slashed { staker: 11, amount: 100 },
				Event::Slashed { staker: 101, amount: 12 }
			]
		));
	})
}

#[test]
fn retroactive_deferred_slashes_one_before() {
	ExtBuilder::default().slash_defer_duration(2).build_and_execute(|| {
		assert_eq!(BondingDuration::get(), 3);

		// unbond at slash era.
		mock::start_active_era(2);
		assert_ok!(Staking::chill(RuntimeOrigin::signed(11)));
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 100));

		mock::start_active_era(3);
		System::reset_events();
		on_offence_in_era(
			&[offence_from(11, None)],
			&[Perbill::from_percent(10)],
			2, // should be deferred for two full eras, and applied at the beginning of era 5.
		);

		mock::start_active_era(4);

		assert_eq!(Staking::ledger(11.into()).unwrap().total, 1000);
		// slash happens after the next line.

		mock::start_active_era(5);
		assert!(matches!(
			staking_events_since_last_call().as_slice(),
			&[
				Event::SlashReported { validator: 11, slash_era: 2, .. },
				..,
				Event::Slashed { staker: 11, amount: 100 },
				Event::Slashed { staker: 101, amount: 12 }
			]
		));

		// their ledger has already been slashed.
		assert_eq!(Staking::ledger(11.into()).unwrap().total, 900);
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1000));
		assert_eq!(Staking::ledger(11.into()).unwrap().total, 900);
	})
}

#[test]
fn staker_cannot_bail_deferred_slash() {
	// as long as SlashDeferDuration is less than BondingDuration, this should not be possible.
	ExtBuilder::default().slash_defer_duration(2).build_and_execute(|| {
		mock::start_active_era(1);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		let exposure = Staking::eras_stakers(active_era(), &11);
		let nominated_value = exposure.others.iter().find(|o| o.who == 101).unwrap().value;

		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)]);

		// now we chill
		assert_ok!(Staking::chill(RuntimeOrigin::signed(101)));
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(101), 500));

		assert_eq!(CurrentEra::<Test>::get().unwrap(), 1);
		assert_eq!(active_era(), 1);

		assert_eq!(
			Ledger::<Test>::get(101).unwrap(),
			StakingLedgerInspect {
				active: 0,
				total: 500,
				stash: 101,
				legacy_claimed_rewards: bounded_vec![],
				unlocking: bounded_vec![UnlockChunk { era: 4u32, value: 500 }],
			}
		);

		// no slash yet.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		// no slash yet.
		mock::start_active_era(2);
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);
		assert_eq!(CurrentEra::<Test>::get().unwrap(), 2);
		assert_eq!(active_era(), 2);

		// no slash yet.
		mock::start_active_era(3);
		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);
		assert_eq!(CurrentEra::<Test>::get().unwrap(), 3);
		assert_eq!(active_era(), 3);

		// and cannot yet unbond:
		assert_storage_noop!(assert!(
			Staking::withdraw_unbonded(RuntimeOrigin::signed(101), 0).is_ok()
		));
		assert_eq!(
			Ledger::<Test>::get(101).unwrap().unlocking.into_inner(),
			vec![UnlockChunk { era: 4u32, value: 500 as Balance }],
		);

		// at the start of era 4, slashes from era 1 are processed,
		// after being deferred for at least 2 full eras.
		mock::start_active_era(4);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 900);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - (nominated_value / 10));

		// and the leftover of the funds can now be unbonded.
	})
}

#[test]
fn remove_deferred() {
	ExtBuilder::default().slash_defer_duration(2).build_and_execute(|| {
		mock::start_active_era(1);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);

		let exposure = Staking::eras_stakers(active_era(), &11);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);
		let nominated_value = exposure.others.iter().find(|o| o.who == 101).unwrap().value;

		// deferred to start of era 4.
		on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)]);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		mock::start_active_era(2);

		// reported later, but deferred to start of era 4 as well.
		System::reset_events();
		on_offence_in_era(&[offence_from(11, None)], &[Perbill::from_percent(15)], 1);

		// fails if empty
		assert_noop!(
			Staking::cancel_deferred_slash(RuntimeOrigin::root(), 1, vec![]),
			Error::<Test>::EmptyTargets
		);

		// cancel one of them.
		assert_ok!(Staking::cancel_deferred_slash(RuntimeOrigin::root(), 4, vec![0]));

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		mock::start_active_era(3);

		assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

		// at the start of era 4, slashes from era 1 are processed,
		// after being deferred for at least 2 full eras.
		mock::start_active_era(4);

		// the first slash for 10% was cancelled, but the 15% one not.
		assert!(matches!(
			staking_events_since_last_call().as_slice(),
			&[
				Event::SlashReported { validator: 11, slash_era: 1, .. },
				..,
				Event::Slashed { staker: 11, amount: 50 },
				Event::Slashed { staker: 101, amount: 7 }
			]
		));

		let slash_10 = Perbill::from_percent(10);
		let slash_15 = Perbill::from_percent(15);
		let initial_slash = slash_10 * nominated_value;

		let total_slash = slash_15 * nominated_value;
		let actual_slash = total_slash - initial_slash;

		// 5% slash (15 - 10) processed now.
		assert_eq!(asset::stakeable_balance::<Test>(&11), 950);
		assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - actual_slash);
	})
}

#[test]
fn remove_multi_deferred() {
	ExtBuilder::default()
		.slash_defer_duration(2)
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.set_status(51, StakerStatus::Validator)
		.build_and_execute(|| {
			mock::start_active_era(1);

			assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
			assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

			on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)]);

			on_offence_now(&[offence_from(21, None)], &[Perbill::from_percent(10)]);

			on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(25)]);

			on_offence_now(&[offence_from(41, None)], &[Perbill::from_percent(25)]);

			on_offence_now(&[offence_from(51, None)], &[Perbill::from_percent(25)]);

			assert_eq!(UnappliedSlashes::<Test>::get(&4).len(), 5);

			// fails if list is not sorted
			assert_noop!(
				Staking::cancel_deferred_slash(RuntimeOrigin::root(), 1, vec![2, 0, 4]),
				Error::<Test>::NotSortedAndUnique
			);
			// fails if list is not unique
			assert_noop!(
				Staking::cancel_deferred_slash(RuntimeOrigin::root(), 1, vec![0, 2, 2]),
				Error::<Test>::NotSortedAndUnique
			);
			// fails if bad index
			assert_noop!(
				Staking::cancel_deferred_slash(RuntimeOrigin::root(), 1, vec![1, 2, 3, 4, 5]),
				Error::<Test>::InvalidSlashIndex
			);

			assert_ok!(Staking::cancel_deferred_slash(RuntimeOrigin::root(), 4, vec![0, 2, 4]));

			let slashes = UnappliedSlashes::<Test>::get(&4);
			assert_eq!(slashes.len(), 2);
			assert_eq!(slashes[0].validator, 21);
			assert_eq!(slashes[1].validator, 41);
		})
}

#[test]
fn claim_reward_at_the_last_era_and_no_double_claim_and_invalid_claim() {
	// should check that:
	// * rewards get paid until history_depth for both validators and nominators
	// * an invalid era to claim doesn't update last_reward
	// * double claim of one era fails
	ExtBuilder::default().nominate(true).build_and_execute(|| {
		// Consumed weight for all payout_stakers dispatches that fail
		let err_weight = <Test as Config>::WeightInfo::payout_stakers_alive_staked(0);

		let init_balance_11 = asset::total_balance::<Test>(&11);
		let init_balance_101 = asset::total_balance::<Test>(&101);

		let part_for_11 = Perbill::from_rational::<u32>(1000, 1125);
		let part_for_101 = Perbill::from_rational::<u32>(125, 1125);

		// Check state
		Payee::<Test>::insert(11, RewardDestination::Account(11));
		Payee::<Test>::insert(101, RewardDestination::Account(101));

		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_0 = current_total_payout_for_duration(reward_time_per_era());

		mock::start_active_era(1);

		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
		// Increase total token issuance to affect the total payout.
		let _ = Balances::deposit_creating(&999, 1_000_000_000);

		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_1 = current_total_payout_for_duration(reward_time_per_era());
		assert!(total_payout_1 != total_payout_0);

		mock::start_active_era(2);

		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
		// Increase total token issuance to affect the total payout.
		let _ = Balances::deposit_creating(&999, 1_000_000_000);
		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_2 = current_total_payout_for_duration(reward_time_per_era());
		assert!(total_payout_2 != total_payout_0);
		assert!(total_payout_2 != total_payout_1);

		mock::start_active_era(HistoryDepth::get() + 1);

		let active_era = active_era();

		// This is the latest planned era in staking, not the active era
		let current_era = CurrentEra::<Test>::get().unwrap();

		// Last kept is 1:
		assert!(current_era - HistoryDepth::get() == 1);
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 0, 0),
			// Fail: Era out of history
			Error::<Test>::InvalidEraToReward.with_weight(err_weight)
		);
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 0));
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0));
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0),
			// Fail: Double claim
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, active_era, 0),
			// Fail: Era not finished yet
			Error::<Test>::InvalidEraToReward.with_weight(err_weight)
		);

		// Era 0 can't be rewarded anymore and current era can't be rewarded yet
		// only era 1 and 2 can be rewarded.

		assert_eq!(
			asset::total_balance::<Test>(&11),
			init_balance_11 + part_for_11 * (total_payout_1 + total_payout_2),
		);
		assert_eq!(
			asset::total_balance::<Test>(&101),
			init_balance_101 + part_for_101 * (total_payout_1 + total_payout_2),
		);
	});
}

#[test]
fn zero_slash_keeps_nominators() {
	ExtBuilder::default()
		.validator_count(7)
		.set_status(41, StakerStatus::Validator)
		.set_status(51, StakerStatus::Validator)
		.set_status(201, StakerStatus::Validator)
		.set_status(202, StakerStatus::Validator)
		.build_and_execute(|| {
			mock::start_active_era(1);

			assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
			assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

			on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(0)]);

			assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
			assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);

			// 11 is not removed
			assert!(Validators::<Test>::iter().any(|(stash, _)| stash == 11));
			// and their nominations are kept.
			assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);
		});
}

#[test]
fn six_session_delay() {
	ExtBuilder::default().initialize_first_session(false).build_and_execute(|| {
		use pallet_session::SessionManager;

		let val_set = Session::validators();
		let init_session = Session::current_index();
		let init_active_era = active_era();

		// pallet-session is delaying session by one, thus the next session to plan is +2.
		assert_eq!(<Staking as SessionManager<_>>::new_session(init_session + 2), None);
		assert_eq!(
			<Staking as SessionManager<_>>::new_session(init_session + 3),
			Some(val_set.clone())
		);
		assert_eq!(<Staking as SessionManager<_>>::new_session(init_session + 4), None);
		assert_eq!(<Staking as SessionManager<_>>::new_session(init_session + 5), None);
		assert_eq!(
			<Staking as SessionManager<_>>::new_session(init_session + 6),
			Some(val_set.clone())
		);

		<Staking as SessionManager<_>>::end_session(init_session);
		<Staking as SessionManager<_>>::start_session(init_session + 1);
		assert_eq!(active_era(), init_active_era);

		<Staking as SessionManager<_>>::end_session(init_session + 1);
		<Staking as SessionManager<_>>::start_session(init_session + 2);
		assert_eq!(active_era(), init_active_era);

		// Reward current era
		Staking::reward_by_ids(vec![(11, 1)]);

		// New active era is triggered here.
		<Staking as SessionManager<_>>::end_session(init_session + 2);
		<Staking as SessionManager<_>>::start_session(init_session + 3);
		assert_eq!(active_era(), init_active_era + 1);

		<Staking as SessionManager<_>>::end_session(init_session + 3);
		<Staking as SessionManager<_>>::start_session(init_session + 4);
		assert_eq!(active_era(), init_active_era + 1);

		<Staking as SessionManager<_>>::end_session(init_session + 4);
		<Staking as SessionManager<_>>::start_session(init_session + 5);
		assert_eq!(active_era(), init_active_era + 1);

		// Reward current era
		Staking::reward_by_ids(vec![(21, 2)]);

		// New active era is triggered here.
		<Staking as SessionManager<_>>::end_session(init_session + 5);
		<Staking as SessionManager<_>>::start_session(init_session + 6);
		assert_eq!(active_era(), init_active_era + 2);

		// That reward are correct
		assert_eq!(ErasRewardPoints::<Test>::get(init_active_era).total, 1);
		assert_eq!(ErasRewardPoints::<Test>::get(init_active_era + 1).total, 2);
	});
}

#[test]
fn test_nominators_over_max_exposure_page_size_are_rewarded() {
	ExtBuilder::default().build_and_execute(|| {
		// bond one nominator more than the max exposure page size to validator 11.
		for i in 0..=MaxExposurePageSize::get() {
			let stash = 10_000 + i as AccountId;
			let balance = 10_000 + i as Balance;
			asset::set_stakeable_balance::<Test>(&stash, balance);
			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(stash),
				balance,
				RewardDestination::Stash
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(stash), vec![11]));
		}
		mock::start_active_era(1);

		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
		// compute and ensure the reward amount is greater than zero.
		let _ = current_total_payout_for_duration(reward_time_per_era());

		mock::start_active_era(2);
		mock::make_all_reward_payment(1);

		// Assert nominators from 1 to Max are rewarded
		let mut i: u32 = 0;
		while i < MaxExposurePageSize::get() {
			let stash = 10_000 + i as AccountId;
			let balance = 10_000 + i as Balance;
			assert!(asset::stakeable_balance::<Test>(&stash) > balance);
			i += 1;
		}

		// Assert overflowing nominators from page 1 are also rewarded
		let stash = 10_000 + i as AccountId;
		assert!(asset::stakeable_balance::<Test>(&stash) > (10_000 + i) as Balance);
	});
}

#[test]
fn test_nominators_are_rewarded_for_all_exposure_page() {
	ExtBuilder::default().build_and_execute(|| {
		// 3 pages of exposure
		let nominator_count = 2 * MaxExposurePageSize::get() + 1;

		for i in 0..nominator_count {
			let stash = 10_000 + i as AccountId;
			let balance = 10_000 + i as Balance;
			asset::set_stakeable_balance::<Test>(&stash, balance);
			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(stash),
				balance,
				RewardDestination::Stash
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(stash), vec![11]));
		}
		mock::start_active_era(1);

		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
		// compute and ensure the reward amount is greater than zero.
		let _ = current_total_payout_for_duration(reward_time_per_era());

		mock::start_active_era(2);
		mock::make_all_reward_payment(1);

		assert_eq!(EraInfo::<Test>::get_page_count(1, &11), 3);

		// Assert all nominators are rewarded according to their stake
		for i in 0..nominator_count {
			// balance of the nominator after the reward payout.
			let current_balance = asset::stakeable_balance::<Test>(&((10000 + i) as AccountId));
			// balance of the nominator in the previous iteration.
			let previous_balance =
				asset::stakeable_balance::<Test>(&((10000 + i - 1) as AccountId));
			// balance before the reward.
			let original_balance = 10_000 + i as Balance;

			assert!(current_balance > original_balance);
			// since the stake of the nominator is increasing for each iteration, the final balance
			// after the reward should also be higher than the previous iteration.
			assert!(current_balance > previous_balance);
		}
	});
}

#[test]
fn test_multi_page_payout_stakers_by_page() {
	// Test that payout_stakers work in general and that it pays the correct amount of reward.
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1000;
		// Track the exposure of the validator and all nominators.
		let mut total_exposure = balance;
		// Create a validator:
		bond_validator(11, balance); // Default(64)
		assert_eq!(Validators::<Test>::count(), 1);

		// Create nominators, targeting stash of validators
		for i in 0..100 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
			// with multi page reward payout, payout exposure is same as total exposure.
			total_exposure += bond_amount;
		}

		mock::start_active_era(1);
		Staking::reward_by_ids(vec![(11, 1)]);

		// Since `MaxExposurePageSize = 64`, there are two pages of validator exposure.
		assert_eq!(EraInfo::<Test>::get_page_count(1, &11), 2);

		// compute and ensure the reward amount is greater than zero.
		let payout = current_total_payout_for_duration(reward_time_per_era());
		mock::start_active_era(2);

		// verify the exposures are calculated correctly.
		let actual_exposure_0 = EraInfo::<Test>::get_paged_exposure(1, &11, 0).unwrap();
		assert_eq!(actual_exposure_0.total(), total_exposure);
		assert_eq!(actual_exposure_0.own(), 1000);
		assert_eq!(actual_exposure_0.others().len(), 64);
		let actual_exposure_1 = EraInfo::<Test>::get_paged_exposure(1, &11, 1).unwrap();
		assert_eq!(actual_exposure_1.total(), total_exposure);
		// own stake is only included once in the first page
		assert_eq!(actual_exposure_1.own(), 0);
		assert_eq!(actual_exposure_1.others().len(), 100 - 64);

		let pre_payout_total_issuance = pallet_balances::TotalIssuance::<Test>::get();
		RewardOnUnbalanceWasCalled::set(false);
		System::reset_events();

		let controller_balance_before_p0_payout = asset::stakeable_balance::<Test>(&11);
		// Payout rewards for first exposure page
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 0));

		// verify `Rewarded` events are being executed
		assert!(matches!(
			staking_events_since_last_call().as_slice(),
			&[
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: Some(1) },
				..,
				Event::Rewarded { stash: 1063, dest: RewardDestination::Stash, amount: 111 },
				Event::Rewarded { stash: 1064, dest: RewardDestination::Stash, amount: 111 },
			]
		));

		let controller_balance_after_p0_payout = asset::stakeable_balance::<Test>(&11);

		// verify rewards have been paid out but still some left
		assert!(pallet_balances::TotalIssuance::<Test>::get() > pre_payout_total_issuance);
		assert!(pallet_balances::TotalIssuance::<Test>::get() < pre_payout_total_issuance + payout);

		// verify the validator has been rewarded
		assert!(controller_balance_after_p0_payout > controller_balance_before_p0_payout);

		// Payout the second and last page of nominators
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 1));

		// verify `Rewarded` events are being executed for the second page.
		let events = staking_events_since_last_call();
		assert!(matches!(
			events.as_slice(),
			&[
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 1, next: None },
				Event::Rewarded { stash: 1065, dest: RewardDestination::Stash, amount: 111 },
				Event::Rewarded { stash: 1066, dest: RewardDestination::Stash, amount: 111 },
				..
			]
		));
		// verify the validator was not rewarded the second time
		assert_eq!(asset::stakeable_balance::<Test>(&11), controller_balance_after_p0_payout);

		// verify all rewards have been paid out
		assert_eq_error_rate!(
			pallet_balances::TotalIssuance::<Test>::get(),
			pre_payout_total_issuance + payout,
			2
		);
		assert!(RewardOnUnbalanceWasCalled::get());

		// Top 64 nominators of validator 11 automatically paid out, including the validator
		assert!(asset::stakeable_balance::<Test>(&11) > balance);
		for i in 0..100 {
			assert!(asset::stakeable_balance::<Test>(&(1000 + i)) > balance + i as Balance);
		}

		// verify we no longer track rewards in `legacy_claimed_rewards` vec
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![]
			}
		);

		// verify rewards are tracked to prevent double claims
		let ledger = Staking::ledger(11.into());
		for page in 0..EraInfo::<Test>::get_page_count(1, &11) {
			assert_eq!(
				EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
					1,
					ledger.as_ref().unwrap(),
					&11,
					page
				),
				true
			);
		}

		for i in 3..16 {
			Staking::reward_by_ids(vec![(11, 1)]);

			// compute and ensure the reward amount is greater than zero.
			let payout = current_total_payout_for_duration(reward_time_per_era());
			let pre_payout_total_issuance = pallet_balances::TotalIssuance::<Test>::get();

			mock::start_active_era(i);
			RewardOnUnbalanceWasCalled::set(false);
			mock::make_all_reward_payment(i - 1);
			assert_eq_error_rate!(
				pallet_balances::TotalIssuance::<Test>::get(),
				pre_payout_total_issuance + payout,
				2
			);
			assert!(RewardOnUnbalanceWasCalled::get());

			// verify we track rewards for each era and page
			for page in 0..EraInfo::<Test>::get_page_count(i - 1, &11) {
				assert_eq!(
					EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
						i - 1,
						Staking::ledger(11.into()).as_ref().unwrap(),
						&11,
						page
					),
					true
				);
			}
		}

		assert_eq!(ClaimedRewards::<Test>::get(14, &11), vec![0, 1]);

		let last_era = 99;
		let history_depth = HistoryDepth::get();
		let last_reward_era = last_era - 1;
		let first_claimable_reward_era = last_era - history_depth;
		for i in 16..=last_era {
			Staking::reward_by_ids(vec![(11, 1)]);
			// compute and ensure the reward amount is greater than zero.
			let _ = current_total_payout_for_duration(reward_time_per_era());
			mock::start_active_era(i);
		}

		// verify we clean up history as we go
		for era in 0..15 {
			assert_eq!(ClaimedRewards::<Test>::get(era, &11), Vec::<sp_staking::Page>::new());
		}

		// verify only page 0 is marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era,
			0
		));
		assert_eq!(ClaimedRewards::<Test>::get(first_claimable_reward_era, &11), vec![0]);

		// verify page 0 and 1 are marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era,
			1
		));
		assert_eq!(ClaimedRewards::<Test>::get(first_claimable_reward_era, &11), vec![0, 1]);

		// verify only page 0 is marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			last_reward_era,
			0
		));
		assert_eq!(ClaimedRewards::<Test>::get(last_reward_era, &11), vec![0]);

		// verify page 0 and 1 are marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			last_reward_era,
			1
		));
		assert_eq!(ClaimedRewards::<Test>::get(last_reward_era, &11), vec![0, 1]);

		// Out of order claims works.
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 69, 0));
		assert_eq!(ClaimedRewards::<Test>::get(69, &11), vec![0]);
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 23, 1));
		assert_eq!(ClaimedRewards::<Test>::get(23, &11), vec![1]);
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 42, 0));
		assert_eq!(ClaimedRewards::<Test>::get(42, &11), vec![0]);
	});
}

#[test]
fn test_multi_page_payout_stakers_backward_compatible() {
	// Test that payout_stakers work in general and that it pays the correct amount of reward.
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1000;
		// Track the exposure of the validator and all nominators.
		let mut total_exposure = balance;
		// Create a validator:
		bond_validator(11, balance); // Default(64)
		assert_eq!(Validators::<Test>::count(), 1);

		let err_weight = <Test as Config>::WeightInfo::payout_stakers_alive_staked(0);

		// Create nominators, targeting stash of validators
		for i in 0..100 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
			// with multi page reward payout, payout exposure is same as total exposure.
			total_exposure += bond_amount;
		}

		mock::start_active_era(1);
		Staking::reward_by_ids(vec![(11, 1)]);

		// Since `MaxExposurePageSize = 64`, there are two pages of validator exposure.
		assert_eq!(EraInfo::<Test>::get_page_count(1, &11), 2);

		// compute and ensure the reward amount is greater than zero.
		let payout = current_total_payout_for_duration(reward_time_per_era());
		mock::start_active_era(2);

		// verify the exposures are calculated correctly.
		let actual_exposure_0 = EraInfo::<Test>::get_paged_exposure(1, &11, 0).unwrap();
		assert_eq!(actual_exposure_0.total(), total_exposure);
		assert_eq!(actual_exposure_0.own(), 1000);
		assert_eq!(actual_exposure_0.others().len(), 64);
		let actual_exposure_1 = EraInfo::<Test>::get_paged_exposure(1, &11, 1).unwrap();
		assert_eq!(actual_exposure_1.total(), total_exposure);
		// own stake is only included once in the first page
		assert_eq!(actual_exposure_1.own(), 0);
		assert_eq!(actual_exposure_1.others().len(), 100 - 64);

		let pre_payout_total_issuance = pallet_balances::TotalIssuance::<Test>::get();
		RewardOnUnbalanceWasCalled::set(false);

		let controller_balance_before_p0_payout = asset::stakeable_balance::<Test>(&11);
		// Payout rewards for first exposure page
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, 1));
		// page 0 is claimed
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 0),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		let controller_balance_after_p0_payout = asset::stakeable_balance::<Test>(&11);

		// verify rewards have been paid out but still some left
		assert!(pallet_balances::TotalIssuance::<Test>::get() > pre_payout_total_issuance);
		assert!(pallet_balances::TotalIssuance::<Test>::get() < pre_payout_total_issuance + payout);

		// verify the validator has been rewarded
		assert!(controller_balance_after_p0_payout > controller_balance_before_p0_payout);

		// This should payout the second and last page of nominators
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, 1));

		// cannot claim any more pages
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, 1),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		// verify the validator was not rewarded the second time
		assert_eq!(asset::stakeable_balance::<Test>(&11), controller_balance_after_p0_payout);

		// verify all rewards have been paid out
		assert_eq_error_rate!(
			pallet_balances::TotalIssuance::<Test>::get(),
			pre_payout_total_issuance + payout,
			2
		);
		assert!(RewardOnUnbalanceWasCalled::get());

		// verify all nominators of validator 11 are paid out, including the validator
		// Validator payout goes to controller.
		assert!(asset::stakeable_balance::<Test>(&11) > balance);
		for i in 0..100 {
			assert!(asset::stakeable_balance::<Test>(&(1000 + i)) > balance + i as Balance);
		}

		// verify we no longer track rewards in `legacy_claimed_rewards` vec
		let ledger = Staking::ledger(11.into());
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![]
			}
		);

		// verify rewards are tracked to prevent double claims
		for page in 0..EraInfo::<Test>::get_page_count(1, &11) {
			assert_eq!(
				EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
					1,
					ledger.as_ref().unwrap(),
					&11,
					page
				),
				true
			);
		}

		for i in 3..16 {
			Staking::reward_by_ids(vec![(11, 1)]);

			// compute and ensure the reward amount is greater than zero.
			let payout = current_total_payout_for_duration(reward_time_per_era());
			let pre_payout_total_issuance = pallet_balances::TotalIssuance::<Test>::get();

			mock::start_active_era(i);
			RewardOnUnbalanceWasCalled::set(false);
			mock::make_all_reward_payment(i - 1);
			assert_eq_error_rate!(
				pallet_balances::TotalIssuance::<Test>::get(),
				pre_payout_total_issuance + payout,
				2
			);
			assert!(RewardOnUnbalanceWasCalled::get());

			// verify we track rewards for each era and page
			for page in 0..EraInfo::<Test>::get_page_count(i - 1, &11) {
				assert_eq!(
					EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
						i - 1,
						Staking::ledger(11.into()).as_ref().unwrap(),
						&11,
						page
					),
					true
				);
			}
		}

		assert_eq!(ClaimedRewards::<Test>::get(14, &11), vec![0, 1]);

		let last_era = 99;
		let history_depth = HistoryDepth::get();
		let last_reward_era = last_era - 1;
		let first_claimable_reward_era = last_era - history_depth;
		for i in 16..=last_era {
			Staking::reward_by_ids(vec![(11, 1)]);
			// compute and ensure the reward amount is greater than zero.
			let _ = current_total_payout_for_duration(reward_time_per_era());
			mock::start_active_era(i);
		}

		// verify we clean up history as we go
		for era in 0..15 {
			assert_eq!(ClaimedRewards::<Test>::get(era, &11), Vec::<sp_staking::Page>::new());
		}

		// verify only page 0 is marked as claimed
		assert_ok!(Staking::payout_stakers(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era
		));
		assert_eq!(ClaimedRewards::<Test>::get(first_claimable_reward_era, &11), vec![0]);

		// verify page 0 and 1 are marked as claimed
		assert_ok!(Staking::payout_stakers(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era,
		));
		assert_eq!(ClaimedRewards::<Test>::get(first_claimable_reward_era, &11), vec![0, 1]);

		// change order and verify only page 1 is marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			last_reward_era,
			1
		));
		assert_eq!(ClaimedRewards::<Test>::get(last_reward_era, &11), vec![1]);

		// verify page 0 is claimed even when explicit page is not passed
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, last_reward_era,));

		assert_eq!(ClaimedRewards::<Test>::get(last_reward_era, &11), vec![1, 0]);

		// cannot claim any more pages
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, last_reward_era),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		// Create 4 nominator pages
		for i in 100..200 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
		}

		let test_era = last_era + 1;
		mock::start_active_era(test_era);

		Staking::reward_by_ids(vec![(11, 1)]);
		// compute and ensure the reward amount is greater than zero.
		let _ = current_total_payout_for_duration(reward_time_per_era());
		mock::start_active_era(test_era + 1);

		// Out of order claims works.
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, test_era, 2));
		assert_eq!(ClaimedRewards::<Test>::get(test_era, &11), vec![2]);

		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, test_era));
		assert_eq!(ClaimedRewards::<Test>::get(test_era, &11), vec![2, 0]);

		// cannot claim page 2 again
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, test_era, 2),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, test_era));
		assert_eq!(ClaimedRewards::<Test>::get(test_era, &11), vec![2, 0, 1]);

		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, test_era));
		assert_eq!(ClaimedRewards::<Test>::get(test_era, &11), vec![2, 0, 1, 3]);
	});
}

#[test]
fn test_page_count_and_size() {
	// Test that payout_stakers work in general and that it pays the correct amount of reward.
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1000;
		// Track the exposure of the validator and all nominators.
		// Create a validator:
		bond_validator(11, balance); // Default(64)
		assert_eq!(Validators::<Test>::count(), 1);

		// Create nominators, targeting stash of validators
		for i in 0..100 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
		}

		mock::start_active_era(1);

		// Since max exposure page size is 64, 2 pages of nominators are created.
		assert_eq!(EraInfo::<Test>::get_page_count(1, &11), 2);

		// first page has 64 nominators
		assert_eq!(EraInfo::<Test>::get_paged_exposure(1, &11, 0).unwrap().others().len(), 64);
		// second page has 36 nominators
		assert_eq!(EraInfo::<Test>::get_paged_exposure(1, &11, 1).unwrap().others().len(), 36);

		// now lets decrease page size
		MaxExposurePageSize::set(32);
		mock::start_active_era(2);
		// now we expect 4 pages.
		assert_eq!(EraInfo::<Test>::get_page_count(2, &11), 4);
		// first 3 pages have 32 nominators each
		assert_eq!(EraInfo::<Test>::get_paged_exposure(2, &11, 0).unwrap().others().len(), 32);
		assert_eq!(EraInfo::<Test>::get_paged_exposure(2, &11, 1).unwrap().others().len(), 32);
		assert_eq!(EraInfo::<Test>::get_paged_exposure(2, &11, 2).unwrap().others().len(), 32);
		assert_eq!(EraInfo::<Test>::get_paged_exposure(2, &11, 3).unwrap().others().len(), 4);

		// now lets decrease page size even more
		MaxExposurePageSize::set(5);
		mock::start_active_era(3);

		// now we expect the max 20 pages (100/5).
		assert_eq!(EraInfo::<Test>::get_page_count(3, &11), 20);
	});
}

#[test]
fn payout_stakers_handles_basic_errors() {
	// Here we will test payouts handle all errors.
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		// Consumed weight for all payout_stakers dispatches that fail
		let err_weight = <Test as Config>::WeightInfo::payout_stakers_alive_staked(0);

		// Same setup as the test above
		let balance = 1000;
		bond_validator(11, balance); // Default(64)

		// Create nominators, targeting stash
		for i in 0..100 {
			bond_nominator(1000 + i, balance + i as Balance, vec![11]);
		}

		mock::start_active_era(1);
		Staking::reward_by_ids(vec![(11, 1)]);

		// compute and ensure the reward amount is greater than zero.
		let _ = current_total_payout_for_duration(reward_time_per_era());

		mock::start_active_era(2);

		// Wrong Era, too big
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0),
			Error::<Test>::InvalidEraToReward.with_weight(err_weight)
		);
		// Wrong Staker
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 10, 1, 0),
			Error::<Test>::NotStash.with_weight(err_weight)
		);

		let last_era = 99;
		for i in 3..=last_era {
			Staking::reward_by_ids(vec![(11, 1)]);
			// compute and ensure the reward amount is greater than zero.
			let _ = current_total_payout_for_duration(reward_time_per_era());
			mock::start_active_era(i);
		}

		let history_depth = HistoryDepth::get();
		let expected_last_reward_era = last_era - 1;
		let expected_start_reward_era = last_era - history_depth;

		// We are at era last_era=99. Given history_depth=80, we should be able
		// to payout era starting from expected_start_reward_era=19 through
		// expected_last_reward_era=98 (80 total eras), but not 18 or 99.
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_start_reward_era - 1,
				0
			),
			Error::<Test>::InvalidEraToReward.with_weight(err_weight)
		);
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era + 1,
				0
			),
			Error::<Test>::InvalidEraToReward.with_weight(err_weight)
		);
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			expected_start_reward_era,
			0
		));
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			expected_last_reward_era,
			0
		));

		// can call page 1
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			expected_last_reward_era,
			1
		));

		// Can't claim again
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_start_reward_era,
				0
			),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era,
				0
			),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era,
				1
			),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		// invalid page
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era,
				2
			),
			Error::<Test>::InvalidPage.with_weight(err_weight)
		);
	});
}

#[test]
fn test_commission_paid_across_pages() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1;
		let commission = 50;
		// Create a validator:
		bond_validator(11, balance);
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(commission), blocked: false }
		));
		assert_eq!(Validators::<Test>::count(), 1);

		// Create nominators, targeting stash of validators
		for i in 0..200 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
		}

		mock::start_active_era(1);
		Staking::reward_by_ids(vec![(11, 1)]);

		// Since `MaxExposurePageSize = 64`, there are four pages of validator
		// exposure.
		assert_eq!(EraInfo::<Test>::get_page_count(1, &11), 4);

		// compute and ensure the reward amount is greater than zero.
		let payout = current_total_payout_for_duration(reward_time_per_era());
		mock::start_active_era(2);

		let initial_balance = asset::stakeable_balance::<Test>(&11);
		// Payout rewards for first exposure page
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 0));

		let controller_balance_after_p0_payout = asset::stakeable_balance::<Test>(&11);

		// some commission is paid
		assert!(initial_balance < controller_balance_after_p0_payout);

		// payout all pages
		for i in 1..4 {
			let before_balance = asset::stakeable_balance::<Test>(&11);
			assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, i));
			let after_balance = asset::stakeable_balance::<Test>(&11);
			// some commission is paid for every page
			assert!(before_balance < after_balance);
		}

		assert_eq_error_rate!(
			asset::stakeable_balance::<Test>(&11),
			initial_balance + payout / 2,
			1,
		);
	});
}

#[test]
fn payout_stakers_handles_weight_refund() {
	// Note: this test relies on the assumption that `payout_stakers_alive_staked` is solely used by
	// `payout_stakers` to calculate the weight of each payout op.
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let max_nom_rewarded = MaxExposurePageSize::get();
		// Make sure the configured value is meaningful for our use.
		assert!(max_nom_rewarded >= 4);
		let half_max_nom_rewarded = max_nom_rewarded / 2;
		// Sanity check our max and half max nominator quantities.
		assert!(half_max_nom_rewarded > 0);
		assert!(max_nom_rewarded > half_max_nom_rewarded);

		let max_nom_rewarded_weight =
			<Test as Config>::WeightInfo::payout_stakers_alive_staked(max_nom_rewarded);
		let half_max_nom_rewarded_weight =
			<Test as Config>::WeightInfo::payout_stakers_alive_staked(half_max_nom_rewarded);
		let zero_nom_payouts_weight = <Test as Config>::WeightInfo::payout_stakers_alive_staked(0);
		assert!(zero_nom_payouts_weight.any_gt(Weight::zero()));
		assert!(half_max_nom_rewarded_weight.any_gt(zero_nom_payouts_weight));
		assert!(max_nom_rewarded_weight.any_gt(half_max_nom_rewarded_weight));

		let balance = 1000;
		bond_validator(11, balance);

		// Era 1
		start_active_era(1);

		// Reward just the validator.
		Staking::reward_by_ids(vec![(11, 1)]);

		// Add some `half_max_nom_rewarded` nominators who will start backing the validator in the
		// next era.
		for i in 0..half_max_nom_rewarded {
			bond_nominator((1000 + i).into(), balance + i as Balance, vec![11]);
		}

		// Era 2
		start_active_era(2);

		// Collect payouts when there are no nominators
		let call = TestCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 1,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), zero_nom_payouts_weight);

		// The validator is not rewarded in this era; so there will be zero payouts to claim for
		// this era.

		// Era 3
		start_active_era(3);

		// Collect payouts for an era where the validator did not receive any points.
		let call = TestCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 2,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), zero_nom_payouts_weight);

		// Reward the validator and its nominators.
		Staking::reward_by_ids(vec![(11, 1)]);

		// Era 4
		start_active_era(4);

		// Collect payouts when the validator has `half_max_nom_rewarded` nominators.
		let call = TestCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 3,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), half_max_nom_rewarded_weight);

		// Add enough nominators so that we are at the limit. They will be active nominators
		// in the next era.
		for i in half_max_nom_rewarded..max_nom_rewarded {
			bond_nominator((1000 + i).into(), balance + i as Balance, vec![11]);
		}

		// Era 5
		start_active_era(5);
		// We now have `max_nom_rewarded` nominators actively nominating our validator.

		// Reward the validator so we can collect for everyone in the next era.
		Staking::reward_by_ids(vec![(11, 1)]);

		// Era 6
		start_active_era(6);

		// Collect payouts when the validator had `half_max_nom_rewarded` nominators.
		let call = TestCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 5,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), max_nom_rewarded_weight);

		// Try and collect payouts for an era that has already been collected.
		let call = TestCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 5,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert!(result.is_err());
		// When there is an error the consumed weight == weight when there are 0 nominator payouts.
		assert_eq!(extract_actual_weight(&result, &info), zero_nom_payouts_weight);
	});
}

#[test]
fn bond_during_era_does_not_populate_legacy_claimed_rewards() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		// Era = None
		bond_validator(9, 1000);
		assert_eq!(
			Staking::ledger(9.into()).unwrap(),
			StakingLedgerInspect {
				stash: 9,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);
		mock::start_active_era(5);
		bond_validator(11, 1000);
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![],
			}
		);

		// make sure only era up to history depth is stored
		let current_era = 99;
		mock::start_active_era(current_era);
		bond_validator(13, 1000);
		assert_eq!(
			Staking::ledger(13.into()).unwrap(),
			StakingLedgerInspect {
				stash: 13,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: Default::default(),
			}
		);
	});
}

#[test]
fn offences_weight_calculated_correctly() {
	ExtBuilder::default().nominate(true).build_and_execute(|| {
		// On offence with zero offenders: 4 Reads, 1 Write
		let zero_offence_weight =
			<Test as frame_system::Config>::DbWeight::get().reads_writes(4, 1);
		assert_eq!(
			<Staking as OnOffenceHandler<_, _, _>>::on_offence(&[], &[Perbill::from_percent(50)], 0),
			zero_offence_weight
		);

		// On Offence with N offenders, Unapplied: 4 Reads, 1 Write + 4 Reads, 5 Writes, 2 Reads + 2
		// Writes for `SessionInterface::report_offence` call.
		let n_offence_unapplied_weight = <Test as frame_system::Config>::DbWeight::get()
			.reads_writes(4, 1) +
			<Test as frame_system::Config>::DbWeight::get().reads_writes(4, 5) +
			<Test as frame_system::Config>::DbWeight::get().reads_writes(2, 2);

		let offenders: Vec<
			OffenceDetails<
				<Test as frame_system::Config>::AccountId,
				pallet_session::historical::IdentificationTuple<Test>,
			>,
		> = (1..10)
			.map(|i| OffenceDetails {
				offender: (i, ()),
				reporters: vec![],
			})
			.collect();
		assert_eq!(
			<Staking as OnOffenceHandler<_, _, _>>::on_offence(
				&offenders,
				&[Perbill::from_percent(50)],
				0,
			),
			n_offence_unapplied_weight
		);

		// On Offence with one offenders, Applied
		let one_offender = [offence_from(11, Some(vec![1]))];

		let n = 1; // Number of offenders
		let rw = 3 + 3 * n; // rw reads and writes
		let one_offence_unapplied_weight =
			<Test as frame_system::Config>::DbWeight::get().reads_writes(4, 1)
		 +
			<Test as frame_system::Config>::DbWeight::get().reads_writes(rw, rw)
			// One `slash_cost`
			+ <Test as frame_system::Config>::DbWeight::get().reads_writes(6, 5)
			// `slash_cost` * nominators (1)
			+ <Test as frame_system::Config>::DbWeight::get().reads_writes(6, 5)
			// `reward_cost` * reporters (1)
			+ <Test as frame_system::Config>::DbWeight::get().reads_writes(2, 2)
			// `SessionInterface::report_offence`
			+ <Test as frame_system::Config>::DbWeight::get().reads_writes(2, 2);

		assert_eq!(
			<Staking as OnOffenceHandler<_, _, _>>::on_offence(
				&one_offender,
				&[Perbill::from_percent(50)],
				0,
			),
			one_offence_unapplied_weight
		);
	});
}

#[test]
fn payout_to_any_account_works() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1000;
		// Create a validator:
		bond_validator(11, balance); // Default(64)

		// Create a stash/controller pair
		bond_nominator(1234, 100, vec![11]);

		// Update payout location
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(1234), RewardDestination::Account(42)));

		// Reward Destination account doesn't exist
		assert_eq!(asset::stakeable_balance::<Test>(&42), 0);

		mock::start_active_era(1);
		Staking::reward_by_ids(vec![(11, 1)]);
		// compute and ensure the reward amount is greater than zero.
		let _ = current_total_payout_for_duration(reward_time_per_era());
		mock::start_active_era(2);
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 0));

		// Payment is successful
		assert!(asset::stakeable_balance::<Test>(&42) > 0);
	})
}

#[test]
fn session_buffering_with_offset() {
	// similar to live-chains, have some offset for the first session
	ExtBuilder::default()
		.offset(2)
		.period(5)
		.session_per_era(5)
		.build_and_execute(|| {
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 0);

			start_session(1);
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 1);
			assert_eq!(System::block_number(), 2);

			start_session(2);
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 2);
			assert_eq!(System::block_number(), 7);

			start_session(3);
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 3);
			assert_eq!(System::block_number(), 12);

			// active era is lagging behind by one session, because of how session module works.
			start_session(4);
			assert_eq!(current_era(), 1);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 4);
			assert_eq!(System::block_number(), 17);

			start_session(5);
			assert_eq!(current_era(), 1);
			assert_eq!(active_era(), 1);
			assert_eq!(Session::current_index(), 5);
			assert_eq!(System::block_number(), 22);

			// go all the way to active 2.
			start_active_era(2);
			assert_eq!(current_era(), 2);
			assert_eq!(active_era(), 2);
			assert_eq!(Session::current_index(), 10);
		});
}

#[test]
fn session_buffering_no_offset() {
	// no offset, first session starts immediately
	ExtBuilder::default()
		.offset(0)
		.period(5)
		.session_per_era(5)
		.build_and_execute(|| {
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 0);

			start_session(1);
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 1);
			assert_eq!(System::block_number(), 5);

			start_session(2);
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 2);
			assert_eq!(System::block_number(), 10);

			start_session(3);
			assert_eq!(current_era(), 0);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 3);
			assert_eq!(System::block_number(), 15);

			// active era is lagging behind by one session, because of how session module works.
			start_session(4);
			assert_eq!(current_era(), 1);
			assert_eq!(active_era(), 0);
			assert_eq!(Session::current_index(), 4);
			assert_eq!(System::block_number(), 20);

			start_session(5);
			assert_eq!(current_era(), 1);
			assert_eq!(active_era(), 1);
			assert_eq!(Session::current_index(), 5);
			assert_eq!(System::block_number(), 25);

			// go all the way to active 2.
			start_active_era(2);
			assert_eq!(current_era(), 2);
			assert_eq!(active_era(), 2);
			assert_eq!(Session::current_index(), 10);
		});
}

#[test]
fn cannot_rebond_to_lower_than_ed() {
	ExtBuilder::default()
		.existential_deposit(11)
		.balance_factor(11)
		.build_and_execute(|| {
			// initial stuff.
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 11 * 1000,
					active: 11 * 1000,
					unlocking: Default::default(),
					legacy_claimed_rewards: bounded_vec![],
				}
			);

			// unbond all of it. must be chilled first.
			assert_ok!(Staking::chill(RuntimeOrigin::signed(21)));
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(21), 11 * 1000));
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 11 * 1000,
					active: 0,
					unlocking: bounded_vec![UnlockChunk { value: 11 * 1000, era: 3 }],
					legacy_claimed_rewards: bounded_vec![],
				}
			);

			// now bond a wee bit more
			assert_noop!(
				Staking::rebond(RuntimeOrigin::signed(21), 5),
				Error::<Test>::InsufficientBond
			);
		})
}

#[test]
fn cannot_bond_extra_to_lower_than_ed() {
	ExtBuilder::default()
		.existential_deposit(11)
		.balance_factor(11)
		.build_and_execute(|| {
			// initial stuff.
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 11 * 1000,
					active: 11 * 1000,
					unlocking: Default::default(),
					legacy_claimed_rewards: bounded_vec![],
				}
			);

			// unbond all of it. must be chilled first.
			assert_ok!(Staking::chill(RuntimeOrigin::signed(21)));
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(21), 11 * 1000));
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 11 * 1000,
					active: 0,
					unlocking: bounded_vec![UnlockChunk { value: 11 * 1000, era: 3 }],
					legacy_claimed_rewards: bounded_vec![],
				}
			);

			// now bond a wee bit more
			assert_noop!(
				Staking::bond_extra(RuntimeOrigin::signed(21), 5),
				Error::<Test>::InsufficientBond,
			);
		})
}

#[test]
fn do_not_die_when_active_is_ed() {
	let ed = 10;
	ExtBuilder::default()
		.existential_deposit(ed)
		.balance_factor(ed)
		.build_and_execute(|| {
			// given
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 1000 * ed,
					active: 1000 * ed,
					unlocking: Default::default(),
					legacy_claimed_rewards: bounded_vec![],
				}
			);

			// when unbond all of it except ed.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(21), 999 * ed));
			start_active_era(3);
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(21), 100));

			// then
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: ed,
					active: ed,
					unlocking: Default::default(),
					legacy_claimed_rewards: bounded_vec![],
				}
			);
		})
}

#[test]
fn on_finalize_weight_is_nonzero() {
	ExtBuilder::default().build_and_execute(|| {
		let on_finalize_weight = <Test as frame_system::Config>::DbWeight::get().reads(1);
		assert!(<Staking as Hooks<u64>>::on_initialize(1).all_gte(on_finalize_weight));
	})
}

#[test]
fn restricted_accounts_can_only_withdraw() {
	ExtBuilder::default().build_and_execute(|| {
		start_active_era(1);
		// alice is a non blacklisted account.
		let alice = 301;
		let _ = Balances::make_free_balance_be(&alice, 500);
		// alice can bond
		assert_ok!(Staking::bond(RuntimeOrigin::signed(alice), 100, RewardDestination::Staked));
		// and bob is a blacklisted account
		let bob = 302;
		let _ = Balances::make_free_balance_be(&bob, 500);
		restrict(&bob);

		// Bob cannot bond
		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(bob), 100, RewardDestination::Staked,),
			Error::<Test>::Restricted
		);

		// alice is blacklisted now and cannot bond anymore
		restrict(&alice);
		assert_noop!(
			Staking::bond_extra(RuntimeOrigin::signed(alice), 100),
			Error::<Test>::Restricted
		);
		// but she can unbond her existing bond
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 100));

		// she cannot rebond the unbonded amount
		start_active_era(2);
		assert_noop!(Staking::rebond(RuntimeOrigin::signed(alice), 50), Error::<Test>::Restricted);

		// move to era when alice fund can be withdrawn
		start_active_era(4);
		// alice can withdraw now
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(alice), 0));
		// she still cannot bond
		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(alice), 100, RewardDestination::Staked,),
			Error::<Test>::Restricted
		);

		// bob is removed from restrict list
		remove_from_restrict_list(&bob);
		// bob can bond now
		assert_ok!(Staking::bond(RuntimeOrigin::signed(bob), 100, RewardDestination::Staked));
		// and bond extra
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(bob), 100));

		start_active_era(6);
		// unbond also works.
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(bob), 100));
		// bob can withdraw as well.
		start_active_era(9);
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(bob), 0));
	})
}

mod election_data_provider {
	use super::*;
	use frame_election_provider_support::ElectionDataProvider;

	#[test]
	fn targets_2sec_block() {
		let mut validators = 1000;
		while <Test as Config>::WeightInfo::get_npos_targets(validators).all_lt(Weight::from_parts(
			2u64 * frame_support::weights::constants::WEIGHT_REF_TIME_PER_SECOND,
			u64::MAX,
		)) {
			validators += 1;
		}

		println!("Can create a snapshot of {} validators in 2sec block", validators);
	}

	#[test]
	fn voters_2sec_block() {
		// we assume a network only wants up to 1000 validators in most cases, thus having 2000
		// candidates is as high as it gets.
		let validators = 2000;
		let mut nominators = 1000;

		while <Test as Config>::WeightInfo::get_npos_voters(validators, nominators).all_lt(
			Weight::from_parts(
				2u64 * frame_support::weights::constants::WEIGHT_REF_TIME_PER_SECOND,
				u64::MAX,
			),
		) {
			nominators += 1;
		}

		println!(
			"Can create a snapshot of {} nominators [{} validators, each 1 slashing] in 2sec block",
			nominators, validators
		);
	}

	#[test]
	fn set_minimum_active_stake_is_correct() {
		ExtBuilder::default()
			.nominate(false)
			.add_staker(61, 61, 2_000, StakerStatus::<AccountId>::Nominator(vec![21]))
			.add_staker(71, 71, 10, StakerStatus::<AccountId>::Nominator(vec![21]))
			.add_staker(81, 81, 50, StakerStatus::<AccountId>::Nominator(vec![21]))
			.build_and_execute(|| {
				// default bounds are unbounded.
				assert_ok!(<Staking as ElectionDataProvider>::electing_voters(
					DataProviderBounds::default()
				));
				assert_eq!(MinimumActiveStake::<Test>::get(), 10);

				// remove staker with lower bond by limiting the number of voters and check
				// `MinimumActiveStake` again after electing voters.
				let bounds = ElectionBoundsBuilder::default().voters_count(5.into()).build();
				assert_ok!(<Staking as ElectionDataProvider>::electing_voters(bounds.voters));
				assert_eq!(MinimumActiveStake::<Test>::get(), 50);
			});
	}

	#[test]
	fn set_minimum_active_stake_lower_bond_works() {
		// if there are no voters, minimum active stake is zero (should not happen).
		ExtBuilder::default().has_stakers(false).build_and_execute(|| {
			// default bounds are unbounded.
			assert_ok!(<Staking as ElectionDataProvider>::electing_voters(
				DataProviderBounds::default()
			));
			assert_eq!(<Test as Config>::VoterList::count(), 0);
			assert_eq!(MinimumActiveStake::<Test>::get(), 0);
		});

		// lower non-zero active stake below `MinNominatorBond` is the minimum active stake if
		// it is selected as part of the npos voters.
		ExtBuilder::default().has_stakers(true).nominate(true).build_and_execute(|| {
			assert_eq!(MinNominatorBond::<Test>::get(), 1);
			assert_eq!(<Test as Config>::VoterList::count(), 4);

			assert_ok!(Staking::bond(RuntimeOrigin::signed(4), 5, RewardDestination::Staked,));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(4), vec![1]));
			assert_eq!(<Test as Config>::VoterList::count(), 5);

			let voters_before =
				<Staking as ElectionDataProvider>::electing_voters(DataProviderBounds::default())
					.unwrap();
			assert_eq!(MinimumActiveStake::<Test>::get(), 5);

			// update minimum nominator bond.
			MinNominatorBond::<Test>::set(10);
			assert_eq!(MinNominatorBond::<Test>::get(), 10);
			// voter list still considers nominator 4 for voting, even though its active stake is
			// lower than `MinNominatorBond`.
			assert_eq!(<Test as Config>::VoterList::count(), 5);

			let voters =
				<Staking as ElectionDataProvider>::electing_voters(DataProviderBounds::default())
					.unwrap();
			assert_eq!(voters_before, voters);

			// minimum active stake is lower than `MinNominatorBond`.
			assert_eq!(MinimumActiveStake::<Test>::get(), 5);
		});
	}

	#[test]
	fn set_minimum_active_bond_corrupt_state() {
		ExtBuilder::default()
			.has_stakers(true)
			.nominate(true)
			.add_staker(61, 61, 2_000, StakerStatus::<AccountId>::Nominator(vec![21]))
			.build_and_execute(|| {
				assert_eq!(Staking::weight_of(&101), 500);
				let voters = <Staking as ElectionDataProvider>::electing_voters(
					DataProviderBounds::default(),
				)
				.unwrap();
				assert_eq!(voters.len(), 5);
				assert_eq!(MinimumActiveStake::<Test>::get(), 500);

				assert_ok!(Staking::unbond(RuntimeOrigin::signed(101), 200));
				start_active_era(10);
				assert_ok!(Staking::unbond(RuntimeOrigin::signed(101), 100));
				start_active_era(20);

				// corrupt ledger state by lowering max unlocking chunks bounds.
				MaxUnlockingChunks::set(1);

				let voters = <Staking as ElectionDataProvider>::electing_voters(
					DataProviderBounds::default(),
				)
				.unwrap();
				// number of returned voters decreases since ledger entry of stash 101 is now
				// corrupt.
				assert_eq!(voters.len(), 4);
				// minimum active stake does not take into consideration the corrupt entry.
				assert_eq!(MinimumActiveStake::<Test>::get(), 2_000);

				// voter weight of corrupted ledger entry is 0.
				assert_eq!(Staking::weight_of(&101), 0);

				// reset max unlocking chunks for try_state to pass.
				MaxUnlockingChunks::set(32);
			})
	}

	#[test]
	fn voters_include_self_vote() {
		ExtBuilder::default().nominate(false).build_and_execute(|| {
			// default bounds are unbounded.
			assert!(<Validators<Test>>::iter().map(|(x, _)| x).all(|v| Staking::electing_voters(
				DataProviderBounds::default()
			)
			.unwrap()
			.into_iter()
			.any(|(w, _, t)| { v == w && t[0] == w })))
		})
	}

	// Tests the criteria that in `ElectionDataProvider::voters` function, we try to get at most
	// `maybe_max_len` voters, and if some of them end up being skipped, we iterate at most `2 *
	// maybe_max_len`.
	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn only_iterates_max_2_times_max_allowed_len() {
		ExtBuilder::default()
			.nominate(false)
			// the best way to invalidate a bunch of nominators is to have them nominate a lot of
			// ppl, but then lower the MaxNomination limit.
			.add_staker(
				61,
				61,
				2_000,
				StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]),
			)
			.add_staker(
				71,
				71,
				2_000,
				StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]),
			)
			.add_staker(
				81,
				81,
				2_000,
				StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]),
			)
			.build_and_execute(|| {
				let bounds_builder = ElectionBoundsBuilder::default();
				// all voters ordered by stake,
				assert_eq!(
					<Test as Config>::VoterList::iter().collect::<Vec<_>>(),
					vec![61, 71, 81, 11, 21, 31]
				);

				AbsoluteMaxNominations::set(2);

				// we want 2 voters now, and in maximum we allow 4 iterations. This is what happens:
				// 61 is pruned;
				// 71 is pruned;
				// 81 is pruned;
				// 11 is taken;
				// we finish since the 2x limit is reached.
				assert_eq!(
					Staking::electing_voters(bounds_builder.voters_count(2.into()).build().voters)
						.unwrap()
						.iter()
						.map(|(stash, _, _)| stash)
						.copied()
						.collect::<Vec<_>>(),
					vec![11],
				);
			});
	}

	#[test]
	fn respects_snapshot_count_limits() {
		ExtBuilder::default()
			.set_status(41, StakerStatus::Validator)
			.build_and_execute(|| {
				// sum of all nominators who'd be voters (1), plus the self-votes (4).
				assert_eq!(<Test as Config>::VoterList::count(), 5);

				let bounds_builder = ElectionBoundsBuilder::default();

				// if voter count limit is less..
				assert_eq!(
					Staking::electing_voters(bounds_builder.voters_count(1.into()).build().voters)
						.unwrap()
						.len(),
					1
				);

				// if voter count limit is equal..
				assert_eq!(
					Staking::electing_voters(bounds_builder.voters_count(5.into()).build().voters)
						.unwrap()
						.len(),
					5
				);

				// if voter count limit is more.
				assert_eq!(
					Staking::electing_voters(bounds_builder.voters_count(55.into()).build().voters)
						.unwrap()
						.len(),
					5
				);

				// if target count limit is more..
				assert_eq!(
					Staking::electable_targets(
						bounds_builder.targets_count(6.into()).build().targets
					)
					.unwrap()
					.len(),
					4
				);

				// if target count limit is equal..
				assert_eq!(
					Staking::electable_targets(
						bounds_builder.targets_count(4.into()).build().targets
					)
					.unwrap()
					.len(),
					4
				);

				// if target limit count is less, then we return an error.
				assert_eq!(
					Staking::electable_targets(
						bounds_builder.targets_count(1.into()).build().targets
					)
					.unwrap_err(),
					"Target snapshot too big"
				);
			});
	}

	#[test]
	fn respects_snapshot_size_limits() {
		ExtBuilder::default().build_and_execute(|| {
			// voters: set size bounds that allows only for 1 voter.
			let bounds = ElectionBoundsBuilder::default().voters_size(26.into()).build();
			let elected = Staking::electing_voters(bounds.voters).unwrap();
			assert!(elected.encoded_size() == 26 as usize);
			let prev_len = elected.len();

			// larger size bounds means more quota for voters.
			let bounds = ElectionBoundsBuilder::default().voters_size(100.into()).build();
			let elected = Staking::electing_voters(bounds.voters).unwrap();
			assert!(elected.encoded_size() <= 100 as usize);
			assert!(elected.len() > 1 && elected.len() > prev_len);

			// targets: set size bounds that allows for only one target to fit in the snapshot.
			let bounds = ElectionBoundsBuilder::default().targets_size(10.into()).build();
			let elected = Staking::electable_targets(bounds.targets).unwrap();
			assert!(elected.encoded_size() == 9 as usize);
			let prev_len = elected.len();

			// larger size bounds means more space for targets.
			let bounds = ElectionBoundsBuilder::default().targets_size(100.into()).build();
			let elected = Staking::electable_targets(bounds.targets).unwrap();
			assert!(elected.encoded_size() <= 100 as usize);
			assert!(elected.len() > 1 && elected.len() > prev_len);
		});
	}

	#[test]
	fn nomination_quota_checks_at_nominate_works() {
		ExtBuilder::default().nominate(false).build_and_execute(|| {
			// stash bond of 222 has a nomination quota of 2 targets.
			bond(61, 222);
			assert_eq!(Staking::api_nominations_quota(222), 2);

			// nominating with targets below the nomination quota works.
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(61), vec![11]));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(61), vec![11, 12]));

			// nominating with targets above the nomination quota returns error.
			assert_noop!(
				Staking::nominate(RuntimeOrigin::signed(61), vec![11, 12, 13]),
				Error::<Test>::TooManyTargets
			);
		});
	}

	#[test]
	fn lazy_quota_npos_voters_works_above_quota() {
		ExtBuilder::default()
			.nominate(false)
			.add_staker(
				61,
				60,
				300, // 300 bond has 16 nomination quota.
				StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]),
			)
			.build_and_execute(|| {
				// unbond 78 from stash 60 so that it's bonded balance is 222, which has a lower
				// nomination quota than at nomination time (max 2 targets).
				assert_ok!(Staking::unbond(RuntimeOrigin::signed(61), 78));
				assert_eq!(Staking::api_nominations_quota(300 - 78), 2);

				// even through 61 has nomination quota of 2 at the time of the election, all the
				// nominations (5) will be used.
				assert_eq!(
					Staking::electing_voters(DataProviderBounds::default())
						.unwrap()
						.iter()
						.map(|(stash, _, targets)| (*stash, targets.len()))
						.collect::<Vec<_>>(),
					vec![(11, 1), (21, 1), (31, 1), (61, 5)],
				);
			});
	}

	#[test]
	fn nominations_quota_limits_size_work() {
		ExtBuilder::default()
			.nominate(false)
			.add_staker(
				71,
				70,
				333,
				StakerStatus::<AccountId>::Nominator(vec![16, 15, 14, 13, 12, 11, 10]),
			)
			.build_and_execute(|| {
				// nominations of controller 70 won't be added due to voter size limit exceeded.
				let bounds = ElectionBoundsBuilder::default().voters_size(100.into()).build();
				assert_eq!(
					Staking::electing_voters(bounds.voters)
						.unwrap()
						.iter()
						.map(|(stash, _, targets)| (*stash, targets.len()))
						.collect::<Vec<_>>(),
					vec![(11, 1), (21, 1), (31, 1)],
				);

				assert_eq!(
					*staking_events().last().unwrap(),
					Event::SnapshotVotersSizeExceeded { size: 75 }
				);

				// however, if the election voter size bounds were larger, the snapshot would
				// include the electing voters of 70.
				let bounds = ElectionBoundsBuilder::default().voters_size(1_000.into()).build();
				assert_eq!(
					Staking::electing_voters(bounds.voters)
						.unwrap()
						.iter()
						.map(|(stash, _, targets)| (*stash, targets.len()))
						.collect::<Vec<_>>(),
					vec![(11, 1), (21, 1), (31, 1), (71, 7)],
				);
			});
	}

	#[test]
	fn estimate_next_election_works() {
		ExtBuilder::default().session_per_era(5).period(5).build_and_execute(|| {
			// first session is always length 0.
			for b in 1..20 {
				run_to_block(b);
				assert_eq!(Staking::next_election_prediction(System::block_number()), 20);
			}

			// election
			run_to_block(20);
			assert_eq!(Staking::next_election_prediction(System::block_number()), 45);
			assert_eq!(staking_events().len(), 1);
			assert_eq!(*staking_events().last().unwrap(), Event::StakersElected);

			for b in 21..45 {
				run_to_block(b);
				assert_eq!(Staking::next_election_prediction(System::block_number()), 45);
			}

			// election
			run_to_block(45);
			assert_eq!(Staking::next_election_prediction(System::block_number()), 70);
			assert_eq!(staking_events().len(), 3);
			assert_eq!(*staking_events().last().unwrap(), Event::StakersElected);

			Staking::force_no_eras(RuntimeOrigin::root()).unwrap();
			assert_eq!(Staking::next_election_prediction(System::block_number()), u64::MAX);

			Staking::force_new_era_always(RuntimeOrigin::root()).unwrap();
			assert_eq!(Staking::next_election_prediction(System::block_number()), 45 + 5);

			Staking::force_new_era(RuntimeOrigin::root()).unwrap();
			assert_eq!(Staking::next_election_prediction(System::block_number()), 45 + 5);

			// Do a fail election
			MinimumValidatorCount::<Test>::put(1000);
			run_to_block(50);
			// Election: failed, next session is a new election
			assert_eq!(Staking::next_election_prediction(System::block_number()), 50 + 5);
			// The new era is still forced until a new era is planned.
			assert_eq!(ForceEra::<Test>::get(), Forcing::ForceNew);

			MinimumValidatorCount::<Test>::put(2);
			run_to_block(55);
			assert_eq!(Staking::next_election_prediction(System::block_number()), 55 + 25);
			assert_eq!(staking_events().len(), 10);
			assert_eq!(
				*staking_events().last().unwrap(),
				Event::ForceEra { mode: Forcing::NotForcing }
			);
			assert_eq!(
				*staking_events().get(staking_events().len() - 2).unwrap(),
				Event::StakersElected
			);
			// The new era has been planned, forcing is changed from `ForceNew` to `NotForcing`.
			assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
		})
	}
}

#[test]
#[should_panic]
fn count_check_works() {
	ExtBuilder::default().build_and_execute(|| {
		// We should never insert into the validators or nominators map directly as this will
		// not keep track of the count. This test should panic as we verify the count is accurate
		// after every test using the `post_checks` in `mock`.
		Validators::<Test>::insert(987654321, ValidatorPrefs::default());
		Nominators::<Test>::insert(
			987654321,
			Nominations {
				targets: Default::default(),
				submitted_in: Default::default(),
				suppressed: false,
			},
		);
	})
}

#[test]
#[should_panic = "called `Result::unwrap()` on an `Err` value: Other(\"number of entries in payee storage items does not match the number of bonded ledgers\")"]
fn check_payee_invariant1_works() {
	// A bonded ledger should always have an assigned `Payee` This test should panic as we verify
	// that a bad state will panic due to the `try_state` checks in the `post_checks` in `mock`.
	ExtBuilder::default().build_and_execute(|| {
		let rogue_ledger = StakingLedger::<Test>::new(123456, 20);
		Ledger::<Test>::insert(123456, rogue_ledger);
	})
}

#[test]
#[should_panic = "called `Result::unwrap()` on an `Err` value: Other(\"number of entries in payee storage items does not match the number of bonded ledgers\")"]
fn check_payee_invariant2_works() {
	// The number of entries in both `Payee` and of bonded staking ledgers should match. This test
	// should panic as we verify that a bad state will panic due to the `try_state` checks in the
	// `post_checks` in `mock`.
	ExtBuilder::default().build_and_execute(|| {
		Payee::<Test>::insert(1111, RewardDestination::Staked);
	})
}

#[test]
fn min_bond_checks_work() {
	ExtBuilder::default()
		.existential_deposit(100)
		.balance_factor(100)
		.min_nominator_bond(1_000)
		.min_validator_bond(1_500)
		.build_and_execute(|| {
			// 500 is not enough for any role
			assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 500, RewardDestination::Stash));
			assert_noop!(
				Staking::nominate(RuntimeOrigin::signed(3), vec![1]),
				Error::<Test>::InsufficientBond
			);
			assert_noop!(
				Staking::validate(RuntimeOrigin::signed(3), ValidatorPrefs::default()),
				Error::<Test>::InsufficientBond,
			);

			// 1000 is enough for nominator
			assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(3), 500));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![1]));
			assert_noop!(
				Staking::validate(RuntimeOrigin::signed(3), ValidatorPrefs::default()),
				Error::<Test>::InsufficientBond,
			);

			// 1500 is enough for validator
			assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(3), 500));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![1]));
			assert_ok!(Staking::validate(RuntimeOrigin::signed(3), ValidatorPrefs::default()));

			// Can't unbond anything as validator
			assert_noop!(
				Staking::unbond(RuntimeOrigin::signed(3), 500),
				Error::<Test>::InsufficientBond
			);

			// Once they are a nominator, they can unbond 500
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![1]));
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 500));
			assert_noop!(
				Staking::unbond(RuntimeOrigin::signed(3), 500),
				Error::<Test>::InsufficientBond
			);

			// Once they are chilled they can unbond everything
			assert_ok!(Staking::chill(RuntimeOrigin::signed(3)));
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 1000));
		})
}

#[test]
fn chill_other_works() {
	ExtBuilder::default()
		.existential_deposit(100)
		.balance_factor(100)
		.min_nominator_bond(1_000)
		.min_validator_bond(1_500)
		.build_and_execute(|| {
			let initial_validators = Validators::<Test>::count();
			let initial_nominators = Nominators::<Test>::count();
			for i in 0..15 {
				let a = 4 * i;
				let b = 4 * i + 2;
				let c = 4 * i + 3;
				asset::set_stakeable_balance::<Test>(&a, 100_000);
				asset::set_stakeable_balance::<Test>(&b, 100_000);
				asset::set_stakeable_balance::<Test>(&c, 100_000);

				// Nominator
				assert_ok!(Staking::bond(RuntimeOrigin::signed(a), 1000, RewardDestination::Stash));
				assert_ok!(Staking::nominate(RuntimeOrigin::signed(a), vec![1]));

				// Validator
				assert_ok!(Staking::bond(RuntimeOrigin::signed(b), 1500, RewardDestination::Stash));
				assert_ok!(Staking::validate(RuntimeOrigin::signed(b), ValidatorPrefs::default()));
			}

			// To chill other users, we need to:
			// * Set a minimum bond amount
			// * Set a limit
			// * Set a threshold
			//
			// If any of these are missing, we do not have enough information to allow the
			// `chill_other` to succeed from one user to another.
			//
			// Out of 8 possible cases, only one will allow the use of `chill_other`, which is
			// when all 3 conditions are met.

			// 1. No limits whatsoever
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Remove,
			));

			// Can't chill these users
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 2),
				Error::<Test>::CannotChillOther
			);

			// 2. Change only the minimum bonds.
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Set(1_500),
				ConfigOp::Set(2_000),
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
			));

			// Still can't chill these users
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 2),
				Error::<Test>::CannotChillOther
			);

			// 3. Add nominator/validator count limits, but no other threshold.
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Set(10),
				ConfigOp::Set(10),
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
			));

			// Still can't chill these users
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 2),
				Error::<Test>::CannotChillOther
			);

			// 4. Add chil threshold, but no other limits
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Set(Percent::from_percent(75)),
				ConfigOp::Noop,
				ConfigOp::Noop,
			));

			// Still can't chill these users
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 2),
				Error::<Test>::CannotChillOther
			);

			// 5. Add bond and count limits, but no threshold
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Set(1_500),
				ConfigOp::Set(2_000),
				ConfigOp::Set(10),
				ConfigOp::Set(10),
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Remove,
			));

			// Still can't chill these users
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 2),
				Error::<Test>::CannotChillOther
			);

			// 6. Add bond and threshold limits, but no count limits
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Set(Percent::from_percent(75)),
				ConfigOp::Noop,
				ConfigOp::Noop,
			));

			// Still can't chill these users
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 2),
				Error::<Test>::CannotChillOther
			);

			// 7. Add count limits and a chill threshold, but no bond limits
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Set(10),
				ConfigOp::Set(10),
				ConfigOp::Set(Percent::from_percent(75)),
				ConfigOp::Noop,
				ConfigOp::Noop,
			));

			// Still can't chill these users
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 2),
				Error::<Test>::CannotChillOther
			);

			// 8. Add all limits
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Set(1_500),
				ConfigOp::Set(2_000),
				ConfigOp::Set(10),
				ConfigOp::Set(10),
				ConfigOp::Set(Percent::from_percent(75)),
				ConfigOp::Noop,
				ConfigOp::Noop,
			));

			// 16 people total because tests start with 2 active one
			assert_eq!(Nominators::<Test>::count(), 15 + initial_nominators);
			assert_eq!(Validators::<Test>::count(), 15 + initial_validators);

			// Users can now be chilled down to 7 people, so we try to remove 9 of them (starting
			// with 16)
			for i in 6..15 {
				let b = 4 * i;
				let d = 4 * i + 2;
				assert_ok!(Staking::chill_other(RuntimeOrigin::signed(1337), b));
				assert_eq!(*staking_events().last().unwrap(), Event::Chilled { stash: b });
				assert_ok!(Staking::chill_other(RuntimeOrigin::signed(1337), d));
				assert_eq!(*staking_events().last().unwrap(), Event::Chilled { stash: d });
			}

			// chill a nominator. Limit is not reached, not chill-able
			assert_eq!(Nominators::<Test>::count(), 7);
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1337), 0),
				Error::<Test>::CannotChillOther
			);
			// chill a validator. Limit is reached, chill-able.
			assert_eq!(Validators::<Test>::count(), 9);
			assert_ok!(Staking::chill_other(RuntimeOrigin::signed(1337), 2));
		})
}

#[test]
fn capped_stakers_works() {
	ExtBuilder::default().build_and_execute(|| {
		let validator_count = Validators::<Test>::count();
		assert_eq!(validator_count, 3);
		let nominator_count = Nominators::<Test>::count();
		assert_eq!(nominator_count, 1);

		// Change the maximums
		let max = 10;
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Set(10),
			ConfigOp::Set(10),
			ConfigOp::Set(max),
			ConfigOp::Set(max),
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Noop,
		));

		// can create `max - validator_count` validators
		let mut some_existing_validator = AccountId::default();
		for i in 0..max - validator_count {
			let (_, controller) = testing_utils::create_stash_controller::<Test>(
				i + 10_000_000,
				100,
				RewardDestination::Stash,
			)
			.unwrap();
			assert_ok!(Staking::validate(
				RuntimeOrigin::signed(controller),
				ValidatorPrefs::default()
			));
			some_existing_validator = controller;
		}

		// but no more
		let (_, last_validator) =
			testing_utils::create_stash_controller::<Test>(1337, 100, RewardDestination::Stash)
				.unwrap();

		assert_noop!(
			Staking::validate(RuntimeOrigin::signed(last_validator), ValidatorPrefs::default()),
			Error::<Test>::TooManyValidators,
		);

		// same with nominators
		let mut some_existing_nominator = AccountId::default();
		for i in 0..max - nominator_count {
			let (_, controller) = testing_utils::create_stash_controller::<Test>(
				i + 20_000_000,
				100,
				RewardDestination::Stash,
			)
			.unwrap();
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(controller), vec![1]));
			some_existing_nominator = controller;
		}

		// one more is too many.
		let (_, last_nominator) = testing_utils::create_stash_controller::<Test>(
			30_000_000,
			100,
			RewardDestination::Stash,
		)
		.unwrap();
		assert_noop!(
			Staking::nominate(RuntimeOrigin::signed(last_nominator), vec![1]),
			Error::<Test>::TooManyNominators
		);

		// Re-nominate works fine
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(some_existing_nominator), vec![1]));
		// Re-validate works fine
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(some_existing_validator),
			ValidatorPrefs::default()
		));

		// No problem when we set to `None` again
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
		));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(last_nominator), vec![1]));
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(last_validator),
			ValidatorPrefs::default()
		));
	})
}

#[test]
fn min_commission_works() {
	ExtBuilder::default().build_and_execute(|| {
		// account 11 controls the stash of itself.
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(5), blocked: false }
		));

		// event emitted should be correct
		assert_eq!(
			*staking_events().last().unwrap(),
			Event::ValidatorPrefsSet {
				stash: 11,
				prefs: ValidatorPrefs { commission: Perbill::from_percent(5), blocked: false }
			}
		);

		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Set(Perbill::from_percent(10)),
			ConfigOp::Noop,
		));

		// can't make it less than 10 now
		assert_noop!(
			Staking::validate(
				RuntimeOrigin::signed(11),
				ValidatorPrefs { commission: Perbill::from_percent(5), blocked: false }
			),
			Error::<Test>::CommissionTooLow
		);

		// can only change to higher.
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(10), blocked: false }
		));

		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(15), blocked: false }
		));
	})
}

#[test]
#[should_panic]
#[cfg(debug_assertions)]
fn change_of_absolute_max_nominations() {
	use frame_election_provider_support::ElectionDataProvider;
	ExtBuilder::default()
		.add_staker(61, 61, 10, StakerStatus::Nominator(vec![1]))
		.add_staker(71, 71, 10, StakerStatus::Nominator(vec![1, 2, 3]))
		.balance_factor(10)
		.build_and_execute(|| {
			// pre-condition
			assert_eq!(AbsoluteMaxNominations::get(), 16);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (71, 3), (61, 1)]
			);

			// default bounds are unbounded.
			let bounds = DataProviderBounds::default();

			// 3 validators and 3 nominators
			assert_eq!(Staking::electing_voters(bounds).unwrap().len(), 3 + 3);

			// abrupt change from 16 to 4, everyone should be fine.
			AbsoluteMaxNominations::set(4);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (71, 3), (61, 1)]
			);
			assert_eq!(Staking::electing_voters(bounds).unwrap().len(), 3 + 3);

			// No one can be chilled on account of non-decodable keys.
			for k in Nominators::<Test>::iter_keys() {
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1), k),
					Error::<Test>::CannotChillOther
				);
			}

			// abrupt change from 4 to 3, everyone should be fine.
			AbsoluteMaxNominations::set(3);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (71, 3), (61, 1)]
			);
			assert_eq!(Staking::electing_voters(bounds).unwrap().len(), 3 + 3);

			// As before, no one can be chilled on account of non-decodable keys.
			for k in Nominators::<Test>::iter_keys() {
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1), k),
					Error::<Test>::CannotChillOther
				);
			}

			// abrupt change from 3 to 2, this should cause some nominators to be non-decodable, and
			// thus non-existent unless they update.
			AbsoluteMaxNominations::set(2);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (61, 1)]
			);

			// 101 and 61 still cannot be chilled by someone else.
			for k in [101, 61].iter() {
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1), *k),
					Error::<Test>::CannotChillOther
				);
			}

			// 71 is still in storage..
			assert!(Nominators::<Test>::contains_key(71));
			// but its value cannot be decoded and default is returned.
			assert!(Nominators::<Test>::get(71).is_none());

			assert_eq!(Staking::electing_voters(bounds).unwrap().len(), 3 + 2);
			assert!(Nominators::<Test>::contains_key(101));

			// abrupt change from 2 to 1, this should cause some nominators to be non-decodable, and
			// thus non-existent unless they update.
			AbsoluteMaxNominations::set(1);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(61, 1)]
			);

			// 61 *still* cannot be chilled by someone else.
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1), 61),
				Error::<Test>::CannotChillOther
			);

			assert!(Nominators::<Test>::contains_key(71));
			assert!(Nominators::<Test>::contains_key(61));
			assert!(Nominators::<Test>::get(71).is_none());
			assert!(Nominators::<Test>::get(61).is_some());
			assert_eq!(Staking::electing_voters(bounds).unwrap().len(), 3 + 1);

			// now one of them can revive themselves by re-nominating to a proper value.
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(71), vec![1]));
			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(71, 1), (61, 1)]
			);

			// or they can be chilled by any account.
			assert!(Nominators::<Test>::contains_key(101));
			assert!(Nominators::<Test>::get(101).is_none());
			assert_ok!(Staking::chill_other(RuntimeOrigin::signed(71), 101));
			assert_eq!(*staking_events().last().unwrap(), Event::Chilled { stash: 101 });
			assert!(!Nominators::<Test>::contains_key(101));
			assert!(Nominators::<Test>::get(101).is_none());
		})
}

#[test]
fn nomination_quota_max_changes_decoding() {
	use frame_election_provider_support::ElectionDataProvider;
	ExtBuilder::default()
		.add_staker(60, 61, 10, StakerStatus::Nominator(vec![1]))
		.add_staker(70, 71, 10, StakerStatus::Nominator(vec![1, 2, 3]))
		.add_staker(30, 330, 10, StakerStatus::Nominator(vec![1, 2, 3, 4]))
		.add_staker(50, 550, 10, StakerStatus::Nominator(vec![1, 2, 3, 4]))
		.balance_factor(11)
		.build_and_execute(|| {
			// pre-condition.
			assert_eq!(MaxNominationsOf::<Test>::get(), 16);

			let unbonded_election = DataProviderBounds::default();

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(70, 3), (101, 2), (50, 4), (30, 4), (60, 1)]
			);
			// 4 validators and 4 nominators
			assert_eq!(Staking::electing_voters(unbonded_election).unwrap().len(), 4 + 4);
		});
}

#[test]
fn api_nominations_quota_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(Staking::api_nominations_quota(10), MaxNominationsOf::<Test>::get());
		assert_eq!(Staking::api_nominations_quota(333), MaxNominationsOf::<Test>::get());
		assert_eq!(Staking::api_nominations_quota(222), 2);
		assert_eq!(Staking::api_nominations_quota(111), 1);
	})
}

mod sorted_list_provider {
	use super::*;
	use frame_election_provider_support::SortedListProvider;

	#[test]
	fn re_nominate_does_not_change_counters_or_list() {
		ExtBuilder::default().nominate(true).build_and_execute(|| {
			// given
			let pre_insert_voter_count =
				(Nominators::<Test>::count() + Validators::<Test>::count()) as u32;
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);

			assert_eq!(
				<Test as Config>::VoterList::iter().collect::<Vec<_>>(),
				vec![11, 21, 31, 101]
			);

			// when account 101 renominates
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(101), vec![41]));

			// then counts don't change
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);
			// and the list is the same
			assert_eq!(
				<Test as Config>::VoterList::iter().collect::<Vec<_>>(),
				vec![11, 21, 31, 101]
			);
		});
	}

	#[test]
	fn re_validate_does_not_change_counters_or_list() {
		ExtBuilder::default().nominate(false).build_and_execute(|| {
			// given
			let pre_insert_voter_count =
				(Nominators::<Test>::count() + Validators::<Test>::count()) as u32;
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);

			assert_eq!(<Test as Config>::VoterList::iter().collect::<Vec<_>>(), vec![11, 21, 31]);

			// when account 11 re-validates
			assert_ok!(Staking::validate(RuntimeOrigin::signed(11), Default::default()));

			// then counts don't change
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);
			// and the list is the same
			assert_eq!(<Test as Config>::VoterList::iter().collect::<Vec<_>>(), vec![11, 21, 31]);
		});
	}
}

#[test]
fn force_apply_min_commission_works() {
	let prefs = |c| ValidatorPrefs { commission: Perbill::from_percent(c), blocked: false };
	let validators = || Validators::<Test>::iter().collect::<Vec<_>>();
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Staking::validate(RuntimeOrigin::signed(31), prefs(10)));
		assert_ok!(Staking::validate(RuntimeOrigin::signed(21), prefs(5)));

		// Given
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(0))]);
		MinCommission::<Test>::set(Perbill::from_percent(5));

		// When applying to a commission greater than min
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 31));
		// Then the commission is not changed
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(0))]);

		// When applying to a commission that is equal to min
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 21));
		// Then the commission is not changed
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(0))]);

		// When applying to a commission that is less than the min
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 11));
		// Then the commission is bumped to the min
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(5))]);

		// When applying commission to a validator that doesn't exist then storage is not altered
		assert_noop!(
			Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 420),
			Error::<Test>::NotStash
		);
	});
}

#[test]
fn proportional_slash_stop_slashing_if_remaining_zero() {
	ExtBuilder::default().nominate(true).build_and_execute(|| {
		let c = |era, value| UnlockChunk::<Balance> { era, value };

		// we have some chunks, but they are not affected.
		let unlocking = bounded_vec![c(1, 10), c(2, 10)];

		// Given
		let mut ledger = StakingLedger::<Test>::new(123, 20);
		ledger.total = 40;
		ledger.unlocking = unlocking;

		assert_eq!(BondingDuration::get(), 3);

		// should not slash more than the amount requested, by accidentally slashing the first
		// chunk.
		assert_eq!(ledger.slash(18, 1, 0), 18);
	});
}

#[test]
fn proportional_ledger_slash_works() {
	ExtBuilder::default().nominate(true).build_and_execute(|| {
		let c = |era, value| UnlockChunk::<Balance> { era, value };
		// Given
		let mut ledger = StakingLedger::<Test>::new(123, 10);
		assert_eq!(BondingDuration::get(), 3);

		// When we slash a ledger with no unlocking chunks
		assert_eq!(ledger.slash(5, 1, 0), 5);
		// Then
		assert_eq!(ledger.total, 5);
		assert_eq!(ledger.active, 5);
		assert_eq!(LedgerSlashPerEra::get().0, 5);
		assert_eq!(LedgerSlashPerEra::get().1, Default::default());

		// When we slash a ledger with no unlocking chunks and the slash amount is greater then the
		// total
		assert_eq!(ledger.slash(11, 1, 0), 5);
		// Then
		assert_eq!(ledger.total, 0);
		assert_eq!(ledger.active, 0);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(LedgerSlashPerEra::get().1, Default::default());

		// Given
		ledger.unlocking = bounded_vec![c(4, 10), c(5, 10)];
		ledger.total = 2 * 10;
		ledger.active = 0;
		// When all the chunks overlap with the slash eras
		assert_eq!(ledger.slash(20, 0, 0), 20);
		// Then
		assert_eq!(ledger.unlocking, vec![]);
		assert_eq!(ledger.total, 0);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(LedgerSlashPerEra::get().1, BTreeMap::from([(4, 0), (5, 0)]));

		// Given
		ledger.unlocking = bounded_vec![c(4, 100), c(5, 100), c(6, 100), c(7, 100)];
		ledger.total = 4 * 100;
		ledger.active = 0;
		// When the first 2 chunks don't overlap with the affected range of unlock eras.
		assert_eq!(ledger.slash(140, 0, 3), 140);
		// Then
		assert_eq!(ledger.unlocking, vec![c(4, 100), c(5, 100), c(6, 30), c(7, 30)]);
		assert_eq!(ledger.total, 4 * 100 - 140);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(LedgerSlashPerEra::get().1, BTreeMap::from([(6, 30), (7, 30)]));

		// Given
		ledger.unlocking = bounded_vec![c(4, 100), c(5, 100), c(6, 100), c(7, 100)];
		ledger.total = 4 * 100;
		ledger.active = 0;
		// When the first 2 chunks don't overlap with the affected range of unlock eras.
		assert_eq!(ledger.slash(15, 0, 3), 15);
		// Then
		assert_eq!(ledger.unlocking, vec![c(4, 100), c(5, 100), c(6, 100 - 8), c(7, 100 - 7)]);
		assert_eq!(ledger.total, 4 * 100 - 15);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(LedgerSlashPerEra::get().1, BTreeMap::from([(6, 92), (7, 93)]));

		// Given
		ledger.unlocking = bounded_vec![c(4, 40), c(5, 100), c(6, 10), c(7, 250)];
		ledger.active = 500;
		// 900
		ledger.total = 40 + 10 + 100 + 250 + 500;
		// When we have a partial slash that touches all chunks
		assert_eq!(ledger.slash(900 / 2, 0, 0), 450);
		// Then
		assert_eq!(ledger.active, 500 / 2);
		assert_eq!(
			ledger.unlocking,
			vec![c(4, 40 / 2), c(5, 100 / 2), c(6, 10 / 2), c(7, 250 / 2)]
		);
		assert_eq!(ledger.total, 900 / 2);
		assert_eq!(LedgerSlashPerEra::get().0, 500 / 2);
		assert_eq!(
			LedgerSlashPerEra::get().1,
			BTreeMap::from([(4, 40 / 2), (5, 100 / 2), (6, 10 / 2), (7, 250 / 2)])
		);

		// slash 1/4th with not chunk.
		ledger.unlocking = bounded_vec![];
		ledger.active = 500;
		ledger.total = 500;
		// When we have a partial slash that touches all chunks
		assert_eq!(ledger.slash(500 / 4, 0, 0), 500 / 4);
		// Then
		assert_eq!(ledger.active, 3 * 500 / 4);
		assert_eq!(ledger.unlocking, vec![]);
		assert_eq!(ledger.total, ledger.active);
		assert_eq!(LedgerSlashPerEra::get().0, 3 * 500 / 4);
		assert_eq!(LedgerSlashPerEra::get().1, Default::default());

		// Given we have the same as above,
		ledger.unlocking = bounded_vec![c(4, 40), c(5, 100), c(6, 10), c(7, 250)];
		ledger.active = 500;
		ledger.total = 40 + 10 + 100 + 250 + 500; // 900
		assert_eq!(ledger.total, 900);
		// When we have a higher min balance
		assert_eq!(
			ledger.slash(
				900 / 2,
				25, /* min balance - chunks with era 0 & 2 will be slashed to <=25, causing it
				     * to get swept */
				0
			),
			450
		);
		assert_eq!(ledger.active, 500 / 2);
		// the last chunk was not slashed 50% like all the rest, because some other earlier chunks
		// got dusted.
		assert_eq!(ledger.unlocking, vec![c(5, 100 / 2), c(7, 150)]);
		assert_eq!(ledger.total, 900 / 2);
		assert_eq!(LedgerSlashPerEra::get().0, 500 / 2);
		assert_eq!(
			LedgerSlashPerEra::get().1,
			BTreeMap::from([(4, 0), (5, 100 / 2), (6, 0), (7, 150)])
		);

		// Given
		// slash order --------------------NA--------2----------0----------1----
		ledger.unlocking = bounded_vec![c(4, 40), c(5, 100), c(6, 10), c(7, 250)];
		ledger.active = 500;
		ledger.total = 40 + 10 + 100 + 250 + 500; // 900
		assert_eq!(
			ledger.slash(
				500 + 10 + 250 + 100 / 2, // active + era 6 + era 7 + era 5 / 2
				0,
				3 /* slash era 6 first, so the affected parts are era 6, era 7 and
				   * ledge.active. This will cause the affected to go to zero, and then we will
				   * start slashing older chunks */
			),
			500 + 250 + 10 + 100 / 2
		);
		// Then
		assert_eq!(ledger.active, 0);
		assert_eq!(ledger.unlocking, vec![c(4, 40), c(5, 100 / 2)]);
		assert_eq!(ledger.total, 90);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(LedgerSlashPerEra::get().1, BTreeMap::from([(5, 100 / 2), (6, 0), (7, 0)]));

		// Given
		// iteration order------------------NA---------2----------0----------1----
		ledger.unlocking = bounded_vec![c(4, 100), c(5, 100), c(6, 100), c(7, 100)];
		ledger.active = 100;
		ledger.total = 5 * 100;
		// When
		assert_eq!(
			ledger.slash(
				351, // active + era 6 + era 7 + era 5 / 2 + 1
				50,  // min balance - everything slashed below 50 will get dusted
				3    /* slash era 3+3 first, so the affected parts are era 6, era 7 and
				      * ledge.active. This will cause the affected to go to zero, and then we
				      * will start slashing older chunks */
			),
			400
		);
		// Then
		assert_eq!(ledger.active, 0);
		assert_eq!(ledger.unlocking, vec![c(4, 100)]);
		assert_eq!(ledger.total, 100);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(LedgerSlashPerEra::get().1, BTreeMap::from([(5, 0), (6, 0), (7, 0)]));

		// Tests for saturating arithmetic

		// Given
		let slash = u64::MAX as Balance * 2;
		// The value of the other parts of ledger that will get slashed
		let value = slash - (10 * 4);

		ledger.active = 10;
		ledger.unlocking = bounded_vec![c(4, 10), c(5, 10), c(6, 10), c(7, value)];
		ledger.total = value + 40;
		// When
		let slash_amount = ledger.slash(slash, 0, 0);
		assert_eq_error_rate!(slash_amount, slash, 5);
		// Then
		assert_eq!(ledger.active, 0); // slash of 9
		assert_eq!(ledger.unlocking, vec![]);
		assert_eq!(ledger.total, 0);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(LedgerSlashPerEra::get().1, BTreeMap::from([(4, 0), (5, 0), (6, 0), (7, 0)]));

		// Given
		use sp_runtime::PerThing as _;
		let slash = u64::MAX as Balance * 2;
		let value = u64::MAX as Balance * 2;
		let unit = 100;
		// slash * value that will saturate
		assert!(slash.checked_mul(value).is_none());
		// but slash * unit won't.
		assert!(slash.checked_mul(unit).is_some());
		ledger.unlocking = bounded_vec![c(4, unit), c(5, value), c(6, unit), c(7, unit)];
		//--------------------------------------note value^^^
		ledger.active = unit;
		ledger.total = unit * 4 + value;
		// When
		assert_eq!(ledger.slash(slash, 0, 0), slash);
		// Then
		// The amount slashed out of `unit`
		let affected_balance = value + unit * 4;
		let ratio = Perquintill::from_rational_with_rounding(slash, affected_balance, Rounding::Up)
			.unwrap();
		// `unit` after the slash is applied
		let unit_slashed = {
			let unit_slash = ratio.mul_ceil(unit);
			unit - unit_slash
		};
		let value_slashed = {
			let value_slash = ratio.mul_ceil(value);
			value - value_slash
		};
		assert_eq!(ledger.active, unit_slashed);
		assert_eq!(ledger.unlocking, vec![c(5, value_slashed), c(7, 32)]);
		assert_eq!(ledger.total, value_slashed + 32);
		assert_eq!(LedgerSlashPerEra::get().0, 0);
		assert_eq!(
			LedgerSlashPerEra::get().1,
			BTreeMap::from([(4, 0), (5, value_slashed), (6, 0), (7, 32)])
		);
	});
}

#[test]
fn reducing_max_unlocking_chunks_abrupt() {
	// Concern is on validators only
	// By Default 11, 10 are stash and ctlr and 21,20
	ExtBuilder::default().build_and_execute(|| {
		// given a staker at era=10 and MaxUnlockChunks set to 2
		MaxUnlockingChunks::set(2);
		start_active_era(10);
		assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 300, RewardDestination::Staked));
		assert!(matches!(Staking::ledger(3.into()), Ok(_)));

		// when staker unbonds
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 20));

		// then an unlocking chunk is added at `current_era + bonding_duration`
		// => 10 + 3 = 13
		let expected_unlocking: BoundedVec<UnlockChunk<Balance>, MaxUnlockingChunks> =
			bounded_vec![UnlockChunk { value: 20 as Balance, era: 13 as EraIndex }];
		assert!(matches!(Staking::ledger(3.into()),
			Ok(StakingLedger {
				unlocking,
				..
			}) if unlocking==expected_unlocking));

		// when staker unbonds at next era
		start_active_era(11);
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 50));
		// then another unlock chunk is added
		let expected_unlocking: BoundedVec<UnlockChunk<Balance>, MaxUnlockingChunks> =
			bounded_vec![UnlockChunk { value: 20, era: 13 }, UnlockChunk { value: 50, era: 14 }];
		assert!(matches!(Staking::ledger(3.into()),
			Ok(StakingLedger {
				unlocking,
				..
			}) if unlocking==expected_unlocking));

		// when staker unbonds further
		start_active_era(12);
		// then further unbonding not possible
		assert_noop!(Staking::unbond(RuntimeOrigin::signed(3), 20), Error::<Test>::NoMoreChunks);

		// when max unlocking chunks is reduced abruptly to a low value
		MaxUnlockingChunks::set(1);
		// then unbond, rebond ops are blocked with ledger in corrupt state
		assert_noop!(Staking::unbond(RuntimeOrigin::signed(3), 20), Error::<Test>::NotController);
		assert_noop!(Staking::rebond(RuntimeOrigin::signed(3), 100), Error::<Test>::NotController);

		// reset the ledger corruption
		MaxUnlockingChunks::set(2);
	})
}

#[test]
fn cannot_set_unsupported_validator_count() {
	ExtBuilder::default().build_and_execute(|| {
		MaxWinners::set(50);
		// set validator count works
		assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 30));
		assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 50));
		// setting validator count above 100 does not work
		assert_noop!(
			Staking::set_validator_count(RuntimeOrigin::root(), 51),
			Error::<Test>::TooManyValidators,
		);
	})
}

#[test]
fn increase_validator_count_errors() {
	ExtBuilder::default().build_and_execute(|| {
		MaxWinners::set(50);
		assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 40));

		// increase works
		assert_ok!(Staking::increase_validator_count(RuntimeOrigin::root(), 6));
		assert_eq!(ValidatorCount::<Test>::get(), 46);

		// errors
		assert_noop!(
			Staking::increase_validator_count(RuntimeOrigin::root(), 5),
			Error::<Test>::TooManyValidators,
		);
	})
}

#[test]
fn scale_validator_count_errors() {
	ExtBuilder::default().build_and_execute(|| {
		MaxWinners::set(50);
		assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 20));

		// scale value works
		assert_ok!(Staking::scale_validator_count(
			RuntimeOrigin::root(),
			Percent::from_percent(200)
		));
		assert_eq!(ValidatorCount::<Test>::get(), 40);

		// errors
		assert_noop!(
			Staking::scale_validator_count(RuntimeOrigin::root(), Percent::from_percent(126)),
			Error::<Test>::TooManyValidators,
		);
	})
}

#[test]
fn set_min_commission_works_with_admin_origin() {
	ExtBuilder::default().build_and_execute(|| {
		// no minimum commission set initially
		assert_eq!(MinCommission::<Test>::get(), Zero::zero());

		// root can set min commission
		assert_ok!(Staking::set_min_commission(RuntimeOrigin::root(), Perbill::from_percent(10)));

		assert_eq!(MinCommission::<Test>::get(), Perbill::from_percent(10));

		// Non privileged origin can not set min_commission
		assert_noop!(
			Staking::set_min_commission(RuntimeOrigin::signed(2), Perbill::from_percent(15)),
			BadOrigin
		);

		// Admin Origin can set min commission
		assert_ok!(Staking::set_min_commission(
			RuntimeOrigin::signed(1),
			Perbill::from_percent(15),
		));

		// setting commission below min_commission fails
		assert_noop!(
			Staking::validate(
				RuntimeOrigin::signed(11),
				ValidatorPrefs { commission: Perbill::from_percent(14), blocked: false }
			),
			Error::<Test>::CommissionTooLow
		);

		// setting commission >= min_commission works
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(15), blocked: false }
		));
	})
}

#[test]
fn can_page_exposure() {
	let mut others: Vec<IndividualExposure<AccountId, Balance>> = vec![];
	let mut total_stake: Balance = 0;
	// 19 nominators
	for i in 1..20 {
		let individual_stake: Balance = 100 * i as Balance;
		others.push(IndividualExposure { who: i, value: individual_stake });
		total_stake += individual_stake;
	}
	let own_stake: Balance = 500;
	total_stake += own_stake;
	assert_eq!(total_stake, 19_500);
	// build full exposure set
	let exposure: Exposure<AccountId, Balance> =
		Exposure { total: total_stake, own: own_stake, others };

	// when
	let (exposure_metadata, exposure_page): (
		PagedExposureMetadata<Balance>,
		Vec<ExposurePage<AccountId, Balance>>,
	) = exposure.clone().into_pages(3);

	// then
	// 7 pages of nominators.
	assert_eq!(exposure_page.len(), 7);
	assert_eq!(exposure_metadata.page_count, 7);
	// first page stake = 100 + 200 + 300
	assert!(matches!(exposure_page[0], ExposurePage { page_total: 600, .. }));
	// second page stake = 0 + 400 + 500 + 600
	assert!(matches!(exposure_page[1], ExposurePage { page_total: 1500, .. }));
	// verify overview has the total
	assert_eq!(exposure_metadata.total, 19_500);
	// verify total stake is same as in the original exposure.
	assert_eq!(
		exposure_page.iter().map(|a| a.page_total).reduce(|a, b| a + b).unwrap(),
		19_500 - exposure_metadata.own
	);
	// verify own stake is correct
	assert_eq!(exposure_metadata.own, 500);
	// verify number of nominators are same as in the original exposure.
	assert_eq!(exposure_page.iter().map(|a| a.others.len()).reduce(|a, b| a + b).unwrap(), 19);
	assert_eq!(exposure_metadata.nominator_count, 19);
}

#[test]
fn should_retain_era_info_only_upto_history_depth() {
	ExtBuilder::default().build_and_execute(|| {
		// remove existing exposure
		Pallet::<Test>::clear_era_information(0);
		let validator_stash = 10;

		for era in 0..4 {
			ClaimedRewards::<Test>::insert(era, &validator_stash, vec![0, 1, 2]);
			for page in 0..3 {
				ErasStakersPaged::<Test>::insert(
					(era, &validator_stash, page),
					ExposurePage { page_total: 100, others: vec![] },
				);
			}
		}

		for i in 0..4 {
			// Count of entries remaining in ClaimedRewards = total - cleared_count
			assert_eq!(ClaimedRewards::<Test>::iter().count(), (4 - i));
			// 1 claimed_rewards entry for each era
			assert_eq!(ClaimedRewards::<Test>::iter_prefix(i as EraIndex).count(), 1);
			// 3 entries (pages) for each era
			assert_eq!(ErasStakersPaged::<Test>::iter_prefix((i as EraIndex,)).count(), 3);

			// when clear era info
			Pallet::<Test>::clear_era_information(i as EraIndex);

			// then all era entries are cleared
			assert_eq!(ClaimedRewards::<Test>::iter_prefix(i as EraIndex).count(), 0);
			assert_eq!(ErasStakersPaged::<Test>::iter_prefix((i as EraIndex,)).count(), 0);
		}
	});
}

#[test]
fn test_legacy_claimed_rewards_is_checked_at_reward_payout() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		// Create a validator:
		bond_validator(11, 1000);

		// reward validator for next 2 eras
		mock::start_active_era(1);
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
		mock::start_active_era(2);
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);
		mock::start_active_era(3);

		//verify rewards are not claimed
		assert_eq!(
			EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
				1,
				Staking::ledger(11.into()).as_ref().unwrap(),
				&11,
				0
			),
			false
		);
		assert_eq!(
			EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
				2,
				Staking::ledger(11.into()).as_ref().unwrap(),
				&11,
				0
			),
			false
		);

		// assume reward claim for era 1 was stored in legacy storage
		Ledger::<Test>::insert(
			11,
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![1],
			},
		);

		// verify rewards for era 1 cannot be claimed
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 0),
			Error::<Test>::AlreadyClaimed
				.with_weight(<Test as Config>::WeightInfo::payout_stakers_alive_staked(0)),
		);
		assert_eq!(
			EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
				1,
				Staking::ledger(11.into()).as_ref().unwrap(),
				&11,
				0
			),
			true
		);

		// verify rewards for era 2 can be claimed
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0));
		assert_eq!(
			EraInfo::<Test>::is_rewards_claimed_with_legacy_fallback(
				2,
				Staking::ledger(11.into()).as_ref().unwrap(),
				&11,
				0
			),
			true
		);
		// but the new claimed rewards for era 2 is not stored in legacy storage
		assert_eq!(
			Ledger::<Test>::get(11).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![1],
			},
		);
		// instead it is kept in `ClaimedRewards`
		assert_eq!(ClaimedRewards::<Test>::get(2, 11), vec![0]);
	});
}

#[test]
fn test_validator_exposure_is_backward_compatible_with_non_paged_rewards_payout() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		// case 1: exposure exist in clipped.
		// set page cap to 10
		MaxExposurePageSize::set(10);
		bond_validator(11, 1000);
		let mut expected_individual_exposures: Vec<IndividualExposure<AccountId, Balance>> = vec![];
		let mut total_exposure: Balance = 0;
		// 1st exposure page
		for i in 0..10 {
			let who = 1000 + i;
			let value = 1000 + i as Balance;
			bond_nominator(who, value, vec![11]);
			expected_individual_exposures.push(IndividualExposure { who, value });
			total_exposure += value;
		}

		for i in 10..15 {
			let who = 1000 + i;
			let value = 1000 + i as Balance;
			bond_nominator(who, value, vec![11]);
			expected_individual_exposures.push(IndividualExposure { who, value });
			total_exposure += value;
		}

		mock::start_active_era(1);
		// reward validator for current era
		Pallet::<Test>::reward_by_ids(vec![(11, 1)]);

		// start new era
		mock::start_active_era(2);
		// verify exposure for era 1 is stored in paged storage, that each exposure is stored in
		// one and only one page, and no exposure is repeated.
		let actual_exposure_page_0 = ErasStakersPaged::<Test>::get((1, 11, 0)).unwrap();
		let actual_exposure_page_1 = ErasStakersPaged::<Test>::get((1, 11, 1)).unwrap();
		expected_individual_exposures.iter().for_each(|exposure| {
			assert!(
				actual_exposure_page_0.others.contains(exposure) ||
					actual_exposure_page_1.others.contains(exposure)
			);
		});
		assert_eq!(
			expected_individual_exposures.len(),
			actual_exposure_page_0.others.len() + actual_exposure_page_1.others.len()
		);
		// verify `EraInfo` returns page from paged storage
		assert_eq!(
			EraInfo::<Test>::get_paged_exposure(1, &11, 0).unwrap().others(),
			&actual_exposure_page_0.others
		);
		assert_eq!(
			EraInfo::<Test>::get_paged_exposure(1, &11, 1).unwrap().others(),
			&actual_exposure_page_1.others
		);
		assert_eq!(EraInfo::<Test>::get_page_count(1, &11), 2);

		// validator is exposed
		assert!(<Staking as sp_staking::StakingInterface>::is_exposed_in_era(&11, &1));
		// nominators are exposed
		for i in 10..15 {
			let who: AccountId = 1000 + i;
			assert!(<Staking as sp_staking::StakingInterface>::is_exposed_in_era(&who, &1));
		}

		// case 2: exposure exist in ErasStakers and ErasStakersClipped (legacy).
		// delete paged storage and add exposure to clipped storage
		<ErasStakersPaged<Test>>::remove((1, 11, 0));
		<ErasStakersPaged<Test>>::remove((1, 11, 1));
		<ErasStakersOverview<Test>>::remove(1, 11);

		<ErasStakers<Test>>::insert(
			1,
			11,
			Exposure {
				total: total_exposure,
				own: 1000,
				others: expected_individual_exposures.clone(),
			},
		);
		let mut clipped_exposure = expected_individual_exposures.clone();
		clipped_exposure.sort_by(|a, b| b.who.cmp(&a.who));
		clipped_exposure.truncate(10);
		<ErasStakersClipped<Test>>::insert(
			1,
			11,
			Exposure { total: total_exposure, own: 1000, others: clipped_exposure.clone() },
		);

		// verify `EraInfo` returns exposure from clipped storage
		let actual_exposure_paged = EraInfo::<Test>::get_paged_exposure(1, &11, 0).unwrap();
		assert_eq!(actual_exposure_paged.others(), &clipped_exposure);
		assert_eq!(actual_exposure_paged.own(), 1000);
		assert_eq!(actual_exposure_paged.exposure_metadata.page_count, 1);

		let actual_exposure_full = EraInfo::<Test>::get_full_exposure(1, &11);
		assert_eq!(actual_exposure_full.others, expected_individual_exposures);
		assert_eq!(actual_exposure_full.own, 1000);
		assert_eq!(actual_exposure_full.total, total_exposure);

		// validator is exposed
		assert!(<Staking as sp_staking::StakingInterface>::is_exposed_in_era(&11, &1));
		// nominators are exposed
		for i in 10..15 {
			let who: AccountId = 1000 + i;
			assert!(<Staking as sp_staking::StakingInterface>::is_exposed_in_era(&who, &1));
		}

		// for pages other than 0, clipped storage returns empty exposure
		assert_eq!(EraInfo::<Test>::get_paged_exposure(1, &11, 1), None);
		// page size is 1 for clipped storage
		assert_eq!(EraInfo::<Test>::get_page_count(1, &11), 1);

		// payout for page 0 works
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 0, 0));
		// payout for page 1 fails
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 0, 1),
			Error::<Test>::InvalidPage
				.with_weight(<Test as Config>::WeightInfo::payout_stakers_alive_staked(0))
		);
	});
}

#[test]
fn test_runtime_api_pending_rewards() {
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN
		let err_weight = <Test as Config>::WeightInfo::payout_stakers_alive_staked(0);
		let stake = 100;

		// validator with non-paged exposure, rewards marked in legacy claimed rewards.
		let validator_one = 301;
		// validator with non-paged exposure, rewards marked in paged claimed rewards.
		let validator_two = 302;
		// validator with paged exposure.
		let validator_three = 303;

		// Set staker
		for v in validator_one..=validator_three {
			let _ = asset::set_stakeable_balance::<Test>(&v, stake);
			assert_ok!(Staking::bond(RuntimeOrigin::signed(v), stake, RewardDestination::Staked));
		}

		// Add reward points
		let reward = EraRewardPoints::<AccountId> {
			total: 1,
			individual: vec![(validator_one, 1), (validator_two, 1), (validator_three, 1)]
				.into_iter()
				.collect(),
		};
		ErasRewardPoints::<Test>::insert(0, reward);

		// build exposure
		let mut individual_exposures: Vec<IndividualExposure<AccountId, Balance>> = vec![];
		for i in 0..=MaxExposurePageSize::get() {
			individual_exposures.push(IndividualExposure { who: i.into(), value: stake });
		}
		let exposure = Exposure::<AccountId, Balance> {
			total: stake * (MaxExposurePageSize::get() as Balance + 2),
			own: stake,
			others: individual_exposures,
		};

		// add non-paged exposure for one and two.
		<ErasStakers<Test>>::insert(0, validator_one, exposure.clone());
		<ErasStakers<Test>>::insert(0, validator_two, exposure.clone());
		// add paged exposure for third validator
		EraInfo::<Test>::set_exposure(0, &validator_three, exposure);

		// add some reward to be distributed
		ErasValidatorReward::<Test>::insert(0, 1000);

		// mark rewards claimed for validator_one in legacy claimed rewards
		<Ledger<Test>>::insert(
			validator_one,
			StakingLedgerInspect {
				stash: validator_one,
				total: stake,
				active: stake,
				unlocking: Default::default(),
				legacy_claimed_rewards: bounded_vec![0],
			},
		);

		// SCENARIO ONE: rewards already marked claimed in legacy storage.
		// runtime api should return false for pending rewards for validator_one.
		assert!(!EraInfo::<Test>::pending_rewards(0, &validator_one));
		// and if we try to pay, we get an error.
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_one, 0),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		// SCENARIO TWO: non-paged exposure
		// validator two has not claimed rewards, so pending rewards is true.
		assert!(EraInfo::<Test>::pending_rewards(0, &validator_two));
		// and payout works
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_two, 0));
		// now pending rewards is false.
		assert!(!EraInfo::<Test>::pending_rewards(0, &validator_two));
		// and payout fails
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_two, 0),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		// SCENARIO THREE: validator with paged exposure (two pages).
		// validator three has not claimed rewards, so pending rewards is true.
		assert!(EraInfo::<Test>::pending_rewards(0, &validator_three));
		// and payout works
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_three, 0));
		// validator three has two pages of exposure, so pending rewards is still true.
		assert!(EraInfo::<Test>::pending_rewards(0, &validator_three));
		// payout again
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_three, 0));
		// now pending rewards is false.
		assert!(!EraInfo::<Test>::pending_rewards(0, &validator_three));
		// and payout fails
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_three, 0),
			Error::<Test>::AlreadyClaimed.with_weight(err_weight)
		);

		// for eras with no exposure, pending rewards is false.
		assert!(!EraInfo::<Test>::pending_rewards(0, &validator_one));
		assert!(!EraInfo::<Test>::pending_rewards(0, &validator_two));
		assert!(!EraInfo::<Test>::pending_rewards(0, &validator_three));
	});
}

mod staking_interface {
	use frame_support::storage::with_storage_layer;
	use sp_staking::StakingInterface;

	use super::*;

	#[test]
	fn force_unstake_with_slash_works() {
		ExtBuilder::default().build_and_execute(|| {
			// without slash
			let _ = with_storage_layer::<(), _, _>(|| {
				// bond an account, can unstake
				assert_eq!(Staking::bonded(&11), Some(11));
				assert_ok!(<Staking as StakingInterface>::force_unstake(11));
				Err(DispatchError::from("revert"))
			});

			// bond again and add a slash, still can unstake.
			assert_eq!(Staking::bonded(&11), Some(11));
			add_slash(&11);
			assert_ok!(<Staking as StakingInterface>::force_unstake(11));
		});
	}

	#[test]
	fn do_withdraw_unbonded_with_wrong_slash_spans_works_as_expected() {
		ExtBuilder::default().build_and_execute(|| {
			on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(100)]);

			assert_eq!(Staking::bonded(&11), Some(11));

			assert_noop!(
				Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0),
				Error::<Test>::IncorrectSlashingSpans
			);

			let num_slashing_spans =
				SlashingSpans::<Test>::get(&11).map_or(0, |s| s.iter().count());
			assert_ok!(Staking::withdraw_unbonded(
				RuntimeOrigin::signed(11),
				num_slashing_spans as u32
			));
		});
	}

	#[test]
	fn do_withdraw_unbonded_can_kill_stash_with_existential_deposit_zero() {
		ExtBuilder::default()
			.existential_deposit(0)
			.nominate(false)
			.build_and_execute(|| {
				// Initial state of 11
				assert_eq!(Staking::bonded(&11), Some(11));
				assert_eq!(
					Staking::ledger(11.into()).unwrap(),
					StakingLedgerInspect {
						stash: 11,
						total: 1000,
						active: 1000,
						unlocking: Default::default(),
						legacy_claimed_rewards: bounded_vec![],
					}
				);
				assert_eq!(
					Staking::eras_stakers(active_era(), &11),
					Exposure { total: 1000, own: 1000, others: vec![] }
				);

				// Unbond all of the funds in stash.
				Staking::chill(RuntimeOrigin::signed(11)).unwrap();
				Staking::unbond(RuntimeOrigin::signed(11), 1000).unwrap();
				assert_eq!(
					Staking::ledger(11.into()).unwrap(),
					StakingLedgerInspect {
						stash: 11,
						total: 1000,
						active: 0,
						unlocking: bounded_vec![UnlockChunk { value: 1000, era: 3 }],
						legacy_claimed_rewards: bounded_vec![],
					},
				);

				// trigger future era.
				mock::start_active_era(3);

				// withdraw unbonded
				assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));

				// empty stash has been reaped
				assert!(!<Ledger<Test>>::contains_key(&11));
				assert!(!<Bonded<Test>>::contains_key(&11));
				assert!(!<Validators<Test>>::contains_key(&11));
				assert!(!<Payee<Test>>::contains_key(&11));
				// lock is removed.
				assert_eq!(asset::staked::<Test>(&11), 0);
			});
	}

	#[test]
	fn status() {
		ExtBuilder::default().build_and_execute(|| {
			// stash of a validator is identified as a validator
			assert_eq!(Staking::status(&11).unwrap(), StakerStatus::Validator);
			// .. but not the controller.
			assert!(Staking::status(&10).is_err());

			// stash of nominator is identified as a nominator
			assert_eq!(Staking::status(&101).unwrap(), StakerStatus::Nominator(vec![11, 21]));
			// .. but not the controller.
			assert!(Staking::status(&100).is_err());

			// stash of chilled is identified as a chilled
			assert_eq!(Staking::status(&41).unwrap(), StakerStatus::Idle);
			// .. but not the controller.
			assert!(Staking::status(&40).is_err());

			// random other account.
			assert!(Staking::status(&42).is_err());
		})
	}
}

mod staking_unchecked {
	use sp_staking::{Stake, StakingInterface, StakingUnchecked};

	use super::*;

	#[test]
	fn virtual_bond_does_not_lock() {
		ExtBuilder::default().build_and_execute(|| {
			mock::start_active_era(1);
			assert_eq!(asset::total_balance::<Test>(&10), 1);
			// 10 can bond more than its balance amount since we do not require lock for virtual
			// bonding.
			assert_ok!(<Staking as StakingUnchecked>::virtual_bond(&10, 100, &15));
			// nothing is locked on 10.
			assert_eq!(asset::staked::<Test>(&10), 0);
			// adding more balance does not lock anything as well.
			assert_ok!(<Staking as StakingInterface>::bond_extra(&10, 1000));
			// but ledger is updated correctly.
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 1100, active: 1100 })
			);

			// lets try unbonding some amount.
			assert_ok!(<Staking as StakingInterface>::unbond(&10, 200));
			assert_eq!(
				Staking::ledger(10.into()).unwrap(),
				StakingLedgerInspect {
					stash: 10,
					total: 1100,
					active: 1100 - 200,
					unlocking: bounded_vec![UnlockChunk { value: 200, era: 1 + 3 }],
					legacy_claimed_rewards: bounded_vec![],
				}
			);

			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 1100, active: 900 })
			);
			// still no locks.
			assert_eq!(asset::staked::<Test>(&10), 0);

			mock::start_active_era(2);
			// cannot withdraw without waiting for unbonding period.
			assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(10, 0));
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 1100, active: 900 })
			);

			// in era 4, 10 can withdraw unlocking amount.
			mock::start_active_era(4);
			assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(10, 0));
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 900, active: 900 })
			);

			// unbond all.
			assert_ok!(<Staking as StakingInterface>::unbond(&10, 900));
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 900, active: 0 })
			);
			mock::start_active_era(7);
			assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(10, 0));

			// ensure withdrawing all amount cleans up storage.
			assert_eq!(Staking::ledger(10.into()), Err(Error::<Test>::NotStash));
			assert_eq!(VirtualStakers::<Test>::contains_key(10), false);
		})
	}

	#[test]
	fn virtual_staker_cannot_pay_reward_to_self_account() {
		ExtBuilder::default().build_and_execute(|| {
			// cannot set payee to self
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&10, 100, &10),
				Error::<Test>::RewardDestinationRestricted
			);

			// to another account works
			assert_ok!(<Staking as StakingUnchecked>::virtual_bond(&10, 100, &11));

			// cannot set via set_payee as well.
			assert_noop!(
				<Staking as StakingInterface>::set_payee(&10, &10),
				Error::<Test>::RewardDestinationRestricted
			);
		});
	}

	#[test]
	fn virtual_staker_cannot_bond_again() {
		ExtBuilder::default().build_and_execute(|| {
			// 200 virtual bonds
			bond_virtual_nominator(200, 201, 500, vec![11, 21]);

			// Tries bonding again
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&200, 200, &201),
				Error::<Test>::AlreadyBonded
			);

			// And again with a different reward destination.
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&200, 200, &202),
				Error::<Test>::AlreadyBonded
			);

			// Direct bond is not allowed as well.
			assert_noop!(
				<Staking as StakingInterface>::bond(&200, 200, &202),
				Error::<Test>::AlreadyBonded
			);
		});
	}

	#[test]
	fn normal_staker_cannot_virtual_bond() {
		ExtBuilder::default().build_and_execute(|| {
			// 101 is a nominator trying to virtual bond
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&101, 200, &102),
				Error::<Test>::AlreadyBonded
			);

			// validator 21 tries to virtual bond
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&21, 200, &22),
				Error::<Test>::AlreadyBonded
			);
		});
	}

	#[test]
	fn migrate_virtual_staker() {
		ExtBuilder::default().build_and_execute(|| {
			// give some balance to 200
			asset::set_stakeable_balance::<Test>(&200, 2000);

			// stake
			assert_ok!(Staking::bond(RuntimeOrigin::signed(200), 1000, RewardDestination::Staked));
			assert_eq!(asset::staked::<Test>(&200), 1000);

			// migrate them to virtual staker
			assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&200));
			// payee needs to be updated to a non-stash account.
			assert_ok!(<Staking as StakingInterface>::set_payee(&200, &201));

			// ensure the balance is not locked anymore
			assert_eq!(asset::staked::<Test>(&200), 0);

			// and they are marked as virtual stakers
			assert_eq!(Pallet::<Test>::is_virtual_staker(&200), true);
		});
	}

	#[test]
	fn virtual_nominators_are_lazily_slashed() {
		ExtBuilder::default()
			.validator_count(7)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.set_status(201, StakerStatus::Validator)
			.set_status(202, StakerStatus::Validator)
			.build_and_execute(|| {
				mock::start_active_era(1);
				let slash_percent = Perbill::from_percent(5);
				let initial_exposure = Staking::eras_stakers(active_era(), &11);
				// 101 is a nominator for 11
				assert_eq!(initial_exposure.others.first().unwrap().who, 101);
				// make 101 a virtual nominator
				assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&101));
				// set payee different to self.
				assert_ok!(<Staking as StakingInterface>::set_payee(&101, &102));

				// cache values
				let nominator_stake = Staking::ledger(101.into()).unwrap().active;
				let nominator_balance = balances(&101).0;
				let validator_stake = Staking::ledger(11.into()).unwrap().active;
				let validator_balance = balances(&11).0;
				let exposed_stake = initial_exposure.total;
				let exposed_validator = initial_exposure.own;
				let exposed_nominator = initial_exposure.others.first().unwrap().value;

				// 11 goes offline
				on_offence_now(&[offence_from(11, None)], &[slash_percent]);

				let slash_amount = slash_percent * exposed_stake;
				let validator_share =
					Perbill::from_rational(exposed_validator, exposed_stake) * slash_amount;
				let nominator_share =
					Perbill::from_rational(exposed_nominator, exposed_stake) * slash_amount;

				// both slash amounts need to be positive for the test to make sense.
				assert!(validator_share > 0);
				assert!(nominator_share > 0);

				// both stakes must have been decreased pro-rata.
				assert_eq!(
					Staking::ledger(101.into()).unwrap().active,
					nominator_stake - nominator_share
				);
				assert_eq!(
					Staking::ledger(11.into()).unwrap().active,
					validator_stake - validator_share
				);

				// validator balance is slashed as usual
				assert_eq!(balances(&11).0, validator_balance - validator_share);
				// Because slashing happened.
				assert!(is_disabled(11));

				// but virtual nominator's balance is not slashed.
				assert_eq!(asset::stakeable_balance::<Test>(&101), nominator_balance);
				// but slash is broadcasted to slash observers.
				assert_eq!(SlashObserver::get().get(&101).unwrap(), &nominator_share);
			})
	}

	#[test]
	fn virtual_stakers_cannot_be_reaped() {
		ExtBuilder::default()
			// we need enough validators such that disables are allowed.
			.validator_count(7)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.set_status(201, StakerStatus::Validator)
			.set_status(202, StakerStatus::Validator)
			.build_and_execute(|| {
				// make 101 only nominate 11.
				assert_ok!(Staking::nominate(RuntimeOrigin::signed(101), vec![11]));

				mock::start_active_era(1);

				// slash all stake.
				let slash_percent = Perbill::from_percent(100);
				let initial_exposure = Staking::eras_stakers(active_era(), &11);
				// 101 is a nominator for 11
				assert_eq!(initial_exposure.others.first().unwrap().who, 101);
				// make 101 a virtual nominator
				assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&101));
				// set payee different to self.
				assert_ok!(<Staking as StakingInterface>::set_payee(&101, &102));

				// cache values
				let validator_balance = asset::stakeable_balance::<Test>(&11);
				let validator_stake = Staking::ledger(11.into()).unwrap().total;
				let nominator_balance = asset::stakeable_balance::<Test>(&101);
				let nominator_stake = Staking::ledger(101.into()).unwrap().total;

				// 11 goes offline
				on_offence_now(&[offence_from(11, None)], &[slash_percent]);

				// both stakes must have been decreased to 0.
				assert_eq!(Staking::ledger(101.into()).unwrap().active, 0);
				assert_eq!(Staking::ledger(11.into()).unwrap().active, 0);

				// all validator stake is slashed
				assert_eq_error_rate!(
					validator_balance - validator_stake,
					asset::stakeable_balance::<Test>(&11),
					1
				);
				// Because slashing happened.
				assert!(is_disabled(11));

				// Virtual nominator's balance is not slashed.
				assert_eq!(asset::stakeable_balance::<Test>(&101), nominator_balance);
				// Slash is broadcasted to slash observers.
				assert_eq!(SlashObserver::get().get(&101).unwrap(), &nominator_stake);

				// validator can be reaped.
				assert_ok!(Staking::reap_stash(RuntimeOrigin::signed(10), 11, u32::MAX));
				// nominator is a virtual staker and cannot be reaped.
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(10), 101, u32::MAX),
					Error::<Test>::VirtualStakerNotAllowed
				);
			})
	}

	#[test]
	fn restore_ledger_not_allowed_for_virtual_stakers() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);
			// 333 is corrupted
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// migrate to virtual staker.
			assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&333));

			// recover the ledger won't work for virtual staker
			assert_noop!(
				Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None),
				Error::<Test>::VirtualStakerNotAllowed
			);

			// migrate 333 back to normal staker
			<VirtualStakers<Test>>::remove(333);

			// try restore again
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));
		})
	}
}
mod ledger {
	use super::*;

	#[test]
	fn paired_account_works() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(10),
				100,
				RewardDestination::Account(10)
			));

			assert_eq!(<Bonded<Test>>::get(&10), Some(10));
			assert_eq!(
				StakingLedger::<Test>::paired_account(StakingAccount::Controller(10)),
				Some(10)
			);
			assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(10)), Some(10));

			assert_eq!(<Bonded<Test>>::get(&42), None);
			assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Controller(42)), None);
			assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(42)), None);

			// bond manually stash with different controller. This is deprecated but the migration
			// has not been complete yet (controller: 100, stash: 200)
			assert_ok!(bond_controller_stash(100, 200));
			assert_eq!(<Bonded<Test>>::get(&200), Some(100));
			assert_eq!(
				StakingLedger::<Test>::paired_account(StakingAccount::Controller(100)),
				Some(200)
			);
			assert_eq!(
				StakingLedger::<Test>::paired_account(StakingAccount::Stash(200)),
				Some(100)
			);
		})
	}

	#[test]
	fn get_ledger_works() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			// stash does not exist
			assert!(StakingLedger::<Test>::get(StakingAccount::Stash(42)).is_err());

			// bonded and paired
			assert_eq!(<Bonded<Test>>::get(&11), Some(11));

			match StakingLedger::<Test>::get(StakingAccount::Stash(11)) {
				Ok(ledger) => {
					assert_eq!(ledger.controller(), Some(11));
					assert_eq!(ledger.stash, 11);
				},
				Err(_) => panic!("staking ledger must exist"),
			};

			// bond manually stash with different controller. This is deprecated but the migration
			// has not been complete yet (controller: 100, stash: 200)
			assert_ok!(bond_controller_stash(100, 200));
			assert_eq!(<Bonded<Test>>::get(&200), Some(100));

			match StakingLedger::<Test>::get(StakingAccount::Stash(200)) {
				Ok(ledger) => {
					assert_eq!(ledger.controller(), Some(100));
					assert_eq!(ledger.stash, 200);
				},
				Err(_) => panic!("staking ledger must exist"),
			};

			match StakingLedger::<Test>::get(StakingAccount::Controller(100)) {
				Ok(ledger) => {
					assert_eq!(ledger.controller(), Some(100));
					assert_eq!(ledger.stash, 200);
				},
				Err(_) => panic!("staking ledger must exist"),
			};
		})
	}

	#[test]
	fn get_ledger_bad_state_fails() {
		ExtBuilder::default().has_stakers(false).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// Case 1: double bonded but not corrupted:
			// stash 444 has controller 555:
			assert_eq!(Bonded::<Test>::get(444), Some(555));
			assert_eq!(Ledger::<Test>::get(555).unwrap().stash, 444);

			// stash 444 is also a controller of 333:
			assert_eq!(Bonded::<Test>::get(333), Some(444));
			assert_eq!(
				StakingLedger::<Test>::paired_account(StakingAccount::Stash(333)),
				Some(444)
			);
			assert_eq!(Ledger::<Test>::get(444).unwrap().stash, 333);

			// although 444 is double bonded (it is a controller and a stash of different ledgers),
			// we can safely retrieve the ledger and mutate it since the correct ledger is
			// returned.
			let ledger_result = StakingLedger::<Test>::get(StakingAccount::Stash(444));
			assert_eq!(ledger_result.unwrap().stash, 444); // correct ledger.

			let ledger_result = StakingLedger::<Test>::get(StakingAccount::Controller(444));
			assert_eq!(ledger_result.unwrap().stash, 333); // correct ledger.

			// fetching ledger 333 by its stash works.
			let ledger_result = StakingLedger::<Test>::get(StakingAccount::Stash(333));
			assert_eq!(ledger_result.unwrap().stash, 333);

			// Case 2: corrupted ledger bonding.
			// in this case, we simulate what happens when fetching a ledger by stash returns a
			// ledger with a different stash. when this happens, we return an error instead of the
			// ledger to prevent ledger mutations.
			let mut ledger = Ledger::<Test>::get(444).unwrap();
			assert_eq!(ledger.stash, 333);
			ledger.stash = 444;
			Ledger::<Test>::insert(444, ledger);

			// now, we are prevented from fetching the ledger by stash from 1. It's associated
			// controller (2) is now bonding a ledger with a different stash (2, not 1).
			assert!(StakingLedger::<Test>::get(StakingAccount::Stash(333)).is_err());
		})
	}

	#[test]
	fn bond_works() {
		ExtBuilder::default().build_and_execute(|| {
			assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Stash(42)));
			assert!(<Bonded<Test>>::get(&42).is_none());

			let mut ledger: StakingLedger<Test> = StakingLedger::default_from(42);
			let reward_dest = RewardDestination::Account(10);

			assert_ok!(ledger.clone().bond(reward_dest));
			assert!(StakingLedger::<Test>::is_bonded(StakingAccount::Stash(42)));
			assert!(<Bonded<Test>>::get(&42).is_some());
			assert_eq!(<Payee<Test>>::get(&42), Some(reward_dest));

			// cannot bond again.
			assert!(ledger.clone().bond(reward_dest).is_err());

			// once bonded, update works as expected.
			ledger.legacy_claimed_rewards = bounded_vec![1];
			assert_ok!(ledger.update());
		})
	}

	#[test]
	fn bond_controller_cannot_be_stash_works() {
		ExtBuilder::default().build_and_execute(|| {
			let (stash, controller) = testing_utils::create_unique_stash_controller::<Test>(
				0,
				10,
				RewardDestination::Staked,
				false,
			)
			.unwrap();

			assert_eq!(Bonded::<Test>::get(stash), Some(controller));
			assert_eq!(Ledger::<Test>::get(controller).map(|l| l.stash), Some(stash));

			// existing controller should not be able become a stash.
			assert_noop!(
				Staking::bond(RuntimeOrigin::signed(controller), 10, RewardDestination::Staked),
				Error::<Test>::AlreadyPaired,
			);
		})
	}

	#[test]
	fn is_bonded_works() {
		ExtBuilder::default().build_and_execute(|| {
			assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Stash(42)));
			assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Controller(42)));

			// adds entry to Bonded without Ledger pair (should not happen).
			<Bonded<Test>>::insert(42, 42);
			assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Controller(42)));

			assert_eq!(<Bonded<Test>>::get(&11), Some(11));
			assert!(StakingLedger::<Test>::is_bonded(StakingAccount::Stash(11)));
			assert!(StakingLedger::<Test>::is_bonded(StakingAccount::Controller(11)));

			<Bonded<Test>>::remove(42); // ensures try-state checks pass.
		})
	}

	#[test]
	#[allow(deprecated)]
	fn set_payee_errors_on_controller_destination() {
		ExtBuilder::default().build_and_execute(|| {
			Payee::<Test>::insert(11, RewardDestination::Staked);
			assert_noop!(
				Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Controller),
				Error::<Test>::ControllerDeprecated
			);
			assert_eq!(Payee::<Test>::get(&11), Some(RewardDestination::Staked));
		})
	}

	#[test]
	#[allow(deprecated)]
	fn update_payee_migration_works() {
		ExtBuilder::default().build_and_execute(|| {
			// migrate a `Controller` variant to `Account` variant.
			Payee::<Test>::insert(11, RewardDestination::Controller);
			assert_eq!(Payee::<Test>::get(&11), Some(RewardDestination::Controller));
			assert_ok!(Staking::update_payee(RuntimeOrigin::signed(11), 11));
			assert_eq!(Payee::<Test>::get(&11), Some(RewardDestination::Account(11)));

			// Do not migrate a variant if not `Controller`.
			Payee::<Test>::insert(21, RewardDestination::Stash);
			assert_eq!(Payee::<Test>::get(&21), Some(RewardDestination::Stash));
			assert_noop!(
				Staking::update_payee(RuntimeOrigin::signed(11), 21),
				Error::<Test>::NotController
			);
			assert_eq!(Payee::<Test>::get(&21), Some(RewardDestination::Stash));
		})
	}

	#[test]
	fn deprecate_controller_batch_works_full_weight() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			// Given:

			let start = 1001;
			let mut controllers: Vec<_> = vec![];
			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let ctlr: u64 = n.into();
				let stash: u64 = (n + 10000).into();

				Ledger::<Test>::insert(
					ctlr,
					StakingLedger {
						controller: None,
						total: (10 + ctlr).into(),
						active: (10 + ctlr).into(),
						..StakingLedger::default_from(stash)
					},
				);
				Bonded::<Test>::insert(stash, ctlr);
				Payee::<Test>::insert(stash, RewardDestination::Staked);

				controllers.push(ctlr);
			}

			// When:

			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(controllers).unwrap();

			// Only `AdminOrigin` can sign.
			assert_noop!(
				Staking::deprecate_controller_batch(
					RuntimeOrigin::signed(2),
					bounded_controllers.clone()
				),
				BadOrigin
			);

			let result =
				Staking::deprecate_controller_batch(RuntimeOrigin::root(), bounded_controllers);
			assert_ok!(result);
			assert_eq!(
				result.unwrap().actual_weight.unwrap(),
				<Test as Config>::WeightInfo::deprecate_controller_batch(
					<Test as Config>::MaxControllersInDeprecationBatch::get()
				)
			);

			// Then:

			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let ctlr: u64 = n.into();
				let stash: u64 = (n + 10000).into();

				// Ledger no longer keyed by controller.
				assert_eq!(Ledger::<Test>::get(ctlr), None);
				// Bonded now maps to the stash.
				assert_eq!(Bonded::<Test>::get(stash), Some(stash));

				// Ledger is now keyed by stash.
				let ledger_updated = Ledger::<Test>::get(stash).unwrap();
				assert_eq!(ledger_updated.stash, stash);

				// Check `active` and `total` values match the original ledger set by controller.
				assert_eq!(ledger_updated.active, (10 + ctlr).into());
				assert_eq!(ledger_updated.total, (10 + ctlr).into());
			}
		})
	}

	#[test]
	fn deprecate_controller_batch_works_half_weight() {
		ExtBuilder::default().build_and_execute(|| {
			// Given:

			let start = 1001;
			let mut controllers: Vec<_> = vec![];
			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let ctlr: u64 = n.into();

				// Only half of entries are unique pairs.
				let stash: u64 = if n % 2 == 0 { (n + 10000).into() } else { ctlr };

				Ledger::<Test>::insert(
					ctlr,
					StakingLedger { controller: None, ..StakingLedger::default_from(stash) },
				);
				Bonded::<Test>::insert(stash, ctlr);
				Payee::<Test>::insert(stash, RewardDestination::Staked);

				controllers.push(ctlr);
			}

			// When:
			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(controllers.clone()).unwrap();

			let result =
				Staking::deprecate_controller_batch(RuntimeOrigin::root(), bounded_controllers);
			assert_ok!(result);
			assert_eq!(
				result.unwrap().actual_weight.unwrap(),
				<Test as Config>::WeightInfo::deprecate_controller_batch(controllers.len() as u32)
			);

			// Then:

			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let unique_pair = n % 2 == 0;
				let ctlr: u64 = n.into();
				let stash: u64 = if unique_pair { (n + 10000).into() } else { ctlr };

				// Side effect of migration for unique pair.
				if unique_pair {
					assert_eq!(Ledger::<Test>::get(ctlr), None);
				}
				// Bonded maps to the stash.
				assert_eq!(Bonded::<Test>::get(stash), Some(stash));

				// Ledger is keyed by stash.
				let ledger_updated = Ledger::<Test>::get(stash).unwrap();
				assert_eq!(ledger_updated.stash, stash);
			}
		})
	}

	#[test]
	fn deprecate_controller_batch_skips_unmigrated_controller_payees() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			// Given:

			let stash: u64 = 1000;
			let ctlr: u64 = 1001;

			Ledger::<Test>::insert(
				ctlr,
				StakingLedger { controller: None, ..StakingLedger::default_from(stash) },
			);
			Bonded::<Test>::insert(stash, ctlr);
			#[allow(deprecated)]
			Payee::<Test>::insert(stash, RewardDestination::Controller);

			// When:

			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(vec![ctlr]).unwrap();

			let result =
				Staking::deprecate_controller_batch(RuntimeOrigin::root(), bounded_controllers);
			assert_ok!(result);
			assert_eq!(
				result.unwrap().actual_weight.unwrap(),
				<Test as Config>::WeightInfo::deprecate_controller_batch(1 as u32)
			);

			// Then:

			// Esure deprecation did not happen.
			assert_eq!(Ledger::<Test>::get(ctlr).is_some(), true);

			// Bonded still keyed by controller.
			assert_eq!(Bonded::<Test>::get(stash), Some(ctlr));

			// Ledger is still keyed by controller.
			let ledger_updated = Ledger::<Test>::get(ctlr).unwrap();
			assert_eq!(ledger_updated.stash, stash);
		})
	}

	#[test]
	fn deprecate_controller_batch_with_bad_state_ok() {
		ExtBuilder::default().has_stakers(false).nominate(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// now let's deprecate all the controllers for all the existing ledgers.
			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(vec![333, 444, 555, 777]).unwrap();

			assert_ok!(Staking::deprecate_controller_batch(
				RuntimeOrigin::root(),
				bounded_controllers
			));

			assert_eq!(
				*staking_events().last().unwrap(),
				Event::ControllerBatchDeprecated { failures: 0 }
			);
		})
	}

	#[test]
	fn deprecate_controller_batch_with_bad_state_failures() {
		ExtBuilder::default().has_stakers(false).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// now let's deprecate all the controllers for all the existing ledgers.
			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(vec![777, 555, 444, 333]).unwrap();

			assert_ok!(Staking::deprecate_controller_batch(
				RuntimeOrigin::root(),
				bounded_controllers
			));

			assert_eq!(
				*staking_events().last().unwrap(),
				Event::ControllerBatchDeprecated { failures: 2 }
			);
		})
	}

	#[test]
	fn set_controller_with_bad_state_ok() {
		ExtBuilder::default().has_stakers(false).nominate(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// in this case, setting controller works due to the ordering of the calls.
			assert_ok!(Staking::set_controller(RuntimeOrigin::signed(333)));
			assert_ok!(Staking::set_controller(RuntimeOrigin::signed(444)));
			assert_ok!(Staking::set_controller(RuntimeOrigin::signed(555)));
		})
	}

	#[test]
	fn set_controller_with_bad_state_fails() {
		ExtBuilder::default().has_stakers(false).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// setting the controller of ledger associated with stash 555 fails since its stash is a
			// controller of another ledger.
			assert_noop!(
				Staking::set_controller(RuntimeOrigin::signed(555)),
				Error::<Test>::BadState
			);
			assert_noop!(
				Staking::set_controller(RuntimeOrigin::signed(444)),
				Error::<Test>::BadState
			);
			assert_ok!(Staking::set_controller(RuntimeOrigin::signed(333)));
		})
	}
}

mod ledger_recovery {
	use super::*;

	#[test]
	fn inspect_recovery_ledger_simple_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// non corrupted ledger.
			assert_eq!(Staking::inspect_bond_state(&11).unwrap(), LedgerIntegrityState::Ok);

			// non bonded stash.
			assert!(Bonded::<Test>::get(&1111).is_none());
			assert!(Staking::inspect_bond_state(&1111).is_err());

			// double bonded but not corrupted.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
		})
	}

	#[test]
	fn inspect_recovery_ledger_corupted_killed_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			let lock_333_before = asset::staked::<Test>(&333);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(333)
			// (444, 444) -> corrupted and None.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 however is OK.
			assert_eq!(Staking::inspect_bond_state(&444).unwrap(), LedgerIntegrityState::Ok);

			// kill the corrupted ledger that is associated with stash 333.
			assert_ok!(StakingLedger::<Test>::kill(&333));

			// 333 bond is no more but it returns `BadState` because the lock on this stash is
			// still set (see checks below).
			assert_eq!(Staking::inspect_bond_state(&333), Err(Error::<Test>::BadState));
			// now the *other* ledger associated with 444 has been corrupted and killed (None).
			assert_eq!(
				Staking::inspect_bond_state(&444),
				Ok(LedgerIntegrityState::CorruptedKilled)
			);

			// side effects on 333 - ledger, bonded, payee, lock should be completely empty.
			// however, 333 lock remains.
			assert_eq!(asset::staked::<Test>(&333), lock_333_before); // NOK
			assert!(Bonded::<Test>::get(&333).is_none()); // OK
			assert!(Payee::<Test>::get(&333).is_none()); // OK
			assert!(Ledger::<Test>::get(&444).is_none()); // OK

			// side effects on 444 - ledger, bonded, payee, lock should remain be intact.
			// however, 444 lock was removed.
			assert_eq!(asset::staked::<Test>(&444), 0); // NOK
			assert!(Bonded::<Test>::get(&444).is_some()); // OK
			assert!(Payee::<Test>::get(&444).is_some()); // OK
			assert!(Ledger::<Test>::get(&555).is_none()); // NOK

			assert!(Staking::do_try_state(System::block_number()).is_err());
		})
	}

	#[test]
	fn inspect_recovery_ledger_corupted_killed_other_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			let lock_333_before = asset::staked::<Test>(&333);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(444)
			// (333, 444) -> corrupted and None
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 however is OK.
			assert_eq!(Staking::inspect_bond_state(&444).unwrap(), LedgerIntegrityState::Ok);

			// kill the *other* ledger that is double bonded but not corrupted.
			assert_ok!(StakingLedger::<Test>::kill(&444));

			// now 333 is corrupted and None through the *other* ledger being killed.
			assert_eq!(
				Staking::inspect_bond_state(&333).unwrap(),
				LedgerIntegrityState::CorruptedKilled,
			);
			// 444 is cleaned and not a stash anymore; no lock left behind.
			assert_eq!(Ledger::<Test>::get(&444), None);
			assert_eq!(Staking::inspect_bond_state(&444), Err(Error::<Test>::NotStash));

			// side effects on 333 - ledger, bonded, payee, lock should be intact.
			assert_eq!(asset::staked::<Test>(&333), lock_333_before); // OK
			assert_eq!(Bonded::<Test>::get(&333), Some(444)); // OK
			assert!(Payee::<Test>::get(&333).is_some());
			// however, ledger associated with its controller was killed.
			assert!(Ledger::<Test>::get(&444).is_none()); // NOK

			// side effects on 444 - ledger, bonded, payee, lock should be completely removed.
			assert_eq!(asset::staked::<Test>(&444), 0); // OK
			assert!(Bonded::<Test>::get(&444).is_none()); // OK
			assert!(Payee::<Test>::get(&444).is_none()); // OK
			assert!(Ledger::<Test>::get(&555).is_none()); // OK

			assert!(Staking::do_try_state(System::block_number()).is_err());
		})
	}

	#[test]
	fn inspect_recovery_ledger_lock_corrupted_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// get into lock corrupted ledger state by bond_extra on a ledger that is double bonded
			// with a corrupted ledger.
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			//  bond_extra(333, 10) -> lock corrupted on 444
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);
			bond_extra_no_checks(&333, 10);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 ledger is not corrupted but locks got out of sync.
			assert_eq!(
				Staking::inspect_bond_state(&444).unwrap(),
				LedgerIntegrityState::LockCorrupted
			);
		})
	}

	// Corrupted ledger restore.
	//
	// * Double bonded and corrupted ledger.
	#[test]
	fn restore_ledger_corrupted_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// get into corrupted and killed ledger state.
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// recover the ledger bonded by 333 stash.
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}

	// Corrupted and killed ledger restore.
	//
	// * Double bonded and corrupted ledger.
	// * Ledger killed by own controller.
	#[test]
	fn restore_ledger_corrupted_killed_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// ledger.total == lock
			let total_444_before_corruption = asset::staked::<Test>(&444);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(333)
			// (444, 444) -> corrupted and None.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// kill the corrupted ledger that is associated with stash 333.
			assert_ok!(StakingLedger::<Test>::kill(&333));

			// 333 bond is no more but it returns `BadState` because the lock on this stash is
			// still set (see checks below).
			assert_eq!(Staking::inspect_bond_state(&333), Err(Error::<Test>::BadState));
			// now the *other* ledger associated with 444 has been corrupted and killed (None).
			assert!(Staking::ledger(StakingAccount::Stash(444)).is_err());

			// try-state should fail.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// recover the ledger bonded by 333 stash.
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));

			// for the try-state checks to pass, we also need to recover the stash 444 which is
			// corrupted too by proxy of kill(333). Currently, both the lock and the ledger of 444
			// have been cleared so we need to provide the new amount to restore the ledger.
			assert_noop!(
				Staking::restore_ledger(RuntimeOrigin::root(), 444, None, None, None),
				Error::<Test>::CannotRestoreLedger
			);

			assert_ok!(Staking::restore_ledger(
				RuntimeOrigin::root(),
				444,
				None,
				Some(total_444_before_corruption),
				None,
			));

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}

	// Corrupted and killed by *other* ledger restore.
	//
	// * Double bonded and corrupted ledger.
	// * Ledger killed by own controller.
	#[test]
	fn restore_ledger_corrupted_killed_other_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(444)
			// (333, 444) -> corrupted and None
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 however is OK.
			assert_eq!(Staking::inspect_bond_state(&444).unwrap(), LedgerIntegrityState::Ok);

			// kill the *other* ledger that is double bonded but not corrupted.
			assert_ok!(StakingLedger::<Test>::kill(&444));

			// recover the ledger bonded by 333 stash.
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));

			// 444 does not need recover in this case since it's been killed successfully.
			assert_eq!(Staking::inspect_bond_state(&444), Err(Error::<Test>::NotStash));

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}

	// Corrupted with bond_extra.
	//
	// * Double bonded and corrupted ledger.
	// * Corrupted ledger calls `bond_extra`
	#[test]
	fn restore_ledger_corrupted_bond_extra_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			let lock_333_before = asset::staked::<Test>(&333);
			let lock_444_before = asset::staked::<Test>(&444);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// bond_extra(444, 40) -> OK
			// bond_extra(333, 30) -> locks out of sync

			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// if 444 bonds extra, the locks remain in sync.
			bond_extra_no_checks(&444, 40);
			assert_eq!(asset::staked::<Test>(&333), lock_333_before);
			assert_eq!(asset::staked::<Test>(&444), lock_444_before + 40);

			// however if 333 bonds extra, the wrong lock is updated.
			bond_extra_no_checks(&333, 30);
			assert_eq!(asset::staked::<Test>(&333), lock_444_before + 40 + 30); //not OK
			assert_eq!(asset::staked::<Test>(&444), lock_444_before + 40); // OK

			// recover the ledger bonded by 333 stash. Note that the total/lock needs to be
			// re-written since on-chain data lock has become out of sync.
			assert_ok!(Staking::restore_ledger(
				RuntimeOrigin::root(),
				333,
				None,
				Some(lock_333_before + 30),
				None
			));

			// now recover 444 that although it's not corrupted, its lock and ledger.total are out
			// of sync. in which case, we need to explicitly set the ledger's lock and amount,
			// otherwise the ledger recover will fail.
			assert_noop!(
				Staking::restore_ledger(RuntimeOrigin::root(), 444, None, None, None),
				Error::<Test>::CannotRestoreLedger
			);

			//and enforcing a new ledger lock/total on this non-corrupted ledger will work.
			assert_ok!(Staking::restore_ledger(
				RuntimeOrigin::root(),
				444,
				None,
				Some(lock_444_before + 40),
				None
			));

			// double-check that ledgers got to expected state and bond_extra done during the
			// corrupted state is part of the recovered ledgers.
			let ledger_333 = Bonded::<Test>::get(&333).and_then(Ledger::<Test>::get).unwrap();
			let ledger_444 = Bonded::<Test>::get(&444).and_then(Ledger::<Test>::get).unwrap();

			assert_eq!(ledger_333.total, lock_333_before + 30);
			assert_eq!(asset::staked::<Test>(&333), ledger_333.total);
			assert_eq!(ledger_444.total, lock_444_before + 40);
			assert_eq!(asset::staked::<Test>(&444), ledger_444.total);

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}
}

mod validator_disabling_integration {
	use super::*;

	#[test]
	fn reenable_lower_offenders() {
		ExtBuilder::default()
			.validator_count(7)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.set_status(201, StakerStatus::Validator)
			.set_status(202, StakerStatus::Validator)
			.build_and_execute(|| {
				mock::start_active_era(1);
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 41, 51, 201, 202]);

				// offence with a low slash
				on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)]);
				on_offence_now(&[offence_from(21, None)], &[Perbill::from_percent(20)]);

				// it does NOT affect the nominator.
				assert_eq!(Staking::nominators(101).unwrap().targets, vec![11, 21]);

				// both validators should be disabled
				assert!(is_disabled(11));
				assert!(is_disabled(21));

				// offence with a higher slash
				on_offence_now(&[offence_from(31, None)], &[Perbill::from_percent(50)]);

				// First offender is no longer disabled
				assert!(!is_disabled(11));
				// Mid offender is still disabled
				assert!(is_disabled(21));
				// New offender is disabled
				assert!(is_disabled(31));

				assert_eq!(
					staking_events_since_last_call(),
					vec![
						Event::StakersElected,
						Event::EraPaid { era_index: 0, validator_payout: 11075, remainder: 33225 },
						Event::SlashReported {
							validator: 11,
							fraction: Perbill::from_percent(10),
							slash_era: 1
						},
						Event::Slashed { staker: 11, amount: 100 },
						Event::Slashed { staker: 101, amount: 12 },
						Event::SlashReported {
							validator: 21,
							fraction: Perbill::from_percent(20),
							slash_era: 1
						},
						Event::Slashed { staker: 21, amount: 200 },
						Event::Slashed { staker: 101, amount: 75 },
						Event::SlashReported {
							validator: 31,
							fraction: Perbill::from_percent(50),
							slash_era: 1
						},
						Event::Slashed { staker: 31, amount: 250 },
					]
				);

				assert!(matches!(
					session_events().as_slice(),
					&[
						..,
						SessionEvent::ValidatorDisabled { validator: 11 },
						SessionEvent::ValidatorDisabled { validator: 21 },
						SessionEvent::ValidatorDisabled { validator: 31 },
						SessionEvent::ValidatorReenabled { validator: 11 },
					]
				));
			});
	}

	#[test]
	fn do_not_reenable_higher_offenders_mock() {
		ExtBuilder::default()
			.validator_count(7)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.set_status(201, StakerStatus::Validator)
			.set_status(202, StakerStatus::Validator)
			.build_and_execute(|| {
				mock::start_active_era(1);
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 41, 51, 201, 202]);

				// offence with a major slash
				on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(50)]);
				on_offence_now(&[offence_from(21, None)], &[Perbill::from_percent(50)]);

				// both validators should be disabled
				assert!(is_disabled(11));
				assert!(is_disabled(21));

				// offence with a minor slash
				on_offence_now(&[offence_from(31, None)], &[Perbill::from_percent(10)]);

				// First and second offenders are still disabled
				assert!(is_disabled(11));
				assert!(is_disabled(21));
				// New offender is not disabled as limit is reached and his prio is lower
				assert!(!is_disabled(31));

				assert_eq!(
					staking_events_since_last_call(),
					vec![
						Event::StakersElected,
						Event::EraPaid { era_index: 0, validator_payout: 11075, remainder: 33225 },
						Event::SlashReported {
							validator: 11,
							fraction: Perbill::from_percent(50),
							slash_era: 1
						},
						Event::Slashed { staker: 11, amount: 500 },
						Event::Slashed { staker: 101, amount: 62 },
						Event::SlashReported {
							validator: 21,
							fraction: Perbill::from_percent(50),
							slash_era: 1
						},
						Event::Slashed { staker: 21, amount: 500 },
						Event::Slashed { staker: 101, amount: 187 },
						Event::SlashReported {
							validator: 31,
							fraction: Perbill::from_percent(10),
							slash_era: 1
						},
						Event::Slashed { staker: 31, amount: 50 },
					]
				);

				assert!(matches!(
					session_events().as_slice(),
					&[
						..,
						SessionEvent::ValidatorDisabled { validator: 11 },
						SessionEvent::ValidatorDisabled { validator: 21 },
					]
				));
			});
	}

	#[test]
	fn clear_disabled_only_on_era_change() {
		ExtBuilder::default()
			.validator_count(7)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.set_status(201, StakerStatus::Validator)
			.set_status(202, StakerStatus::Validator)
			.session_per_era(3)
			.build_and_execute(|| {
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 41, 51, 201, 202]);

				// offence with a major slash
				on_offence_now(
					&[offence_from(11, None), offence_from(21, None)],
					&[Perbill::from_percent(50), Perbill::from_percent(50)],
				);

				// both validators should be disabled
				assert!(is_disabled(11));
				assert!(is_disabled(21));

				// progress session and check if disablement is retained
				start_session(2);
				assert!(is_disabled(11));
				assert!(is_disabled(21));

				// progress era (3 sessions per era) and clear disablement
				start_session(3);
				assert!(!is_disabled(11));
				assert!(!is_disabled(21));
			});
	}

	#[test]
	fn validator_is_not_disabled_for_an_offence_in_previous_era() {
		ExtBuilder::default()
			.validator_count(4)
			.set_status(41, StakerStatus::Validator)
			.build_and_execute(|| {
				mock::start_active_era(1);

				assert!(<Validators<Test>>::contains_key(11));
				assert!(Session::validators().contains(&11));

				on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(0)]);

				assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
				assert!(is_disabled(11));

				mock::start_active_era(2);

				// the validator is not disabled in the new era
				Staking::validate(RuntimeOrigin::signed(11), Default::default()).unwrap();
				assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
				assert!(<Validators<Test>>::contains_key(11));
				assert!(Session::validators().contains(&11));

				mock::start_active_era(3);

				// an offence committed in era 1 is reported in era 3
				on_offence_in_era(&[offence_from(11, None)], &[Perbill::from_percent(0)], 1);

				// the validator doesn't get disabled for an old offence
				assert!(Validators::<Test>::iter().any(|(stash, _)| stash == 11));
				assert!(!is_disabled(11));

				// and we are not forcing a new era
				assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);

				on_offence_in_era(
					&[offence_from(11, None)],
					// NOTE: A 100% slash here would clean up the account, causing de-registration.
					&[Perbill::from_percent(95)],
					1,
				);

				// the validator doesn't get disabled again
				assert!(Validators::<Test>::iter().any(|(stash, _)| stash == 11));
				assert!(!is_disabled(11));
				// and we are still not forcing a new era
				assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
			});
	}

	#[test]
	fn non_slashable_offence_disables_validator() {
		ExtBuilder::default()
			.validator_count(7)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.set_status(201, StakerStatus::Validator)
			.set_status(202, StakerStatus::Validator)
			.build_and_execute(|| {
				mock::start_active_era(1);
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 41, 51, 201, 202]);

				// offence with no slash associated
				on_offence_now(&[offence_from(11, None)], &[Perbill::zero()]);

				// it does NOT affect the nominator.
				assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

				// offence that slashes 25% of the bond
				on_offence_now(&[offence_from(21, None)], &[Perbill::from_percent(25)]);

				// it DOES NOT affect the nominator.
				assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

				assert_eq!(
					staking_events_since_last_call(),
					vec![
						Event::StakersElected,
						Event::EraPaid { era_index: 0, validator_payout: 11075, remainder: 33225 },
						Event::SlashReported {
							validator: 11,
							fraction: Perbill::from_percent(0),
							slash_era: 1
						},
						Event::SlashReported {
							validator: 21,
							fraction: Perbill::from_percent(25),
							slash_era: 1
						},
						Event::Slashed { staker: 21, amount: 250 },
						Event::Slashed { staker: 101, amount: 94 }
					]
				);

				assert!(matches!(
					session_events().as_slice(),
					&[
						..,
						SessionEvent::ValidatorDisabled { validator: 11 },
						SessionEvent::ValidatorDisabled { validator: 21 },
					]
				));

				// the offence for validator 11 wasn't slashable but it is disabled
				assert!(is_disabled(11));
				// validator 21 gets disabled too
				assert!(is_disabled(21));
			});
	}

	#[test]
	fn slashing_independent_of_disabling_validator() {
		ExtBuilder::default()
			.validator_count(5)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.build_and_execute(|| {
				mock::start_active_era(1);
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 41, 51]);

				let now = ActiveEra::<Test>::get().unwrap().index;

				// --- Disable without a slash ---
				// offence with no slash associated
				on_offence_in_era(&[offence_from(11, None)], &[Perbill::zero()], now);

				// nomination remains untouched.
				assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

				// first validator is disabled
				assert!(is_disabled(11));

				// --- Slash without disabling (because limit reached) ---
				// offence that slashes 50% of the bond (setup for next slash)
				on_offence_in_era(&[offence_from(11, None)], &[Perbill::from_percent(50)], now);

				// offence that slashes 25% of the bond but does not disable
				on_offence_in_era(&[offence_from(21, None)], &[Perbill::from_percent(25)], now);

				// nomination remains untouched.
				assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

				// second validator is slashed but not disabled
				assert!(!is_disabled(21));
				assert!(is_disabled(11));

				assert_eq!(
					staking_events_since_last_call(),
					vec![
						Event::StakersElected,
						Event::EraPaid { era_index: 0, validator_payout: 11075, remainder: 33225 },
						Event::SlashReported {
							validator: 11,
							fraction: Perbill::from_percent(0),
							slash_era: 1
						},
						Event::SlashReported {
							validator: 11,
							fraction: Perbill::from_percent(50),
							slash_era: 1
						},
						Event::Slashed { staker: 11, amount: 500 },
						Event::Slashed { staker: 101, amount: 62 },
						Event::SlashReported {
							validator: 21,
							fraction: Perbill::from_percent(25),
							slash_era: 1
						},
						Event::Slashed { staker: 21, amount: 250 },
						Event::Slashed { staker: 101, amount: 94 }
					]
				);

				assert_eq!(
					session_events(),
					vec![
						SessionEvent::NewSession { session_index: 1 },
						SessionEvent::NewSession { session_index: 2 },
						SessionEvent::NewSession { session_index: 3 },
						SessionEvent::ValidatorDisabled { validator: 11 }
					]
				);
			});
	}

	#[test]
	fn offence_threshold_doesnt_force_new_era() {
		ExtBuilder::default()
			.validator_count(4)
			.set_status(41, StakerStatus::Validator)
			.build_and_execute(|| {
				mock::start_active_era(1);
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 41]);

				assert_eq!(
					UpToLimitWithReEnablingDisablingStrategy::<DISABLING_LIMIT_FACTOR>::disable_limit(
						Session::validators().len()
					),
					1
				);

				// we have 4 validators and an offending validator threshold of 1,
				// even if two validators commit an offence a new era should not be forced
				on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(50)]);

				// 11 should be disabled because the byzantine threshold is 1
				assert!(is_disabled(11));

				assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);

				on_offence_now(&[offence_from(21, None)], &[Perbill::zero()]);

				// 21 should not be disabled because the number of disabled validators will be above
				// the byzantine threshold
				assert!(!is_disabled(21));

				assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);

				on_offence_now(&[offence_from(31, None)], &[Perbill::zero()]);

				// same for 31
				assert!(!is_disabled(31));

				assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
			});
	}
}

#[cfg(all(feature = "try-runtime", test))]
mod migration_tests {
	use super::*;
	use frame_support::traits::UncheckedOnRuntimeUpgrade;
	use migrations::{v15, v16};

	#[test]
	fn migrate_v15_to_v16_with_try_runtime() {
		ExtBuilder::default().validator_count(7).build_and_execute(|| {
			// Initial setup: Create old `DisabledValidators` in the form of `Vec<u32>`
			let old_disabled_validators = vec![1u32, 2u32];
			v15::DisabledValidators::<Test>::put(old_disabled_validators.clone());

			// Run pre-upgrade checks
			let pre_upgrade_result = v16::VersionUncheckedMigrateV15ToV16::<Test>::pre_upgrade();
			assert!(pre_upgrade_result.is_ok());
			let pre_upgrade_state = pre_upgrade_result.unwrap();

			// Run the migration
			v16::VersionUncheckedMigrateV15ToV16::<Test>::on_runtime_upgrade();

			// Run post-upgrade checks
			let post_upgrade_result =
				v16::VersionUncheckedMigrateV15ToV16::<Test>::post_upgrade(pre_upgrade_state);
			assert!(post_upgrade_result.is_ok());
		});
	}
}

mod getters {
	use crate::{
		mock::{self},
		pallet::pallet::{Invulnerables, MinimumValidatorCount, ValidatorCount},
		slashing,
		tests::{Staking, Test},
		ActiveEra, ActiveEraInfo, BalanceOf, CanceledSlashPayout, ClaimedRewards, CurrentEra,
		CurrentPlannedSession, EraRewardPoints, ErasRewardPoints, ErasStakersClipped,
		ErasStartSessionIndex, ErasTotalStake, ErasValidatorPrefs, ErasValidatorReward, ForceEra,
		Forcing, Nominations, Nominators, Perbill, SlashRewardFraction, SlashingSpans,
		ValidatorPrefs, Validators,
	};
	use sp_staking::{EraIndex, Exposure, IndividualExposure, Page, SessionIndex};

	#[test]
	fn get_validator_count_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let v: u32 = 12;
			ValidatorCount::<Test>::put(v);

			// when
			let result = Staking::validator_count();

			// then
			assert_eq!(result, v);
		});
	}

	#[test]
	fn get_minimum_validator_count_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let v: u32 = 12;
			MinimumValidatorCount::<Test>::put(v);

			// when
			let result = Staking::minimum_validator_count();

			// then
			assert_eq!(result, v);
		});
	}

	#[test]
	fn get_invulnerables_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let v: Vec<mock::AccountId> = vec![1, 2, 3];
			Invulnerables::<Test>::put(v.clone());

			// when
			let result = Staking::invulnerables();

			// then
			assert_eq!(result, v);
		});
	}

	#[test]
	fn get_validators_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let account_id: mock::AccountId = 1;
			let validator_prefs = ValidatorPrefs::default();

			Validators::<Test>::insert(account_id, validator_prefs.clone());

			// when
			let result = Staking::validators(&account_id);

			// then
			assert_eq!(result, validator_prefs);
		});
	}

	#[test]
	fn get_nominators_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let account_id: mock::AccountId = 1;
			let nominations: Nominations<Test> = Nominations {
				targets: Default::default(),
				submitted_in: Default::default(),
				suppressed: false,
			};

			Nominators::<Test>::insert(account_id, nominations.clone());

			// when
			let result = Staking::nominators(account_id);

			// then
			assert_eq!(result, Some(nominations));
		});
	}

	#[test]
	fn get_current_era_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			CurrentEra::<Test>::put(era);

			// when
			let result = Staking::current_era();

			// then
			assert_eq!(result, Some(era));
		});
	}

	#[test]
	fn get_active_era_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era = ActiveEraInfo { index: 2, start: None };
			ActiveEra::<Test>::put(era);

			// when
			let result: Option<ActiveEraInfo> = Staking::active_era();

			// then
			if let Some(era_info) = result {
				assert_eq!(era_info.index, 2);
				assert_eq!(era_info.start, None);
			} else {
				panic!("Expected Some(era_info), got None");
			};
		});
	}

	#[test]
	fn get_eras_start_session_index_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			let session_index: SessionIndex = 14;
			ErasStartSessionIndex::<Test>::insert(era, session_index);

			// when
			let result = Staking::eras_start_session_index(era);

			// then
			assert_eq!(result, Some(session_index));
		});
	}

	#[test]
	fn get_eras_stakers_clipped_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			let account_id: mock::AccountId = 1;
			let exposure: Exposure<mock::AccountId, BalanceOf<Test>> = Exposure {
				total: 1125,
				own: 1000,
				others: vec![IndividualExposure { who: 101, value: 125 }],
			};
			ErasStakersClipped::<Test>::insert(era, account_id, exposure.clone());

			// when
			let result = Staking::eras_stakers_clipped(era, &account_id);

			// then
			assert_eq!(result, exposure);
		});
	}

	#[test]
	fn get_claimed_rewards_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			let account_id: mock::AccountId = 1;
			let rewards = Vec::<Page>::new();
			ClaimedRewards::<Test>::insert(era, account_id, rewards.clone());

			// when
			let result = Staking::claimed_rewards(era, &account_id);

			// then
			assert_eq!(result, rewards);
		});
	}

	#[test]
	fn get_eras_validator_prefs_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			let account_id: mock::AccountId = 1;
			let validator_prefs = ValidatorPrefs::default();

			ErasValidatorPrefs::<Test>::insert(era, account_id, validator_prefs.clone());

			// when
			let result = Staking::eras_validator_prefs(era, &account_id);

			// then
			assert_eq!(result, validator_prefs);
		});
	}

	#[test]
	fn get_eras_validator_reward_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			let balance_of = BalanceOf::<Test>::default();

			ErasValidatorReward::<Test>::insert(era, balance_of);

			// when
			let result = Staking::eras_validator_reward(era);

			// then
			assert_eq!(result, Some(balance_of));
		});
	}

	#[test]
	fn get_eras_reward_points_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			let reward_points = EraRewardPoints::<mock::AccountId> {
				total: 1,
				individual: vec![(11, 1)].into_iter().collect(),
			};
			ErasRewardPoints::<Test>::insert(era, reward_points);

			// when
			let result = Staking::eras_reward_points(era);

			// then
			assert_eq!(result.total, 1);
		});
	}

	#[test]
	fn get_eras_total_stake_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let era: EraIndex = 12;
			let balance_of = BalanceOf::<Test>::default();

			ErasTotalStake::<Test>::insert(era, balance_of);

			// when
			let result = Staking::eras_total_stake(era);

			// then
			assert_eq!(result, balance_of);
		});
	}

	#[test]
	fn get_force_era_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let forcing = Forcing::NotForcing;
			ForceEra::<Test>::put(forcing);

			// when
			let result = Staking::force_era();

			// then
			assert_eq!(result, forcing);
		});
	}

	#[test]
	fn get_slash_reward_fraction_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let perbill = Perbill::one();
			SlashRewardFraction::<Test>::put(perbill);

			// when
			let result = Staking::slash_reward_fraction();

			// then
			assert_eq!(result, perbill);
		});
	}

	#[test]
	fn get_canceled_payout_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let balance_of = BalanceOf::<Test>::default();
			CanceledSlashPayout::<Test>::put(balance_of);

			// when
			let result = Staking::canceled_payout();

			// then
			assert_eq!(result, balance_of);
		});
	}

	#[test]
	fn get_slashing_spans_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let account_id: mock::AccountId = 1;
			let spans = slashing::SlashingSpans::new(2);
			SlashingSpans::<Test>::insert(account_id, spans);

			// when
			let result: Option<slashing::SlashingSpans> = Staking::slashing_spans(&account_id);

			// then
			// simple check so as not to add extra macros to slashing::SlashingSpans struct
			assert!(result.is_some());
		});
	}

	#[test]
	fn get_current_planned_session_returns_value_from_storage() {
		sp_io::TestExternalities::default().execute_with(|| {
			// given
			let session_index = SessionIndex::default();
			CurrentPlannedSession::<Test>::put(session_index);

			// when
			let result = Staking::current_planned_session();

			// then
			assert_eq!(result, session_index);
		});
	}
}

mod hold_migration {
	use super::*;
	use sp_staking::{Stake, StakingInterface};

	#[test]
	fn ledger_update_creates_hold() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// GIVEN alice who is a nominator with old currency
			let alice = 300;
			bond_nominator(alice, 1000, vec![11]);
			assert_eq!(asset::staked::<Test>(&alice), 1000);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
			// migrate alice currency to legacy locks
			testing_utils::migrate_to_old_currency::<Test>(alice);
			// no more holds
			assert_eq!(asset::staked::<Test>(&alice), 0);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: 1000, active: 1000 })
			);

			// any ledger mutation should create a hold
			hypothetically!({
				// give some extra balance to alice.
				let _ = asset::mint_into_existing::<Test>(&alice, 100);

				// WHEN new fund is bonded to ledger.
				assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(alice), 100));

				// THEN new hold is created
				assert_eq!(asset::staked::<Test>(&alice), 1000 + 100);
				assert_eq!(
					<Staking as StakingInterface>::stake(&alice),
					Ok(Stake { total: 1100, active: 1100 })
				);

				// old locked balance is untouched
				assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			});

			hypothetically!({
				// WHEN new fund is unbonded from ledger.
				assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 100));

				// THEN hold is updated.
				assert_eq!(asset::staked::<Test>(&alice), 1000);
				assert_eq!(
					<Staking as StakingInterface>::stake(&alice),
					Ok(Stake { total: 1000, active: 900 })
				);

				// old locked balance is untouched
				assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			});

			// WHEN alice currency is migrated.
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), alice));

			// THEN hold is updated.
			assert_eq!(asset::staked::<Test>(&alice), 1000);
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: 1000, active: 1000 })
			);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), alice),
				Error::<Test>::AlreadyMigrated
			);

			// locked balance is removed
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
		});
	}

	#[test]
	fn migrate_removes_old_lock() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// GIVEN alice who is a nominator with old currency
			let alice = 300;
			bond_nominator(alice, 1000, vec![11]);
			testing_utils::migrate_to_old_currency::<Test>(alice);
			assert_eq!(asset::staked::<Test>(&alice), 0);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			let pre_migrate_consumer = System::consumers(&alice);
			System::reset_events();

			// WHEN alice currency is migrated.
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), alice));

			// THEN
			// the extra consumer from old code is removed.
			assert_eq!(System::consumers(&alice), pre_migrate_consumer - 1);
			// ensure no lock
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
			// ensure stake and hold are same.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: 1000, active: 1000 })
			);
			assert_eq!(asset::staked::<Test>(&alice), 1000);
			// ensure events are emitted.
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::CurrencyMigrated { stash: alice, force_withdraw: 0 }]
			);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), alice),
				Error::<Test>::AlreadyMigrated
			);
		});
	}
	#[test]
	fn cannot_hold_all_stake() {
		// When there is not enough funds to hold all stake, part of the stake if force withdrawn.
		// At end of the migration, the stake and hold should be same.
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// GIVEN alice who is a nominator with old currency.
			let alice = 300;
			let stake = 1000;
			bond_nominator(alice, stake, vec![11]);
			testing_utils::migrate_to_old_currency::<Test>(alice);
			assert_eq!(asset::staked::<Test>(&alice), 0);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), stake);
			// ledger has 1000 staked.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: stake, active: stake })
			);

			// Get rid of the extra ED to emulate all their balance including ED is staked.
			assert_ok!(Balances::transfer_allow_death(
				RuntimeOrigin::signed(alice),
				10,
				ExistentialDeposit::get()
			));

			let expected_force_withdraw = ExistentialDeposit::get();

			// ledger mutation would fail in this case before migration because of failing hold.
			assert_noop!(
				Staking::unbond(RuntimeOrigin::signed(alice), 100),
				Error::<Test>::NotEnoughFunds
			);

			// clear events
			System::reset_events();

			// WHEN alice currency is migrated.
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), alice));

			// THEN
			let expected_hold = stake - expected_force_withdraw;
			// ensure no lock
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
			// ensure stake and hold are same.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: expected_hold, active: expected_hold })
			);
			assert_eq!(asset::staked::<Test>(&alice), expected_hold);
			// ensure events are emitted.
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::CurrencyMigrated {
					stash: alice,
					force_withdraw: expected_force_withdraw
				}]
			);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), alice),
				Error::<Test>::AlreadyMigrated
			);

			// unbond works after migration.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 100));
		});
	}

	#[test]
	fn overstaked_and_partially_unbonding() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// GIVEN alice who is a nominator with T::OldCurrency.
			let alice = 300;
			// 1000 + ED
			let _ = Balances::make_free_balance_be(&alice, 1001);
			let stake = 600;
			let reserved_by_another_pallet = 400;
			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(alice),
				stake,
				RewardDestination::Staked
			));

			// AND Alice is partially unbonding.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 300));

			// AND Alice has some funds reserved with another pallet.
			assert_ok!(Balances::reserve(&alice, reserved_by_another_pallet));

			// convert stake to T::OldCurrency.
			testing_utils::migrate_to_old_currency::<Test>(alice);
			assert_eq!(asset::staked::<Test>(&alice), 0);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), stake);

			// ledger has correct amount staked.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: stake, active: stake - 300 })
			);

			// Alice becomes overstaked by withdrawing some staked balance.
			assert_ok!(Balances::transfer_allow_death(
				RuntimeOrigin::signed(alice),
				10,
				reserved_by_another_pallet
			));

			let expected_force_withdraw = reserved_by_another_pallet;

			// ledger mutation would fail in this case before migration because of failing hold.
			assert_noop!(
				Staking::unbond(RuntimeOrigin::signed(alice), 100),
				Error::<Test>::NotEnoughFunds
			);

			// clear events
			System::reset_events();

			// WHEN alice currency is migrated.
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), alice));

			// THEN
			let expected_hold = stake - expected_force_withdraw;
			// ensure no lock
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
			// ensure stake and hold are same.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				// expected stake is 0 since force withdrawn (400) is taken out completely of
				// active stake.
				Ok(Stake { total: expected_hold, active: 0 })
			);

			assert_eq!(asset::staked::<Test>(&alice), expected_hold);
			// ensure events are emitted.
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::CurrencyMigrated {
					stash: alice,
					force_withdraw: expected_force_withdraw
				}]
			);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), alice),
				Error::<Test>::AlreadyMigrated
			);

			// unbond works after migration.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 100));
		});
	}

	#[test]
	fn virtual_staker_consumer_provider_dec() {
		// Ensure virtual stakers consumer and provider count is decremented.
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// 200 virtual bonds
			bond_virtual_nominator(200, 201, 500, vec![11, 21]);

			// previously the virtual nominator had a provider inc by the delegation system as
			// well as a consumer by this pallet.
			System::inc_providers(&200);
			System::inc_consumers(&200).expect("has provider, can consume");

			hypothetically!({
				// migrate 200
				assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), 200));

				// ensure account does not exist in system anymore.
				assert_eq!(System::consumers(&200), 0);
				assert_eq!(System::providers(&200), 0);
				assert!(!System::account_exists(&200));

				// ensure cannot migrate again.
				assert_noop!(
					Staking::migrate_currency(RuntimeOrigin::signed(1), 200),
					Error::<Test>::AlreadyMigrated
				);
			});

			hypothetically!({
				// 200 has an erroneously extra provider
				System::inc_providers(&200);

				// causes migration to fail.
				assert_noop!(
					Staking::migrate_currency(RuntimeOrigin::signed(1), 200),
					Error::<Test>::BadState
				);
			});

			// 200 is funded for more than ED by a random account.
			assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(999), 200, 10));

			// it has an extra provider now.
			assert_eq!(System::providers(&200), 2);

			// migrate 200
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), 200));

			// 1 provider is left, consumers is 0.
			assert_eq!(System::providers(&200), 1);
			assert_eq!(System::consumers(&200), 0);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), 200),
				Error::<Test>::AlreadyMigrated
			);
		});
	}
}

// Tests for manual_slash extrinsic
// Covers the following scenarios:
// 1. Basic slashing functionality - verifies root origin slashing works correctly
// 2. Slashing with a lower percentage - should have no effect
// 3. Slashing with a higher percentage - should increase the slash amount
// 4. Slashing in non-existent eras - should fail with an error
// 5. Slashing in previous eras - should work within history depth
#[test]
fn manual_slashing_works() {
	ExtBuilder::default().validator_count(2).build_and_execute(|| {
		// setup: Start with era 0
		start_active_era(0);

		let validator_stash = 11;
		let initial_balance = Staking::slashable_balance_of(&validator_stash);
		assert!(initial_balance > 0, "Validator must have stake to be slashed");

		// scenario 1: basic slashing works
		// this verifies that the manual_slash extrinsic properly slashes a validator when
		// called with root origin
		let current_era = CurrentEra::<Test>::get().unwrap();
		let slash_fraction_1 = Perbill::from_percent(25);

		// only root can call this function
		assert_noop!(
			Staking::manual_slash(
				RuntimeOrigin::signed(10),
				validator_stash,
				current_era,
				slash_fraction_1
			),
			BadOrigin
		);

		// root can slash
		assert_ok!(Staking::manual_slash(
			RuntimeOrigin::root(),
			validator_stash,
			current_era,
			slash_fraction_1
		));

		// check if balance was slashed correctly (25%)
		let balance_after_first_slash = Staking::slashable_balance_of(&validator_stash);
		let expected_balance_1 = initial_balance - (initial_balance / 4); // 25% slash

		assert!(
			balance_after_first_slash <= expected_balance_1 &&
				balance_after_first_slash >= expected_balance_1 - 5,
			"First slash was not applied correctly. Expected around {}, got {}",
			expected_balance_1,
			balance_after_first_slash
		);

		// clear events from first slash
		System::reset_events();

		// scenario 2: slashing with a smaller fraction has no effect
		// when a validator has already been slashed by a higher percentage,
		// attempting to slash with a lower percentage should have no effect
		let slash_fraction_2 = Perbill::from_percent(10); // Smaller than 25%
		assert_ok!(Staking::manual_slash(
			RuntimeOrigin::root(),
			validator_stash,
			current_era,
			slash_fraction_2
		));

		// balance should not change because we already slashed with a higher percentage
		let balance_after_second_slash = Staking::slashable_balance_of(&validator_stash);
		assert_eq!(
			balance_after_first_slash, balance_after_second_slash,
			"Balance changed after slashing with smaller fraction"
		);

		// verify no Slashed event since slash fraction is lower than previous
		let no_slashed_events = !System::events().iter().any(|record| {
			matches!(record.event, RuntimeEvent::Staking(Event::<Test>::Slashed { .. }))
		});
		assert!(no_slashed_events, "A Slashed event was incorrectly emitted immediately");

		// clear events again
		System::reset_events();

		// scenario 3: slashing with a larger fraction works
		// when a validator is slashed with a higher percentage than previous slashes,
		// their stake should be further reduced to match the new larger slash percentage
		let slash_fraction_3 = Perbill::from_percent(50); // Larger than 25%
		assert_ok!(Staking::manual_slash(
			RuntimeOrigin::root(),
			validator_stash,
			current_era,
			slash_fraction_3
		));

		// check if balance was further slashed (from 75% to 50% of original)
		let balance_after_third_slash = Staking::slashable_balance_of(&validator_stash);
		let expected_balance_3 = initial_balance / 2; // 50% of original

		assert!(
			balance_after_third_slash <= expected_balance_3 &&
				balance_after_third_slash >= expected_balance_3 - 5,
			"Third slash was not applied correctly. Expected around {}, got {}",
			expected_balance_3,
			balance_after_third_slash
		);

		// verify a Slashed event was emitted
		assert!(
			System::events().iter().any(|record| {
				matches!(
					record.event,
					RuntimeEvent::Staking(Event::<Test>::Slashed { staker, .. })
					if staker == validator_stash
				)
			}),
			"No Slashed event was emitted after effective slash"
		);

		// scenario 4: slashing in a non-existent era fails
		// the manual_slash extrinsic should validate that the era exists within history depth
		assert_noop!(
			Staking::manual_slash(RuntimeOrigin::root(), validator_stash, 999, slash_fraction_1),
			Error::<Test>::InvalidEraToReward
		);

		// move to next era
		start_active_era(1);

		// scenario 5: slashing in previous era still works
		// as long as the era is within history depth, validators can be slashed for past eras
		assert_ok!(Staking::manual_slash(
			RuntimeOrigin::root(),
			validator_stash,
			0,
			Perbill::from_percent(75)
		));

		// check balance was further reduced
		let balance_after_fifth_slash = Staking::slashable_balance_of(&validator_stash);
		let expected_balance_5 = initial_balance / 4; // 25% of original (75% slashed)

		assert!(
			balance_after_fifth_slash <= expected_balance_5 &&
				balance_after_fifth_slash >= expected_balance_5 - 5,
			"Fifth slash was not applied correctly. Expected around {}, got {}",
			expected_balance_5,
			balance_after_fifth_slash
		);
	})
}
