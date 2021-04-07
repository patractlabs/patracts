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
	exec::Executable, storage::Storage, wasm::PrefabWasmModule, AliveContractInfo, BalanceOf,
	Config, ContractInfo, ContractInfoOf, Deposits, Error, Event, Pallet,
};
use codec::{Decode, Encode};
use frame_support::traits::{Currency, ExistenceRequirement, Get};
use frame_support::weights::{DispatchInfo, PostDispatchInfo};
use pallet_contracts_primitives::{RentProjection, RentProjectionResult};
use sp_core::crypto::UncheckedFrom;
use sp_runtime::{
	traits::{DispatchInfoOf, Dispatchable, PostDispatchInfoOf, Saturating, SignedExtension},
	transaction_validity::{InvalidTransaction, TransactionValidityError},
	DispatchError, DispatchResult,
};

/// The amount to charge.
///
/// This amount respects the contract's storage deposit and the subsistence deposit.
/// Because of that, charging the amount cannot remove the contract.
struct OutstandingAmount<T: Config> {
	amount: BalanceOf<T>,
}

impl<T: Config> OutstandingAmount<T> {
	/// Create the new outstanding amount.
	///
	/// The amount should be always transferable and it should not kill the account.
	fn new(amount: BalanceOf<T>) -> Self {
		Self { amount }
	}

	/// Returns the amount this instance wraps.
	#[allow(unused)]
	fn peek(&self) -> BalanceOf<T> {
		self.amount
	}

	/// Deposit the outstanding amount to the deposit pool from the given account.
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

	/// Return the outstanding amount to the given account from the deposit pool.
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
	/// Everything is OK, we just only take some charge.
	Charge { amount: BalanceOf<T> },
	/// Refund of deposit if storage is deleted.
	Refund { amount: BalanceOf<T> },
	/// The free balance or rest gas is not enough to pay for storage deposit.
	InsufficientDeposit,
	/// Call the contract self-destruct method, delete all storage and state of the contract,
	/// and return all deposits to the caller.
	SelfDestruct { amount: Option<BalanceOf<T>> },
}

pub struct Deposit<T, E>(sp_std::marker::PhantomData<(T, E)>);

