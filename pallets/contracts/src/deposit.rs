// This file is part of Substrate.

// Copyright (C) 2020-2021 Patract Labs Ltd.
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

//! A module responsible for computing the right amount of weight and charging it.

use crate::{
	exec::Executable, wasm::PrefabWasmModule, AliveContractInfo, BalanceOf, Config, ContractInfo,
	ContractInfoOf, Error, Pallet,
};
use frame_support::traits::{Currency, ExistenceRequirement, Get};
use pallet_contracts_primitives::{RentProjection, RentProjectionResult};
use sp_core::crypto::UncheckedFrom;
use sp_runtime::{traits::Saturating, DispatchError, DispatchResult};

/// The amount to charge.
///
/// This amount respects the contract's rent allowance and the subsistence deposit.
/// Because of that, charging the amount cannot remove the contract.
struct OutstandingAmount<T: Config> {
	amount: BalanceOf<T>,
}

impl<T: Config> OutstandingAmount<T> {
	/// Create the new outstanding amount.
	///
	/// The amount should be always withdrawable and it should not kill the account.
	fn new(amount: BalanceOf<T>) -> Self {
		Self { amount }
	}

	/// Returns the amount this instance wraps.
	fn peek(&self) -> BalanceOf<T> {
		self.amount
	}

	/// Deposit the outstanding amount from the given account.
	fn deposit(self, account: &T::AccountId, deposit_pool: &T::AccountId) -> DispatchResult {
		T::Currency::transfer(
			account,
			deposit_pool,
			self.amount,
			ExistenceRequirement::KeepAlive,
		)
		.map_err(|_| Error::<T>::TransferFailed)?;
		Ok(())
	}

	/// Refund the outstanding amount to the given account.
	fn refund(self, deposit_pool: &T::AccountId, account: &T::AccountId) -> DispatchResult {
		T::Currency::transfer(
			deposit_pool,
			account,
			self.amount,
			ExistenceRequirement::KeepAlive,
		)
		.map_err(|_| Error::<T>::TransferFailed)?;
		Ok(())
	}
}

enum Verdict<T: Config> {
	InsufficientDeposit,
	/// Everything is OK, we just only take some charge.
	Charge {
		amount: OutstandingAmount<T>,
	},
	/// Refund of deposit if storage is deleted.
	Refund {
		amount: OutstandingAmount<T>,
	},
}

pub struct Deposit<T, E>(sp_std::marker::PhantomData<(T, E)>);

impl<T, E> Deposit<T, E>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Executable<T>,
{
	/// Returns a fee charged from the contract.
	///
	/// This function accounts for the storage rent deposit.
	fn compute_deposit(
		initial_contract: &AliveContractInfo<T>,
		contract: &AliveContractInfo<T>,
		aggregate_code: Option<u32>,
	) -> (BalanceOf<T>, bool) {
		let (bytes_size, bytes_overflow) = contract
			.storage_size
			.saturating_add(aggregate_code.unwrap_or(0))
			.overflowing_sub(initial_contract.storage_size);
		let bytes_deposit = T::DepositPerStorageByte::get().saturating_mul(bytes_size.into());

		let (pair_count, pair_count_overflow) = contract
			.pair_count
			.overflowing_sub(initial_contract.pair_count);
		let pair_count_deposit = T::DepositPerStorageItem::get().saturating_mul(pair_count.into());

		log::debug!(
			target: "runtime::contracts",
			"compute deposit, bytes size: {:?}, bytes deposit: {:?}, pair count: {:?}, pair count deposit: {:?}",
			bytes_size, bytes_deposit, pair_count, pair_count_deposit
		);

		match (bytes_overflow, pair_count_overflow) {
			(true, true) => {
				// bytes minus & pair_count minus
				let deposit_balance = bytes_deposit.saturating_add(pair_count_deposit);
				(deposit_balance, true)
			}
			(true, false) => {
				// bytes minus & pair_count add
				if bytes_deposit > pair_count_deposit {
					let deposit_balance = bytes_deposit.saturating_sub(pair_count_deposit);
					(deposit_balance, true)
				} else {
					let deposit_balance = pair_count_deposit.saturating_sub(bytes_deposit);
					(deposit_balance, false)
				}
			}
			(false, true) => {
				// bytes add & pair_count minus
				if bytes_deposit > pair_count_deposit {
					let deposit_balance = bytes_deposit.saturating_sub(pair_count_deposit);
					(deposit_balance, false)
				} else {
					let deposit_balance = pair_count_deposit.saturating_sub(bytes_deposit);
					(deposit_balance, true)
				}
			}
			(false, false) => {
				// bytes add & pair_count add
				let deposit_balance = bytes_deposit.saturating_add(pair_count_deposit);
				(deposit_balance, false)
			}
		}
	}

