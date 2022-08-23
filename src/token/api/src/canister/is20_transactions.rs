use candid::Principal;
use ic_canister::ic_kit::ic;
use ic_helpers::ledger::{AccountIdentifier, Subaccount as SubaccountIdentifier};
use ic_helpers::tokens::Tokens128;

use crate::account::{Account, CheckedAccount, Subaccount, WithRecipient};
use crate::error::TxError;
use crate::principal::{CheckedPrincipal, Owner, TestNet};
use crate::state::{Balances, CanisterState, FeeRatio};
use crate::types::{BatchTransferArgs, TransferArgs, TxId, TxReceipt};

use super::icrc1_transfer::{PERMITTED_DRIFT, TX_WINDOW};
use super::is20_auction::auction_account;
use super::TokenCanisterAPI;

pub(crate) fn is20_transfer(
    canister: &impl TokenCanisterAPI,
    caller: CheckedAccount<WithRecipient>,
    transfer: &TransferArgs,
) -> TxReceipt {
    let from = caller.inner();
    let to = caller.recipient();
    let created_at_time = validate_and_get_tx_ts(canister, from.owner, transfer)?;
    let TransferArgs { amount, memo, .. } = transfer;

    let state = canister.state();
    let mut state = state.borrow_mut();
    let CanisterState {
        ref mut balances,
        ref bidding_state,
        ref stats,
        ..
    } = &mut *state;

    let (fee, fee_to) = stats.fee_info();
    let fee_ratio = bidding_state.fee_ratio;

    if let Some(requested_fee) = transfer.fee {
        if fee != requested_fee {
            return Err(TxError::BadFee { expected_fee: fee });
        }
    }

    transfer_internal(balances, from, to, *amount, fee, fee_to.into(), fee_ratio)?;

    let id = state
        .ledger
        .transfer(from, to, *amount, fee, *memo, created_at_time);
    Ok(id.into())
}

pub(crate) fn transfer_internal(
    balances: &mut Balances,
    from: Account,
    to: Account,
    amount: Tokens128,
    fee: Tokens128,
    fee_to: Account,
    auction_fee_ratio: FeeRatio,
) -> Result<(), TxError> {
    // We use `updaets` structure because sometimes from or to can be equal to fee_to or even to
    // auction_account, so we must take carefull approach.
    let mut updates = Balances::default();
    updates.set_balance(from, balances.balance_of(from));
    updates.set_balance(to, balances.balance_of(to));
    updates.set_balance(fee_to, balances.balance_of(fee_to));
    updates.set_balance(auction_account(), balances.balance_of(auction_account()));

    let from_balance = updates.balance_of(from);

    // If `amount + fee` overflows max `Tokens128` value, the balance cannot be larger then this
    // value, so we can safely return `InsufficientFunds` error.
    let amount_with_fee = (amount + fee).ok_or(TxError::InsufficientFunds {
        balance: from_balance,
    })?;

    let updated_from_balance =
        (from_balance - amount_with_fee).ok_or(TxError::InsufficientFunds {
            balance: from_balance,
        })?;
    updates.set_balance(from, updated_from_balance);

    let to_balance = updates.balance_of(to);
    let updated_to_balance = (to_balance + amount).ok_or(TxError::AmountOverflow)?;
    updates.set_balance(to, updated_to_balance);

    let (owner_fee, auction_fee) = auction_fee_ratio.get_value(fee);

    let fee_to_balance = updates.balance_of(fee_to);
    let updated_fee_to_balance = (fee_to_balance + owner_fee).ok_or(TxError::AmountOverflow)?;
    updates.set_balance(fee_to, updated_fee_to_balance);

    let auction_balance = updates.balance_of(auction_account());
    let updated_auction_balance = (auction_balance + auction_fee).ok_or(TxError::AmountOverflow)?;
    updates.set_balance(auction_account(), updated_auction_balance);

    // At this point all the checks are done and no further errors are possible, so we modify the
    // canister state only at this point.

    balances.apply_change(&updates);

    Ok(())
}

