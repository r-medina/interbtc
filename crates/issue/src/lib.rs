//! # Issue Module
//! Based on the [specification](https://interlay.gitlab.io/polkabtc-spec/spec/issue.html).

#![deny(warnings)]
#![cfg_attr(test, feature(proc_macro_hygiene))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(any(feature = "runtime-benchmarks", test))]
mod benchmarking;

mod default_weights;
pub use default_weights::WeightInfo;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(test)]
extern crate mocktopus;

#[cfg(test)]
use mocktopus::macros::mockable;

mod ext;
pub mod types;

#[doc(inline)]
pub use crate::types::{IssueRequest, IssueRequestStatus};

use crate::types::{Backing, IssueRequestV2, Issuing, Version};
use bitcoin::utils::sha256d_le;
use btc_relay::{BtcAddress, BtcPublicKey};
use frame_support::{
    decl_error, decl_event, decl_module, decl_storage,
    dispatch::{DispatchError, DispatchResult},
    ensure, transactional,
    weights::Weight,
};
use frame_system::{ensure_root, ensure_signed};
use primitive_types::H256;
use sp_runtime::{traits::*, ModuleId};
use sp_std::{convert::TryInto, vec::Vec};
use vault_registry::{CurrencySource, VaultStatus};

/// The issue module id, used for deriving its sovereign account ID.
const _MODULE_ID: ModuleId = ModuleId(*b"issuemod");

/// The pallet's configuration trait.
pub trait Config:
    frame_system::Config
    + vault_registry::Config
    + currency::Config<currency::Collateral>
    + currency::Config<currency::Treasury>
    + btc_relay::Config
    + exchange_rate_oracle::Config
    + fee::Config
    + sla::Config
    + refund::Config
{
    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;

    /// Weight information for the extrinsics in this module.
    type WeightInfo: WeightInfo;
}

// The pallet's storage items.
decl_storage! {
    trait Store for Module<T: Config> as Issue {
        /// Users create issue requests to issue tokens. This mapping provides access
        /// from a unique hash `IssueId` to an `IssueRequest` struct.
        IssueRequests: map hasher(blake2_128_concat) H256 => IssueRequest<T::AccountId, T::BlockNumber, Issuing<T>, Backing<T>>;

        /// The time difference in number of blocks between an issue request is created
        /// and required completion time by a user. The issue period has an upper limit
        /// to prevent griefing of vault collateral.
        IssuePeriod get(fn issue_period) config(): T::BlockNumber;

        /// Build storage at V1 (requires default 0).
        StorageVersion get(fn storage_version) build(|_| Version::V2): Version = Version::V0;
    }
}

// The pallet's events.
decl_event!(
    pub enum Event<T>
    where
        AccountId = <T as frame_system::Config>::AccountId,
        Issuing = Issuing<T>,
        Backing = Backing<T>,
    {
        RequestIssue(
            H256,         // issue_id
            AccountId,    // requester
            Issuing,      // amount
            Issuing,      // fee
            Backing,      // griefing_collateral
            AccountId,    // vault_id
            BtcAddress,   // vault deposit address
            BtcPublicKey, // vault public key
        ),
        // issue_id, amount, fee, confiscated_griefing_collateral
        IssueAmountChange(H256, Issuing, Issuing, Backing),
        // [issue_id, requester, total_amount, vault]
        ExecuteIssue(H256, AccountId, Issuing, AccountId),
        // [issue_id, requester, griefing_collateral]
        CancelIssue(H256, AccountId, Backing),
    }
);