impl<T, E> Deposit<T, E>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Executable<T>,
{
	/// Returns a fee charged from the increase or decrease of contract storage and whether to refund fee to the tx origin.
	///
	/// This function accounts for the storage deposit.
	fn compute_deposit(
		initial_contract: &AliveContractInfo<T>,
		executed_contract: &AliveContractInfo<T>,
		aggregate_code: Option<u32>,
	) -> (BalanceOf<T>, bool) {
		let (bytes_size, bytes_overflow) = executed_contract
			.storage_size
			.saturating_add(aggregate_code.unwrap_or(0))
			.overflowing_sub(initial_contract.storage_size);
		let bytes_deposit = T::DepositPerStorageByte::get().saturating_mul(bytes_size.into());

		let (pair_count, pair_count_overflow) = executed_contract
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
				(bytes_deposit.saturating_add(pair_count_deposit), true)
			}
			(true, false) => {
				// bytes minus & pair_count add
				if bytes_deposit > pair_count_deposit {
					(bytes_deposit.saturating_sub(pair_count_deposit), true)
				} else {
					(pair_count_deposit.saturating_sub(bytes_deposit), false)
				}
			}
			(false, true) => {
				// bytes add & pair_count minus
				if bytes_deposit > pair_count_deposit {
					(bytes_deposit.saturating_sub(pair_count_deposit), false)
				} else {
					(pair_count_deposit.saturating_sub(bytes_deposit), true)
				}
			}
			(false, false) => {
				// bytes add & pair_count add
				(bytes_deposit.saturating_add(pair_count_deposit), false)
			}
		}
	}

	/// Returns amount of funds available to consume by deposit mechanism.
	///
	/// allowed charge must be less than deposit limit and it cannot make
	/// the balance lower than [`subsistence_threshold`].
	///
	/// In case the total_balance is below the subsistence threshold, this function returns `None`.
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
		let deposit_allowed_to_charge = free_balance.saturating_sub(subsistence_threshold);
		Some(<BalanceOf<T>>::min(
			*deposit_limit,
			deposit_allowed_to_charge,
		))
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
		let (deposit_value, is_refund) =
			Self::compute_deposit(initial_contract, contract, aggregate_code);
		if is_refund {
			return Verdict::Refund {
				amount: deposit_value,
			};
		}

		// TODO 合约调用合约时 limit 是否累加
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

		if deposit_budget < deposit_value {
			return Verdict::InsufficientDeposit;
		}

		let dues_limited = deposit_value.min(deposit_budget);
		return Verdict::Charge {
			// We choose to use `dues_limited` here instead of `deposit_value` just to err on the safer side.
			amount: dues_limited,
		};
	}

	/// Enacts the given verdict and returns the updated `ContractInfo`.
	///
	/// `alive_contract_info` should be from the same address as `account`.
	///
	/// # Note
	///
	/// if `evictable_code` is `None` an `SelfDestruct` verdict will not be enacted. This is for
	/// when calling this function during a `call` where access to the soon to be evicted
	/// contract should be denied but storage should be left unmodified.
	fn enact_verdict(
		tx_origin: &T::AccountId,
		account: &T::AccountId,
		alive_contract_info: AliveContractInfo<T>,
		verdict: Verdict<T>,
		evictable_code: Option<PrefabWasmModule<T>>,
	) -> Result<(Option<AliveContractInfo<T>>, Option<BalanceOf<T>>), DispatchError> {
		let nonce = <frame_system::Pallet<T>>::account_nonce(tx_origin);
		let current_block_number = <frame_system::Pallet<T>>::block_number();

		match (verdict, evictable_code) {
			(Verdict::Charge { amount }, _) => {
				let contract = ContractInfo::Alive(AliveContractInfo::<T> {
					deduct_block: current_block_number,
					rent_payed: alive_contract_info.rent_payed.saturating_add(amount),
					..alive_contract_info
				});
				<ContractInfoOf<T>>::insert(account, &contract);
				<Deposits<T>>::insert((tx_origin.clone(), nonce), (amount, false));

				Ok((
					Some(
						contract
							.get_alive()
							.expect("We just constructed it as alive. qed"),
					),
					Some(amount),
				))
			}
			(Verdict::Refund { amount }, _) => {
				let contract = ContractInfo::Alive(AliveContractInfo::<T> {
					deduct_block: current_block_number,
					rent_payed: alive_contract_info.rent_payed.saturating_sub(amount),
					..alive_contract_info
				});
				<ContractInfoOf<T>>::insert(account, &contract);
				<Deposits<T>>::insert((tx_origin.clone(), nonce), (amount, true));

				Ok((
					Some(
						contract
							.get_alive()
							.expect("We just constructed it as alive. qed"),
					),
					None,
				))
			}
			(Verdict::InsufficientDeposit, _) => Err(DispatchError::Other(
				"Insufficient fund to deposit for contract storage.",
			)),
			(Verdict::SelfDestruct { amount }, Some(code)) => {
				// TODO Not the final design
				// We need to remove the trie first because it is the only operation
				// that can fail and this function is called without a storage
				// transaction when called through `claim_surcharge`.
				Storage::<T>::queue_trie_for_deletion(&alive_contract_info)?;

				if let Some(amount) = amount {
					<Deposits<T>>::insert((tx_origin.clone(), nonce), (amount, true));
				}

				<ContractInfoOf<T>>::remove(account);
				code.drop_from_storage();
				<Pallet<T>>::deposit_event(Event::Evicted(account.clone()));
				Ok((None, None))
			}
			(Verdict::SelfDestruct { amount: _amount }, None) => Ok((None, None)),
		}
	}

	/// Make account paying the deposit for the contract storage.
	///
	/// This functions does **not** evict the contract.
	pub fn charge(
		tx_origin: &T::AccountId,
		account: &T::AccountId,
		initial_contract: AliveContractInfo<T>,
		contract: AliveContractInfo<T>,
		deposit_limit: &BalanceOf<T>,
		aggregate_code: Option<u32>,
	) -> Result<(Option<AliveContractInfo<T>>, Option<BalanceOf<T>>), DispatchError> {
		let verdict = Self::consider_case(
			tx_origin,
			account,
			&initial_contract,
			&contract,
			deposit_limit,
			aggregate_code,
		);
		let result = Self::enact_verdict(tx_origin, account, contract, verdict, None);
		if let Err(_err) = result {
			log::error!(target: "runtime::contracts", "charge enact verdict failed: {:?}", result);
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
		tx_origin: &T::AccountId,
		account: &T::AccountId,
	) -> Result<(Option<BalanceOf<T>>, u32), DispatchError> {
		let contract = <ContractInfoOf<T>>::get(account);
		let contract = match contract {
			None | Some(ContractInfo::Tombstone(_)) => return Ok((None, 0)),
			Some(ContractInfo::Alive(contract)) => contract,
		};
		let module = PrefabWasmModule::<T>::from_storage_noinstr(contract.code_hash)?;
		let code_len = module.code_len();
		let deposit_payed = contract.rent_payed;
		let verdict = Verdict::SelfDestruct {
			amount: Some(deposit_payed),
		};
		Self::enact_verdict(tx_origin, account, contract, verdict, Some(module))?;
		Ok((Some(deposit_payed), code_len))
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

/// Require the transactor pay for deposit of contract storage.
#[derive(Encode, Decode, Clone, Eq, PartialEq)]
pub struct ChargeDepositPayment<T: Config + Send + Sync>(sp_std::marker::PhantomData<T>);

impl<T: Config + Send + Sync> ChargeDepositPayment<T> {
	/// Create new `SignedExtension` to check runtime version.
	pub fn new() -> Self {
		Self(sp_std::marker::PhantomData)
	}
}

impl<T: Config + Send + Sync> sp_std::fmt::Debug for ChargeDepositPayment<T> {
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		write!(f, "ChargeDepositPayment<{:?}>", self.0)
	}
	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		Ok(())
	}
}