/// Transfers `value` amount to the `to` principal, applying American style fee. This means, that
/// the recipient will receive `value - fee`, and the sender account will be reduced exactly by `value`.
///
/// Note, that the `value` cannot be less than the `fee` amount. If the value given is too small,
/// transaction will fail with `TxError::AmountTooSmall` error.
pub fn transfer_include_fee(
    canister: &impl TokenCanisterAPI,
    from: CheckedAccount<WithRecipient>,
    transfer_args: &TransferArgs,
) -> TxReceipt {
    let (fee, _) = canister.state().borrow().stats.fee_info();
    let adjusted_amount = (transfer_args.amount - fee).ok_or(TxError::AmountTooSmall)?;
    if adjusted_amount.is_zero() {
        return Err(TxError::AmountTooSmall);
    }

    is20_transfer(canister, from, &transfer_args.with_amount(adjusted_amount))
}

fn validate_and_get_tx_ts(
    canister: &impl TokenCanisterAPI,
    caller: Principal,
    transfer_args: &TransferArgs,
) -> Result<u64, TxError> {
    let now = ic::time();
    let from = Account::new(caller, transfer_args.from_subaccount);
    let to = transfer_args.to;

    let created_at_time = match transfer_args.created_at_time {
        Some(created_at_time) => {
            if now.saturating_sub(created_at_time) > TX_WINDOW {
                return Err(TxError::TooOld {
                    allowed_window_nanos: TX_WINDOW,
                });
            }

            if created_at_time.saturating_sub(now) > PERMITTED_DRIFT {
                return Err(TxError::CreatedInFuture { ledger_time: now });
            }

            for tx in canister.state().borrow().ledger.iter().rev() {
                if now.saturating_sub(tx.timestamp) > TX_WINDOW {
                    break;
                }

                if tx.timestamp == created_at_time
                    && tx.from == from
                    && tx.to == to
                    && tx.memo == transfer_args.memo
                    && tx.amount == transfer_args.amount
                    && tx.fee == transfer_args.fee.unwrap_or(tx.fee)
                {
                    return Err(TxError::Duplicate {
                        duplicate_of: tx.index,
                    });
                }
            }

            created_at_time
        }

        None => now,
    };

    Ok(created_at_time)
}

pub fn mint(
    state: &mut CanisterState,
    caller: Principal,
    to: Account,
    amount: Tokens128,
) -> TxReceipt {
    let balance = state.balances.get_mut_or_insert_default(to);

    let new_balance = (*balance + amount).ok_or(TxError::AmountOverflow)?;
    *balance = new_balance;

    let id = state.ledger.mint(caller.into(), to, amount);

    Ok(id.into())
}

pub fn mint_test_token(
    state: &mut CanisterState,
    caller: CheckedPrincipal<TestNet>,
    to: Principal,
    to_subaccount: Option<Subaccount>,
    amount: Tokens128,
) -> TxReceipt {
    mint(
        state,
        caller.inner(),
        Account::new(to, to_subaccount),
        amount,
    )
}

pub fn mint_as_owner(
    state: &mut CanisterState,
    caller: CheckedPrincipal<Owner>,
    to: Principal,
    to_subaccount: Option<Subaccount>,
    amount: Tokens128,
) -> TxReceipt {
    mint(
        state,
        caller.inner(),
        Account::new(to, to_subaccount),
        amount,
    )
}

pub fn burn(
    state: &mut CanisterState,
    caller: Principal,
    from: Account,
    amount: Tokens128,
) -> TxReceipt {
    let balance = state.balances.balance_of(from);

    if !amount.is_zero() && balance == Tokens128::ZERO {
        return Err(TxError::InsufficientFunds { balance });
    }

    let new_balance = (balance - amount).ok_or(TxError::InsufficientFunds { balance })?;

    if new_balance == Tokens128::ZERO {
        state.balances.remove(from)
    } else {
        state.balances.set_balance(from, new_balance)
    }

    let id = state.ledger.burn(caller.into(), from, amount);
    Ok(id.into())
}

pub fn burn_own_tokens(
    state: &mut CanisterState,
    from_subaccount: Option<Subaccount>,
    amount: Tokens128,
) -> TxReceipt {
    let caller = ic::caller();
    burn(state, caller, Account::new(caller, from_subaccount), amount)
}

