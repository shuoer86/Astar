// This file is part of Astar.

// Copyright (C) 2019-2023 Stake Technologies Pte.Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later

// Astar is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Astar is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Astar. If not, see <http://www.gnu.org/licenses/>.

//! # XVM pallet
//!
//! A module to provide
//!
//! ## Overview
//!
//! The XVM pallet provides a runtime interface to call different VMs. It currently
//! supports two VMs: EVM and WASM. With further development, more VMs can be added.
//!
//! Together with other functionalities like Chain Extension and precompiles,
//! the XVM pallet enables the runtime to support cross-VM calls.
//!
//! ## Interface
//!
//! ### Implementation
//!
//! - Implements `XvmCall` trait.
//!

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
use alloc::format;

use fp_evm::ExitReason;
use frame_support::{ensure, traits::Currency, weights::Weight};
use pallet_contracts::{CollectEvents, DebugInfo, Determinism};
use pallet_contracts_primitives::ReturnFlags;
use pallet_evm::GasWeightMapping;
use parity_scale_codec::Decode;
use sp_core::{H160, U256};
use sp_std::{marker::PhantomData, prelude::*};

use astar_primitives::{
    ethereum_checked::{
        AccountMapping, CheckedEthereumTransact, CheckedEthereumTx, EthereumTxInput,
    },
    xvm::{
        CallFailure, CallOutput, CallResult, Context, FailureError::*, FailureRevert::*, VmId,
        XvmCall,
    },
    Balance,
};

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

pub mod weights;
pub use weights::WeightInfo;

mod mock;
mod tests;

pub use pallet::*;

pub type WeightInfoOf<T> = <T as Config>::WeightInfo;

environmental::thread_local_impl!(static IN_XVM: environmental::RefCell<bool> = environmental::RefCell::new(false));

#[frame_support::pallet]
pub mod pallet {
    use super::*;

    #[pallet::pallet]
    pub struct Pallet<T>(PhantomData<T>);

    #[pallet::config]
    pub trait Config: frame_system::Config + pallet_contracts::Config {
        /// Mapping from `Account` to `H160`.
        type AccountMapping: AccountMapping<Self::AccountId>;

        /// Mapping from Ethereum gas to Substrate weight.
        type GasWeightMapping: GasWeightMapping;

        /// `CheckedEthereumTransact` implementation.
        type EthereumTransact: CheckedEthereumTransact;

        /// Weight information for extrinsics in this pallet.
        type WeightInfo: WeightInfo;
    }
}

impl<T> XvmCall<T::AccountId> for Pallet<T>
where
    T: Config,
    T::Currency: Currency<T::AccountId, Balance = Balance>,
{
    fn call(
        context: Context,
        vm_id: VmId,
        source: T::AccountId,
        target: Vec<u8>,
        input: Vec<u8>,
        value: Balance,
        storage_deposit_limit: Option<Balance>,
    ) -> CallResult {
        Pallet::<T>::do_call(
            context,
            vm_id,
            source,
            target,
            input,
            value,
            storage_deposit_limit,
            false,
        )
    }
}