// The pallet's dispatchable functions.
decl_module! {
    /// The module declaration.
    pub struct Module<T: Config> for enum Call where origin: T::Origin {
        type Error = Error<T>;

        // Initializing events
        // this is needed only if you are using events in your pallet
        fn deposit_event() = default;

        /// Upgrade the runtime depending on the current `StorageVersion`.
        fn on_runtime_upgrade() -> Weight {
            use frame_support::{migration::StorageKeyIterator, Blake2_128Concat};

            if matches!(Self::storage_version(), Version::V2) {
                StorageKeyIterator::<H256, IssueRequestV2<T::AccountId, T::BlockNumber, Issuing<T>, Backing<T>>, Blake2_128Concat>::new(<IssueRequests<T>>::module_prefix(), b"IssueRequests")
                    .drain()
                    .for_each(|(id, old_request)| {
                        let new_request = IssueRequest {
                            vault: old_request.vault,
                            opentime: old_request.opentime,
                            period: Self::issue_period(),
                            griefing_collateral: old_request.griefing_collateral,
                            amount: old_request.amount,
                            fee: old_request.fee,
                            requester: old_request.requester,
                            btc_address: old_request.btc_address,
                            btc_public_key: old_request.btc_public_key,
                            btc_height: 1969929, // extra conservative, testnet height at april 4th
                            status: old_request.status,
                        };
                        <IssueRequests<T>>::insert(id, new_request);
                    });

                StorageVersion::put(Version::V3);
            }

            0
        }


        /// Request the issuance of tokens
        ///
        /// # Arguments
        ///
        /// * `origin` - sender of the transaction
        /// * `amount` - amount of BTC the user wants to convert to tokens. Note that the amount of
        /// tokens received will be less, because a fee is subtracted.
        /// * `vault` - address of the vault
        /// * `griefing_collateral` - amount of collateral
        #[weight = <T as Config>::WeightInfo::request_issue()]
        #[transactional]
        fn request_issue(
            origin,
            #[compact] amount: Issuing<T>,
            vault_id: T::AccountId,
            #[compact] griefing_collateral: Backing<T>
        ) -> DispatchResult {
            let requester = ensure_signed(origin)?;
            Self::_request_issue(requester, amount, vault_id, griefing_collateral)?;
            Ok(())
        }

        /// Finalize the issuance of tokens
        ///
        /// # Arguments
        ///
        /// * `origin` - sender of the transaction
        /// * `issue_id` - identifier of issue request as output from request_issue
        /// * `tx_block_height` - block number of backing chain
        /// * `merkle_proof` - raw bytes
        /// * `raw_tx` - raw bytes
        #[weight = <T as Config>::WeightInfo::execute_issue()]
        #[transactional]
        fn execute_issue(origin, issue_id: H256, merkle_proof: Vec<u8>, raw_tx: Vec<u8>)
            -> DispatchResult
        {
            let executor = ensure_signed(origin)?;
            Self::_execute_issue(executor, issue_id, merkle_proof, raw_tx)?;
            Ok(())
        }

        /// Cancel the issuance of tokens if expired
        ///
        /// # Arguments
        ///
        /// * `origin` - sender of the transaction
        /// * `issue_id` - identifier of issue request as output from request_issue
        #[weight = <T as Config>::WeightInfo::cancel_issue()]
        #[transactional]
        fn cancel_issue(origin, issue_id: H256)
            -> DispatchResult
        {
            let requester = ensure_signed(origin)?;
            Self::_cancel_issue(requester, issue_id)?;
            Ok(())
        }

        /// Set the default issue period for tx verification.
        ///
        /// # Arguments
        ///
        /// * `origin` - the dispatch origin of this call (must be _Root_)
        /// * `period` - default period for new requests
        ///
        /// # Weight: `O(1)`
        #[weight = <T as Config>::WeightInfo::set_issue_period()]
        #[transactional]
        fn set_issue_period(origin, period: T::BlockNumber) {
            ensure_root(origin)?;
            <IssuePeriod<T>>::set(period);
        }
    }
}