pub fn burn_as_owner(
    state: &mut CanisterState,
    caller: CheckedPrincipal<Owner>,
    from: Principal,
    from_subaccount: Option<Subaccount>,
    amount: Tokens128,
) -> TxReceipt {
    burn(
        state,
        caller.inner(),
        Account::new(from, from_subaccount),
        amount,
    )
}

pub fn mint_to_accountid(
    state: &mut CanisterState,
    to: AccountIdentifier,
    amount: Tokens128,
) -> Result<(), TxError> {
    let balance = state.claims.entry(to).or_default();
    let new_balance = (*balance + amount).ok_or(TxError::AmountOverflow)?;
    *balance = new_balance;
    Ok(())
}

pub fn claim(
    state: &mut CanisterState,
    account: AccountIdentifier,
    subaccount: Option<Subaccount>,
) -> TxReceipt {
    let caller = ic_canister::ic_kit::ic::caller();
    let amount = state.claim_amount(account);

    if account
        != AccountIdentifier::new(
            caller.into(),
            Some(SubaccountIdentifier(subaccount.unwrap_or_default())),
        )
    {
        return Err(TxError::ClaimNotAllowed);
    }
    let to = Account::new(caller, subaccount);

    let id = mint(state, caller, to, amount);

    state.claims.remove(&account);

    id
}