impl<T> Pallet<T>
where
    T: Config,
    T::Currency: Currency<T::AccountId, Balance = Balance>,
{
    fn do_call(
        context: Context,
        vm_id: VmId,
        source: T::AccountId,
        target: Vec<u8>,
        input: Vec<u8>,
        value: Balance,
        storage_deposit_limit: Option<Balance>,
        skip_execution: bool,
    ) -> CallResult {
        let overheads = match vm_id {
            VmId::Evm => WeightInfoOf::<T>::evm_call_overheads(),
            VmId::Wasm => WeightInfoOf::<T>::wasm_call_overheads(),
        };

        ensure!(
            context.source_vm_id != vm_id,
            CallFailure::error(SameVmCallDenied, overheads)
        );

        // Set `IN_XVM` to true & check reentrance.
        if IN_XVM.with(|in_xvm| in_xvm.replace(true)) {
            return Err(CallFailure::error(ReentranceDenied, overheads));
        }

        let res = match vm_id {
            VmId::Evm => Pallet::<T>::evm_call(
                context,
                source,
                target,
                input,
                value,
                overheads,
                skip_execution,
            ),
            VmId::Wasm => Pallet::<T>::wasm_call(
                context,
                source,
                target,
                input,
                value,
                overheads,
                storage_deposit_limit,
                skip_execution,
            ),
        };

        // Set `IN_XVM` to false.
        // We should make sure that this line is executed whatever the execution path.
        let _ = IN_XVM.with(|in_xvm| in_xvm.take());

        res
    }

    fn evm_call(
        context: Context,
        source: T::AccountId,
        target: Vec<u8>,
        input: Vec<u8>,
        value: Balance,
        overheads: Weight,
        skip_execution: bool,
    ) -> CallResult {
        log::trace!(
            target: "xvm::evm_call",
            "Calling EVM: {:?} {:?}, {:?}, {:?}, {:?}",
            context, source, target, input, value,
        );

        ensure!(
            target.len() == H160::len_bytes(),
            CallFailure::revert(InvalidTarget, overheads)
        );
        let target_decoded = Decode::decode(&mut target.as_ref())
            .map_err(|_| CallFailure::revert(InvalidTarget, overheads))?;
        let bounded_input = EthereumTxInput::try_from(input)
            .map_err(|_| CallFailure::revert(InputTooLarge, overheads))?;

        let value_u256 = U256::from(value);
        // With overheads, less weight is available.
        let weight_limit = context.weight_limit.saturating_sub(overheads);
        let gas_limit = U256::from(T::GasWeightMapping::weight_to_gas(weight_limit));

        let source = T::AccountMapping::into_h160(source);
        let tx = CheckedEthereumTx {
            gas_limit,
            target: target_decoded,
            value: value_u256,
            input: bounded_input,
            maybe_access_list: None,
        };

        // Note the skip execution check should be exactly before `T::EthereumTransact::xvm_transact`
        // to benchmark the correct overheads.
        if skip_execution {
            return Ok(CallOutput::new(vec![], overheads));
        }

        let transact_result = T::EthereumTransact::xvm_transact(source, tx);
        log::trace!(
            target: "xvm::evm_call",
            "EVM call result: {:?}", transact_result,
        );

        match transact_result {
            Ok((post_dispatch_info, call_info)) => {
                let used_weight = post_dispatch_info
                    .actual_weight
                    .unwrap_or_default()
                    .saturating_add(overheads);
                match call_info.exit_reason {
                    ExitReason::Succeed(_) => Ok(CallOutput::new(call_info.value, used_weight)),
                    ExitReason::Revert(_) => {
                        // On revert, the `call_info.value` is the encoded error data. Refer to Contract
                        // ABI specification for details. https://docs.soliditylang.org/en/latest/abi-spec.html#errors
                        Err(CallFailure::revert(VmRevert(call_info.value), used_weight))
                    }
                    ExitReason::Error(err) => Err(CallFailure::error(
                        VmError(format!("EVM call error: {:?}", err).into()),
                        used_weight,
                    )),
                    ExitReason::Fatal(err) => Err(CallFailure::error(
                        VmError(format!("EVM call error: {:?}", err).into()),
                        used_weight,
                    )),
                }
            }
            Err(e) => {
                let used_weight = e
                    .post_info
                    .actual_weight
                    .unwrap_or_default()
                    .saturating_add(overheads);
                Err(CallFailure::error(
                    VmError(format!("EVM call error: {:?}", e.error).into()),
                    used_weight,
                ))
            }
        }
    }

    fn wasm_call(
        context: Context,
        source: T::AccountId,
        target: Vec<u8>,
        input: Vec<u8>,
        value: Balance,
        overheads: Weight,
        storage_deposit_limit: Option<Balance>,
        skip_execution: bool,
    ) -> CallResult {
        log::trace!(
            target: "xvm::wasm_call",
            "Calling WASM: {:?} {:?}, {:?}, {:?}, {:?}, {:?}",
            context, source, target, input, value, storage_deposit_limit,
        );

        let dest = {
            let error = CallFailure::revert(InvalidTarget, overheads);
            Decode::decode(&mut target.as_ref()).map_err(|_| error.clone())
        }?;

        // With overheads, less weight is available.
        let weight_limit = context.weight_limit.saturating_sub(overheads);

        // Note the skip execution check should be exactly before `pallet_contracts::bare_call`
        // to benchmark the correct overheads.
        if skip_execution {
            return Ok(CallOutput::new(vec![], overheads));
        }

        let call_result = pallet_contracts::Pallet::<T>::bare_call(
            source,
            dest,
            value,
            weight_limit,
            storage_deposit_limit,
            input,
            DebugInfo::Skip,
            CollectEvents::Skip,
            Determinism::Enforced,
        );
        log::trace!(target: "xvm::wasm_call", "WASM call result: {:?}", call_result);

        let used_weight = call_result.gas_consumed.saturating_add(overheads);
        match call_result.result {
            Ok(val) => {
                if val.flags.contains(ReturnFlags::REVERT) {
                    Err(CallFailure::revert(VmRevert(val.data), used_weight))
                } else {
                    Ok(CallOutput::new(val.data, used_weight))
                }
            }
            Err(error) => Err(CallFailure::error(
                VmError(format!("WASM call error: {:?}", error).into()),
                used_weight,
            )),
        }
    }

    #[cfg(feature = "runtime-benchmarks")]
    pub fn call_without_execution(
        context: Context,
        vm_id: VmId,
        source: T::AccountId,
        target: Vec<u8>,
        input: Vec<u8>,
        value: Balance,
        storage_deposit_limit: Option<Balance>,
    ) -> CallResult {
        Self::do_call(
            context,
            vm_id,
            source,
            target,
            input,
            value,
            storage_deposit_limit,
            true,
        )
    }
}
