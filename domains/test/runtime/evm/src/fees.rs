use crate::{AccountId, Balance, Balances, BlockFees, Runtime, RuntimeCall};
use codec::Encode;
use frame_support::traits::{Currency, ExistenceRequirement, Get, Imbalance, WithdrawReasons};
use pallet_balances::NegativeImbalance;
use sp_runtime::traits::{DispatchInfoOf, PostDispatchInfoOf, Zero};
use sp_runtime::transaction_validity::{InvalidTransaction, TransactionValidityError};

pub struct DomainTransactionByteFee;

impl Get<Balance> for DomainTransactionByteFee {
    fn get() -> Balance {
        BlockFees::domain_transaction_byte_fee()
    }
}

pub struct LiquidityInfo {
    storage_fee: Balance,
    imbalance: NegativeImbalance<Runtime>,
}

/// Implementation of [`pallet_transaction_payment::OnChargeTransaction`] that charges transaction
/// fees and distributes storage/compute fees and tip separately.
pub struct OnChargeDomainTransaction;

impl pallet_transaction_payment::OnChargeTransaction<Runtime> for OnChargeDomainTransaction {
    type LiquidityInfo = Option<LiquidityInfo>;
    type Balance = Balance;

    fn withdraw_fee(
        who: &AccountId,
        call: &RuntimeCall,
        _info: &DispatchInfoOf<RuntimeCall>,
        fee: Self::Balance,
        tip: Self::Balance,
    ) -> Result<Self::LiquidityInfo, TransactionValidityError> {
        if fee.is_zero() {
            return Ok(None);
        }

        let withdraw_reason = if tip.is_zero() {
            WithdrawReasons::TRANSACTION_PAYMENT
        } else {
            WithdrawReasons::TRANSACTION_PAYMENT | WithdrawReasons::TIP
        };

        let withdraw_result = <Balances as Currency<AccountId>>::withdraw(
            who,
            fee,
            withdraw_reason,
            ExistenceRequirement::KeepAlive,
        );
        let imbalance = withdraw_result.map_err(|_error| InvalidTransaction::Payment)?;

        // Separate storage fee while we have access to the call data structure to calculate it.
        let storage_fee = DomainTransactionByteFee::get()
            * Balance::try_from(call.encoded_size())
                .expect("Size of the call never exceeds balance units; qed");

        Ok(Some(LiquidityInfo {
            storage_fee,
            imbalance,
        }))
    }

    fn correct_and_deposit_fee(
        who: &AccountId,
        _dispatch_info: &DispatchInfoOf<RuntimeCall>,
        _post_info: &PostDispatchInfoOf<RuntimeCall>,
        corrected_fee: Self::Balance,
        _tip: Self::Balance,
        liquidity_info: Self::LiquidityInfo,
    ) -> Result<(), TransactionValidityError> {
        if let Some(LiquidityInfo {
            storage_fee,
            imbalance,
        }) = liquidity_info
        {
            // Calculate how much refund we should return
            let refund_amount = imbalance.peek().saturating_sub(corrected_fee);
            // Refund to the the account that paid the fees. If this fails, the account might have
            // dropped below the existential balance. In that case we don't refund anything.
            let refund_imbalance = Balances::deposit_into_existing(who, refund_amount)
                .unwrap_or_else(|_| <Balances as Currency<AccountId>>::PositiveImbalance::zero());
            // Merge the imbalance caused by paying the fees and refunding parts of it again.
            let adjusted_paid = imbalance
                .offset(refund_imbalance)
                .same()
                .map_err(|_| TransactionValidityError::Invalid(InvalidTransaction::Payment))?;

            // Split paid storage and th compute fees so that they can be distributed separately.
            let (paid_storage_fee, compute_fee_and_tip) = adjusted_paid.split(storage_fee);

            BlockFees::note_storage_fee(paid_storage_fee.peek());
            BlockFees::note_execution_fee(compute_fee_and_tip.peek());
        }
        Ok(())
    }
}