// "Internal" functions, callable by code.
#[cfg_attr(test, mockable)]
impl<T: Config> Module<T> {
    /// Requests CBA issuance, returns unique tracking ID.
    fn _request_issue(
        requester: T::AccountId,
        amount_requested: Issuing<T>,
        vault_id: T::AccountId,
        griefing_collateral: Backing<T>,
    ) -> Result<H256, DispatchError> {
        // Check that Parachain is RUNNING
        ext::security::ensure_parachain_status_not_shutdown::<T>()?;

        ensure!(
            ext::btc_relay::is_fully_initialized::<T>()?,
            Error::<T>::WaitingForRelayerInitialization
        );

        let vault = ext::vault_registry::get_active_vault_from_id::<T>(&vault_id)?;

        // ensure that the vault is accepting new issues
        ensure!(
            vault.status == VaultStatus::Active(true),
            Error::<T>::VaultNotAcceptingNewIssues
        );

        // Check that the vault is currently not banned
        ext::vault_registry::ensure_not_banned::<T>(&vault_id)?;

        // calculate griefing collateral based on the total amount of tokens to be issued
        let amount_dot = ext::oracle::btc_to_dots::<T>(amount_requested)?;
        let expected_griefing_collateral = ext::fee::get_issue_griefing_collateral::<T>(amount_dot)?;

        ensure!(
            griefing_collateral >= expected_griefing_collateral,
            Error::<T>::InsufficientCollateral
        );
        ext::collateral::lock_collateral::<T>(&requester, griefing_collateral)?;

        ext::vault_registry::try_increase_to_be_issued_tokens::<T>(&vault_id, amount_requested)?;

        let fee = ext::fee::get_issue_fee::<T>(amount_requested)?;
        // calculate the amount of polkabtc that will be transferred to the user upon execution
        let amount_user = amount_requested
            .checked_sub(&fee)
            .ok_or(Error::<T>::ArithmeticUnderflow)?;

        let issue_id = ext::security::get_secure_id::<T>(&requester);
        let btc_address = ext::vault_registry::register_deposit_address::<T>(&vault_id, issue_id)?;

        let request = IssueRequest {
            vault: vault_id,
            opentime: ext::security::active_block_number::<T>(),
            requester,
            btc_address,
            btc_public_key: vault.wallet.public_key,
            amount: amount_user,
            fee,
            griefing_collateral,
            period: Self::issue_period(),
            btc_height: ext::btc_relay::get_best_block_height::<T>(),
            status: IssueRequestStatus::Pending,
        };
        Self::insert_issue_request(&issue_id, &request);

        Self::deposit_event(<Event<T>>::RequestIssue(
            issue_id,
            request.requester,
            request.amount,
            request.fee,
            request.griefing_collateral,
            request.vault,
            request.btc_address,
            request.btc_public_key,
        ));
        Ok(issue_id)
    }