pub fn batch_transfer(
    canister: &impl TokenCanisterAPI,
    from_subaccount: Option<Subaccount>,
    transfers: Vec<BatchTransferArgs>,
) -> Result<Vec<TxId>, TxError> {
    let caller = ic_canister::ic_kit::ic::caller();
    let from = Account::new(caller, from_subaccount);
    let state = canister.state();
    let mut state = state.borrow_mut();
    let CanisterState {
        ref mut balances,
        ref bidding_state,
        ref mut ledger,
        ref stats,
        ..
    } = &mut *state;

    let (fee, fee_to) = stats.fee_info();
    let fee_to = Account::new(fee_to, None);
    let auction_fee_ratio = bidding_state.fee_ratio;

    let mut updated_balances = Balances::default();
    updated_balances.set_balance(from, balances.balance_of(from));
    updated_balances.set_balance(fee_to, balances.balance_of(fee_to));
    updated_balances.set_balance(auction_account(), balances.balance_of(auction_account()));

    for transfer in &transfers {
        updated_balances.set_balance(transfer.receiver, balances.balance_of(transfer.receiver));
    }

    for transfer in &transfers {
        transfer_internal(
            &mut updated_balances,
            from,
            transfer.receiver,
            transfer.amount,
            fee,
            fee_to,
            auction_fee_ratio,
        )
        .map_err(|err| match err {
            TxError::InsufficientFunds { .. } => TxError::InsufficientFunds {
                balance: balances.balance_of(from),
            },
            other => other,
        })?;
    }

    balances.apply_change(&updated_balances);

    let id = ledger.batch_transfer(from, transfers, fee);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use ic_canister::ic_kit::mock_principals::{alice, bob, john, xtc};
    use ic_canister::ic_kit::MockContext;
    use ic_canister::Canister;
    use rand::{thread_rng, Rng};

    use crate::mock::TokenCanisterMock;
    use crate::types::Metadata;

    use super::*;

    #[cfg(coverage_nightly)]
    use coverage_helper::test;

    // Method for generating random Subaccount.
    fn gen_subaccount() -> Subaccount {
        // generate a random subaccount
        let mut subaccount = [0u8; 32];
        thread_rng().fill(&mut subaccount);
        subaccount
    }

    fn test_canister() -> TokenCanisterMock {
        MockContext::new().with_caller(alice()).inject();

        let canister = TokenCanisterMock::init_instance();
        canister.init(
            Metadata {
                logo: "".to_string(),
                name: "".to_string(),
                symbol: "".to_string(),
                decimals: 8,
                owner: alice(),
                fee: Tokens128::from(0),
                feeTo: alice(),
                isTestToken: None,
            },
            Tokens128::from(1000),
        );

        // This is to make tests that don't rely on auction state
        // pass, because since we are running auction state on each
        // endpoint call, it affects `BiddingInfo.fee_ratio` that is
        // used for charging fees in `approve` endpoint.
        canister.state.borrow_mut().stats.min_cycles = 0;

        canister
    }

    #[test]
    fn batch_transfer_without_fee() {
        let canister = test_canister();
        assert_eq!(
            Tokens128::from(1000),
            canister.icrc1_balance_of(Account::new(alice(), None))
        );
        let transfer1 = BatchTransferArgs {
            receiver: Account {
                owner: bob(),
                subaccount: None,
            },
            amount: Tokens128::from(100),
        };
        let transfer2 = BatchTransferArgs {
            receiver: Account {
                owner: john(),
                subaccount: None,
            },
            amount: Tokens128::from(200),
        };
        let receipt = canister
            .batchTransfer(None, vec![transfer1, transfer2])
            .unwrap();
        assert_eq!(receipt.len(), 2);
        assert_eq!(
            canister.icrc1_balance_of(Account::new(alice(), None)),
            Tokens128::from(700)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), None)),
            Tokens128::from(100)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(john(), None)),
            Tokens128::from(200)
        );
    }

    #[test]
    fn batch_transfer_with_fee() {
        let canister = test_canister();
        let mut state = canister.state.borrow_mut();
        state.stats.fee = Tokens128::from(50);
        state.stats.fee_to = john();
        drop(state);
        assert_eq!(
            Tokens128::from(1000),
            canister.icrc1_balance_of(Account::new(alice(), None))
        );
        let transfer1 = BatchTransferArgs {
            receiver: Account {
                owner: bob(),
                subaccount: None,
            },
            amount: Tokens128::from(100),
        };
        let transfer2 = BatchTransferArgs {
            receiver: Account {
                owner: xtc(),
                subaccount: None,
            },
            amount: Tokens128::from(200),
        };
        let receipt = canister
            .batchTransfer(None, vec![transfer1, transfer2])
            .unwrap();
        assert_eq!(receipt.len(), 2);
        assert_eq!(
            canister.icrc1_balance_of(Account::new(alice(), None)),
            Tokens128::from(600)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), None)),
            Tokens128::from(100)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(xtc(), None)),
            Tokens128::from(200)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(john(), None)),
            Tokens128::from(100)
        );
    }

    #[test]
    fn batch_transfer_insufficient_balance() {
        let canister = test_canister();

        let transfer1 = BatchTransferArgs {
            receiver: Account {
                owner: bob(),
                subaccount: None,
            },
            amount: Tokens128::from(500),
        };
        let transfer2 = BatchTransferArgs {
            receiver: Account {
                owner: john(),
                subaccount: None,
            },
            amount: Tokens128::from(600),
        };
        let receipt = canister.batchTransfer(None, vec![transfer1, transfer2]);
        assert!(receipt.is_err());
        let balance = canister.icrc1_balance_of(Account::new(alice(), None));
        assert_eq!(receipt.unwrap_err(), TxError::InsufficientFunds { balance });
        assert_eq!(
            canister.icrc1_balance_of(Account::new(alice(), None)),
            Tokens128::from(1000)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), None)),
            Tokens128::from(0)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(john(), None)),
            Tokens128::from(0)
        );
    }

    #[test]
    fn transfer_without_fee() {
        let canister = test_canister();
        let bob_sub = gen_subaccount();
        assert_eq!(
            Tokens128::from(1000),
            canister.icrc1_balance_of(Account::new(alice(), None))
        );

        assert!(canister
            .transferIncludeFee(None, bob(), None, Tokens128::from(100), None, None)
            .is_ok());
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), None)),
            Tokens128::from(100)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(alice(), None)),
            Tokens128::from(900)
        );

        assert!(canister
            .transferIncludeFee(None, bob(), Some(bob_sub), Tokens128::from(100), None, None)
            .is_ok());
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), Some(bob_sub))),
            Tokens128::from(100)
        );
    }

    #[test]
    fn transfer_with_fee() {
        let bob_sub = gen_subaccount();
        let canister = test_canister();
        let mut state = canister.state.borrow_mut();
        state.stats.fee = Tokens128::from(100);
        state.stats.fee_to = john();
        drop(state);

        assert!(canister
            .transferIncludeFee(None, bob(), None, Tokens128::from(200), None, None)
            .is_ok());
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), None)),
            Tokens128::from(100)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(alice(), None)),
            Tokens128::from(800)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(john(), None)),
            Tokens128::from(100)
        );

        assert!(canister
            .transferIncludeFee(None, bob(), Some(bob_sub), Tokens128::from(150), None, None)
            .is_ok());
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), Some(bob_sub))),
            Tokens128::from(50)
        );
    }

    #[test]
    fn transfer_insufficient_balance() {
        let canister = test_canister();
        let balance = canister.icrc1_balance_of(Account::new(alice(), None));
        assert!(canister
            .transferIncludeFee(None, bob(), None, Tokens128::from(1001), None, None)
            .is_err());
        assert_eq!(
            canister.transferIncludeFee(None, bob(), None, Tokens128::from(1001), None, None),
            Err(TxError::InsufficientFunds { balance })
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(alice(), None)),
            Tokens128::from(1000)
        );
        assert_eq!(
            canister.icrc1_balance_of(Account::new(bob(), None)),
            Tokens128::from(0)
        );
    }

    #[test]
    fn deduplication_error() {
        let canister = test_canister();
        let curr_time = ic::time();

        let transfer = TransferArgs {
            from_subaccount: None,
            to: Account::new(bob(), None),
            amount: 10_000.into(),
            fee: None,
            memo: None,
            created_at_time: Some(curr_time),
        };

        assert!(validate_and_get_tx_ts(&canister, alice(), &transfer).is_ok());

        let tx_id = canister.icrc1_transfer(transfer.clone()).unwrap();

        assert_eq!(
            validate_and_get_tx_ts(&canister, alice(), &transfer),
            Err(TxError::Duplicate {
                duplicate_of: tx_id as u64
            })
        )
    }

    #[test]
    fn deduplicate_check_pass() {
        let canister = test_canister();
        let curr_time = ic::time();

        let transfer = TransferArgs {
            from_subaccount: None,
            to: Account::new(bob(), None),
            amount: 10_000.into(),
            fee: None,
            memo: None,
            created_at_time: Some(curr_time),
        };

        let _ = canister.icrc1_transfer(transfer.clone()).unwrap();
        assert!(validate_and_get_tx_ts(&canister, john(), &transfer).is_ok());

        let mut tx = transfer.clone();
        tx.from_subaccount = Some([0; 32]);
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());

        let mut tx = transfer.clone();
        tx.amount = 10_001.into();
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());

        let mut tx = transfer.clone();
        tx.fee = Some(0.into());
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());

        let mut tx = transfer.clone();
        tx.memo = Some([0; 32]);
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());

        let mut tx = transfer.clone();
        tx.created_at_time = None;
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());

        let mut tx = transfer;
        tx.created_at_time = Some(curr_time + 1);
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());

        let transfer = TransferArgs {
            from_subaccount: None,
            to: Account::new(bob(), None),
            amount: 10_000.into(),
            fee: None,
            memo: Some([1; 32]),
            created_at_time: Some(curr_time),
        };

        let _ = canister.icrc1_transfer(transfer.clone()).unwrap();
        assert!(validate_and_get_tx_ts(&canister, john(), &transfer).is_ok());

        let mut tx = transfer.clone();
        tx.memo = None;
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());

        let mut tx = transfer;
        tx.memo = Some([2; 32]);
        assert!(validate_and_get_tx_ts(&canister, john(), &tx).is_ok());
    }

    #[test]
    fn deduplicate_check_no_created_at_time() {
        let canister = test_canister();

        let transfer = TransferArgs {
            from_subaccount: None,
            to: Account::new(bob(), None),
            amount: 10_000.into(),
            fee: None,
            memo: None,
            created_at_time: None,
        };

        let _ = canister.icrc1_transfer(transfer.clone()).unwrap();
        assert!(validate_and_get_tx_ts(&canister, alice(), &transfer).is_ok());
    }
}