	/// Returns amount of funds available to consume by rent mechanism.
	///
	/// Rent mechanism cannot consume more than `rent_allowance` set by the contract and it cannot make
	/// the balance lower than [`subsistence_threshold`].
	///
	/// In case the toal_balance is below the subsistence threshold, this function returns `None`.
	fn deposit_budget(
		total_balance: &BalanceOf<T>,
		free_balance: &BalanceOf<T>,
		deposit_limit: &BalanceOf<T>,
	) -> Option<BalanceOf<T>> {
		let subsistence_threshold = Pallet::<T>::subsistence_threshold();
		// Reserved balance contributes towards the subsistence threshold to stay consistent
		// with the existential deposit where the reserved balance is also counted.
		if *total_balance < subsistence_threshold {
			return None;
		}

		// However, reserved balance cannot be charged so we need to use the free balance
		// to calculate the actual budget (which can be 0).
		let rent_allowed_to_charge = free_balance.saturating_sub(subsistence_threshold);
		Some(<BalanceOf<T>>::min(*deposit_limit, rent_allowed_to_charge))
	}

	/// Consider the case for deposit payment of the tx origin account and returns a `Verdict`.
	fn consider_case(
		tx_origin: &T::AccountId,
		account: &T::AccountId,
		initial_contract: &AliveContractInfo<T>,
		contract: &AliveContractInfo<T>,
		deposit_limit: &BalanceOf<T>,
		aggregate_code: Option<u32>,
	) -> Verdict<T> {
		let total_balance = T::Currency::total_balance(tx_origin);
		let free_balance = T::Currency::free_balance(tx_origin);

		// An amount of funds to charge for storage taken up by the contract.
		let (deposit_value, refund) =
			Self::compute_deposit(initial_contract, contract, aggregate_code);
		if refund {
			return Verdict::Refund {
				amount: OutstandingAmount::new(deposit_value),
			};
		}

		let deposit_budget =
			match Self::deposit_budget(&total_balance, &free_balance, deposit_limit) {
				Some(deposit_budget) => deposit_budget,
				None => {
					// call from apps for simulation executive with zero account.
					if *tx_origin == Default::default() {
						*deposit_limit
					} else {
						// All functions that allow a contract to transfer balance enforce
						// that the contract always stays above the subsistence threshold.
						// We want the rent system to always leave a tombstone to prevent the
						// accidental loss of a contract. Ony `seal_terminate` can remove a
						// contract without a tombstone. Therefore this case should be never
						// hit.
						log::error!(
							target: "runtime::contracts",
							"Tombstoned a contract that is below the subsistence threshold: {:?}",
							account,
						);
						0u32.into()
					}
				}
			};

		log::debug!(target: "runtime::contracts",
					"deposit consider case, tx origin: {:?}, total balance: {:?}, free balance: {:?}, \
					 deposit value: {:?}, deposit budget: {:?}",
					tx_origin, total_balance, free_balance, deposit_value, deposit_budget
		);

		let insufficient_deposit = deposit_budget < deposit_value;
		if insufficient_deposit {
			return Verdict::InsufficientDeposit;
		}

		// // If the rent payment cannot be withdrawn due to locks on the account balance, then evict the
		// // account.
		// //
		// // NOTE: This seems problematic because it provides a way to tombstone an account while
		// // avoiding the last rent payment. In effect, someone could retroactively set rent_allowance
		// // for their contract to 0.
		let dues_limited = deposit_value.min(deposit_budget);
		return Verdict::Charge {
			// We choose to use `dues_limited` here instead of `dues` just to err on the safer side.
			amount: OutstandingAmount::new(dues_limited),
		};
	}