    /// Completes CBA issuance, removing request from storage and minting token.
    fn _execute_issue(
        executor: T::AccountId,
        issue_id: H256,
        merkle_proof: Vec<u8>,
        raw_tx: Vec<u8>,
    ) -> Result<(), DispatchError> {
        // Check that Parachain is RUNNING
        ext::security::ensure_parachain_status_not_shutdown::<T>()?;

        let mut issue = Self::get_issue_request_from_id(&issue_id)?;
        let mut maybe_refund_id = None;
        // allow anyone to complete issue request
        let requester = issue.requester.clone();

        // only executable before the request has expired
        ensure!(
            !ext::security::has_expired::<T>(issue.opentime, Self::issue_period().max(issue.period))?,
            Error::<T>::CommitPeriodExpired
        );

        let tx_id = sha256d_le(&raw_tx);
        ext::btc_relay::verify_transaction_inclusion::<T>(tx_id, merkle_proof)?;
        let (refund_address, amount_transferred) =
            ext::btc_relay::validate_transaction::<T>(raw_tx, None, issue.btc_address, None)?;

        let expected_total_amount = issue
            .amount
            .checked_add(&issue.fee)
            .ok_or(Error::<T>::ArithmeticOverflow)?;
        let amount_transferred = Self::u128_to_btc(amount_transferred as u128)?;

        // check for unexpected bitcoin amounts, and update the issue struct
        if amount_transferred < expected_total_amount {
            // only the requester of the issue can execute payments with insufficient amounts
            ensure!(requester == executor, Error::<T>::InvalidExecutor);

            // decrease the to-be-issued tokens that will not be issued after all
            let deficit = expected_total_amount
                .checked_sub(&amount_transferred)
                .ok_or(Error::<T>::ArithmeticUnderflow)?;
            ext::vault_registry::decrease_to_be_issued_tokens::<T>(&issue.vault, deficit)?;

            // slash/release griefing collateral proportionally to the amount sent
            let released_collateral = ext::vault_registry::calculate_collateral::<T>(
                issue.griefing_collateral,
                amount_transferred,
                expected_total_amount,
            )?;
            ext::collateral::release_collateral::<T>(&requester, released_collateral)?;
            let slashed_collateral = issue
                .griefing_collateral
                .checked_sub(&released_collateral)
                .ok_or(Error::<T>::ArithmeticUnderflow)?;
            ext::vault_registry::slash_collateral::<T>(
                CurrencySource::Griefing(issue.requester.clone()),
                CurrencySource::FreeBalance(ext::fee::fee_pool_account_id::<T>()),
                slashed_collateral,
            )?;
            ext::fee::increase_dot_rewards_for_epoch::<T>(slashed_collateral);

            Self::update_issue_amount(&issue_id, &mut issue, amount_transferred, slashed_collateral)?;
        } else {
            // release griefing collateral
            ext::collateral::release_collateral::<T>(&requester, issue.griefing_collateral)?;

            if amount_transferred > expected_total_amount
                && !ext::vault_registry::is_vault_liquidated::<T>(&issue.vault)?
            {
                let surplus_btc = amount_transferred
                    .checked_sub(&expected_total_amount)
                    .ok_or(Error::<T>::ArithmeticUnderflow)?;

                match ext::vault_registry::try_increase_to_be_issued_tokens::<T>(&issue.vault, surplus_btc) {
                    Ok(_) => {
                        // Current vault can handle the surplus; update the issue request
                        Self::update_issue_amount(&issue_id, &mut issue, amount_transferred, 0u32.into())?;
                    }
                    Err(_) => {
                        // vault does not have enough collateral to accept the over payment, so refund.
                        maybe_refund_id = ext::refund::request_refund::<T>(
                            surplus_btc,
                            issue.vault.clone(),
                            issue.requester,
                            refund_address,
                            issue_id,
                        )?;
                    }
                }
            }
        };

        // issue struct may have been update above; recalculate the total
        let total = issue
            .amount
            .checked_add(&issue.fee)
            .ok_or(Error::<T>::ArithmeticOverflow)?;
        ext::vault_registry::issue_tokens::<T>(&issue.vault, total)?;

        // mint polkabtc amount
        ext::treasury::mint::<T>(requester.clone(), issue.amount);

        // mint polkabtc fees
        ext::treasury::mint::<T>(ext::fee::fee_pool_account_id::<T>(), issue.fee);
        ext::fee::increase_polka_btc_rewards_for_epoch::<T>(issue.fee);

        if !ext::vault_registry::is_vault_liquidated::<T>(&issue.vault)? {
            // reward the vault for having issued tokens by increasing its sla
            ext::sla::event_update_vault_sla::<T>(&issue.vault, ext::sla::VaultEvent::ExecutedIssue(total))?;
        }

        // if it was a vault that did the execution on behalf of someone else, reward it by
        // increasing its SLA score
        if requester != executor {
            if let Ok(vault) = ext::vault_registry::get_active_vault_from_id::<T>(&executor) {
                ext::sla::event_update_vault_sla::<T>(&vault.id, ext::sla::VaultEvent::SubmittedIssueProof)?;
            }
        }

        Self::set_issue_status(issue_id, IssueRequestStatus::Completed(maybe_refund_id));

        Self::deposit_event(<Event<T>>::ExecuteIssue(
            issue_id,
            requester,
            expected_total_amount,
            issue.vault,
        ));
        Ok(())
    }

    /// Cancels CBA issuance if time has expired and slashes collateral.
    fn _cancel_issue(requester: T::AccountId, issue_id: H256) -> Result<(), DispatchError> {
        // Check that Parachain is RUNNING
        ext::security::ensure_parachain_status_not_shutdown::<T>()?;

        let issue = Self::get_issue_request_from_id(&issue_id)?;

        // only cancellable after the request has expired
        ensure!(
            ext::security::has_expired::<T>(issue.opentime, Self::issue_period().max(issue.period))?,
            Error::<T>::TimeNotExpired
        );

        // Decrease to-be-redeemed tokens:
        let full_amount = issue
            .amount
            .checked_add(&issue.fee)
            .ok_or(Error::<T>::ArithmeticOverflow)?;

        ext::vault_registry::decrease_to_be_issued_tokens::<T>(&issue.vault, full_amount)?;

        if ext::vault_registry::is_vault_liquidated::<T>(&issue.vault)? {
            ext::collateral::release_collateral::<T>(&issue.requester, issue.griefing_collateral)?;
        } else {
            ext::vault_registry::slash_collateral::<T>(
                CurrencySource::Griefing(issue.requester.clone()),
                CurrencySource::FreeBalance(ext::fee::fee_pool_account_id::<T>()),
                issue.griefing_collateral,
            )?;
            ext::fee::increase_dot_rewards_for_epoch::<T>(issue.griefing_collateral);
        }
        Self::set_issue_status(issue_id, IssueRequestStatus::Cancelled);

        Self::deposit_event(<Event<T>>::CancelIssue(issue_id, requester, issue.griefing_collateral));
        Ok(())
    }