impl<T: Config + Send + Sync> SignedExtension for ChargeDepositPayment<T>
where
	T::Call: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
{
	const IDENTIFIER: &'static str = "ChargeDepositPayment";
	type AccountId = T::AccountId;
	type Call = T::Call;
	type AdditionalSigned = ();
	type Pre = (
		// who paid the deposit
		Self::AccountId,
		// tx origin nonce
		T::Index,
	);
	fn additional_signed(&self) -> sp_std::result::Result<(), TransactionValidityError> {
		Ok(())
	}

	fn pre_dispatch(
		self,
		who: &Self::AccountId,
		_call: &Self::Call,
		_info: &DispatchInfoOf<Self::Call>,
		_len: usize,
	) -> Result<Self::Pre, TransactionValidityError> {
		let nonce = <frame_system::Pallet<T>>::account_nonce(who);
		Ok((who.clone(), nonce))
	}

	fn post_dispatch(
		pre: Self::Pre,
		_info: &DispatchInfoOf<Self::Call>,
		_post_info: &PostDispatchInfoOf<Self::Call>,
		_len: usize,
		_result: &DispatchResult,
	) -> Result<(), TransactionValidityError> {
		let (tx_origin, nonce) = pre;
		if let Some((deposit_value, is_refund)) = <Deposits<T>>::take((tx_origin.clone(), nonce)) {
			let deposit_pool = Pallet::<T>::account_id();
			log::debug!(target: "runtime::contracts", "post dispatch get deposit: {:?}, is_refund: {}", deposit_value, is_refund);

			let amount = OutstandingAmount::<T>::new(deposit_value);
			if is_refund {
				amount
					.refund(&deposit_pool, &tx_origin)
					.map_err(|_| TransactionValidityError::Invalid(InvalidTransaction::Payment))?;
			} else {
				amount
					.deposit(&tx_origin, &deposit_pool)
					.map_err(|_| TransactionValidityError::Invalid(InvalidTransaction::Payment))?;
			}
		}
		Ok(())
	}
}