	/// Enacts the given verdict and returns the updated `ContractInfo`.
	///
	/// `alive_contract_info` should be from the same address as `account`.
	///
	/// # Note
	///
	/// if `evictable_code` is `None` an `Evict` verdict will not be enacted. This is for
	/// when calling this function during a `call` where access to the soon to be evicted
	/// contract should be denied but storage should be left unmodified.
	fn enact_verdict(
		tx_origin: &T::AccountId,
		account: &T::AccountId,
		alive_contract_info: AliveContractInfo<T>,
		current_block_number: T::BlockNumber,
		verdict: Verdict<T>,
		evictable_code: Option<PrefabWasmModule<T>>,
	) -> Result<Option<AliveContractInfo<T>>, DispatchError> {
		let deposit_pool = Pallet::<T>::account_id();
		match (verdict, evictable_code) {
			(Verdict::Charge { amount }, _) => {
				let contract = ContractInfo::Alive(AliveContractInfo::<T> {
					deduct_block: current_block_number,
					rent_payed: alive_contract_info.rent_payed.saturating_add(amount.peek()),
					..alive_contract_info
				});
				<ContractInfoOf<T>>::insert(account, &contract);
				amount.deposit(tx_origin, &deposit_pool)?;
				Ok(Some(
					contract
						.get_alive()
						.expect("We just constructed it as alive. qed"),
				))
			}
			(Verdict::Refund { amount }, _) => {
				let contract = ContractInfo::Alive(AliveContractInfo::<T> {
					deduct_block: current_block_number,
					rent_payed: alive_contract_info.rent_payed.saturating_sub(amount.peek()),
					..alive_contract_info
				});
				<ContractInfoOf<T>>::insert(account, &contract);
				amount.refund(&deposit_pool, tx_origin)?;
				Ok(Some(
					contract
						.get_alive()
						.expect("We just constructed it as alive. qed"),
				))
			}
			(Verdict::InsufficientDeposit, _) => Err(DispatchError::Other(
				"Insufficient fund to deposit for contract storage.",
			)),
		}
	}

	/// Make account paying the rent for the current block number
	///
	/// This functions does **not** evict the contract. It returns `None` in case the
	/// contract is in need of eviction. [`try_eviction`] must
	/// be called to perform the eviction.
	pub fn charge(
		tx_origin: &T::AccountId,
		account: &T::AccountId,
		initial_contract: AliveContractInfo<T>,
		contract: AliveContractInfo<T>,
		deposit_limit: &BalanceOf<T>,
		aggregate_code: Option<u32>,
	) -> Result<Option<AliveContractInfo<T>>, DispatchError> {
		let current_block_number = <frame_system::Pallet<T>>::block_number();
		let verdict = Self::consider_case(
			tx_origin,
			account,
			&initial_contract,
			&contract,
			deposit_limit,
			aggregate_code,
		);
		let result = Self::enact_verdict(
			tx_origin,
			account,
			contract,
			current_block_number,
			verdict,
			None,
		);
		if let Err(_err) = result {
			log::error!(target: "runtime::contracts", "enact verdict failed: {:?}", result);
		}
		result
	}

	/// Process a report that a contract under the given address should be evicted.
	///
	/// Enact the eviction right away if the contract should be evicted and return the amount
	/// of rent that the contract payed over its lifetime.
	/// Otherwise, **do nothing** and return None.
	///
	/// The `handicap` parameter gives a way to check the rent to a moment in the past instead
	/// of current block. E.g. if the contract is going to be evicted at the current block,
	/// `handicap = 1` can defer the eviction for 1 block. This is useful to handicap certain snitchers
	/// relative to others.
	///
	/// NOTE this function performs eviction eagerly. All changes are read and written directly to
	/// storage.
	pub fn try_eviction(
		account: &T::AccountId,
		_handicap: T::BlockNumber,
	) -> Result<(Option<BalanceOf<T>>, u32), DispatchError> {
		let contract = <ContractInfoOf<T>>::get(account);
		let contract = match contract {
			None | Some(ContractInfo::Tombstone(_)) => return Ok((None, 0)),
			Some(ContractInfo::Alive(contract)) => contract,
		};
		let module = PrefabWasmModule::<T>::from_storage_noinstr(contract.code_hash)?;
		let code_len = module.code_len();
		Ok((None, code_len))
	}

	/// Returns the projected time a given contract will be able to sustain paying its rent. The
	/// returned projection is relevant for the current block, i.e. it is as if the contract was
	/// accessed at the beginning of the current block. Returns `None` in case if the contract was
	/// evicted before or as a result of the rent collection.
	///
	/// The returned value is only an estimation. It doesn't take into account any top ups, changing the
	/// rent allowance, or any problems coming from withdrawing the dues.
	///
	/// NOTE that this is not a side-effect free function! It will actually collect rent and then
	/// compute the projection. This function is only used for implementation of an RPC method through
	/// `RuntimeApi` meaning that the changes will be discarded anyway.
	pub fn compute_projection(_account: &T::AccountId) -> RentProjectionResult<T::BlockNumber> {
		Ok(RentProjection::NoEviction)
	}
}