    /// Fetch all issue requests for the specified account.
    ///
    /// # Arguments
    ///
    /// * `account_id` - user account id
    pub fn get_issue_requests_for_account(
        account_id: T::AccountId,
    ) -> Vec<(H256, IssueRequest<T::AccountId, T::BlockNumber, Issuing<T>, Backing<T>>)> {
        <IssueRequests<T>>::iter()
            .filter(|(_, request)| request.requester == account_id)
            .collect::<Vec<_>>()
    }

    /// Fetch all issue requests for the specified vault.
    ///
    /// # Arguments
    ///
    /// * `account_id` - vault account id
    pub fn get_issue_requests_for_vault(
        account_id: T::AccountId,
    ) -> Vec<(H256, IssueRequest<T::AccountId, T::BlockNumber, Issuing<T>, Backing<T>>)> {
        <IssueRequests<T>>::iter()
            .filter(|(_, request)| request.vault == account_id)
            .collect::<Vec<_>>()
    }

    pub fn get_issue_request_from_id(
        issue_id: &H256,
    ) -> Result<IssueRequest<T::AccountId, T::BlockNumber, Issuing<T>, Backing<T>>, DispatchError> {
        ensure!(<IssueRequests<T>>::contains_key(*issue_id), Error::<T>::IssueIdNotFound);

        let issue_request = <IssueRequests<T>>::get(issue_id);
        // NOTE: temporary workaround until we delete
        match issue_request.status {
            IssueRequestStatus::Completed(_) => Err(Error::<T>::IssueCompleted.into()),
            IssueRequestStatus::Cancelled => Err(Error::<T>::IssueCancelled.into()),
            IssueRequestStatus::Pending => Ok(issue_request),
        }
    }

    /// update the fee & amount in an issue request based on the actually transferred amount
    fn update_issue_amount(
        issue_id: &H256,
        issue: &mut IssueRequest<T::AccountId, T::BlockNumber, Issuing<T>, Backing<T>>,
        transferred_btc: Issuing<T>,
        confiscated_griefing_collateral: Backing<T>,
    ) -> Result<(), DispatchError> {
        // Current vault can handle the surplus; update the issue request
        issue.fee = ext::fee::get_issue_fee::<T>(transferred_btc)?;
        issue.amount = transferred_btc
            .checked_sub(&issue.fee)
            .ok_or(Error::<T>::ArithmeticUnderflow)?;

        // update storage
        <IssueRequests<T>>::mutate(issue_id, |x| {
            x.fee = issue.fee;
            x.amount = issue.amount;
        });

        Self::deposit_event(<Event<T>>::IssueAmountChange(
            *issue_id,
            issue.amount,
            issue.fee,
            confiscated_griefing_collateral,
        ));

        Ok(())
    }

    fn insert_issue_request(key: &H256, value: &IssueRequest<T::AccountId, T::BlockNumber, Issuing<T>, Backing<T>>) {
        <IssueRequests<T>>::insert(key, value)
    }

    fn set_issue_status(id: H256, status: IssueRequestStatus) {
        // TODO: delete issue request from storage
        <IssueRequests<T>>::mutate(id, |request| {
            request.status = status;
        });
    }

    fn u128_to_btc(x: u128) -> Result<Issuing<T>, DispatchError> {
        TryInto::<Issuing<T>>::try_into(x).map_err(|_| Error::<T>::TryIntoIntError.into())
    }
}

decl_error! {
    pub enum Error for Module<T: Config> {
        InsufficientCollateral,
        IssueIdNotFound,
        CommitPeriodExpired,
        TimeNotExpired,
        IssueCompleted,
        IssueCancelled,
        VaultNotAcceptingNewIssues,
        WaitingForRelayerInitialization,
        /// Unable to convert value
        TryIntoIntError,
        ArithmeticUnderflow,
        ArithmeticOverflow,
        InvalidExecutor,
    }
}
