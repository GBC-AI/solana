use log::*;
use solana_sdk::{
    account::Account,
    account_utils::StateMut,
    instruction::InstructionError,
    keyed_account::{from_keyed_account, get_signers, next_keyed_account, KeyedAccount},
    nonce,
    nonce_keyed_account::NonceKeyedAccount,
    process_instruction::InvokeContext,
    program_utils::limited_deserialize,
    pubkey::Pubkey,
    system_instruction::{SystemError, SystemInstruction, MAX_PERMITTED_DATA_LENGTH},
    system_program,
    sysvar::{self, recent_blockhashes::RecentBlockhashes, rent::Rent},
};
use std::collections::HashSet;

// represents an address that may or may not have been generated
//  from a seed
#[derive(PartialEq, Default, Debug)]
struct Address {
    address: Pubkey,
    base: Option<Pubkey>,
}

impl Address {
    fn is_signer(&self, signers: &HashSet<Pubkey>) -> bool {
        if let Some(base) = self.base {
            signers.contains(&base)
        } else {
            signers.contains(&self.address)
        }
    }
    fn create(
        address: &Pubkey,
        with_seed: Option<(&Pubkey, &str, &Pubkey)>,
    ) -> Result<Self, InstructionError> {
        let base = if let Some((base, seed, owner)) = with_seed {
            // re-derive the address, must match the supplied address
            if *address != Pubkey::create_with_seed(base, seed, owner)? {
                return Err(SystemError::AddressWithSeedMismatch.into());
            }
            Some(*base)
        } else {
            None
        };

        Ok(Self {
            address: *address,
            base,
        })
    }
}

fn allocate(
    account: &mut Account,
    address: &Address,
    space: u64,
    signers: &HashSet<Pubkey>,
) -> Result<(), InstructionError> {
    if !address.is_signer(signers) {
        debug!("Allocate: must carry signature of `to`");
        return Err(InstructionError::MissingRequiredSignature);
    }

    // if it looks like the `to` account is already in use, bail
    //   (note that the id check is also enforced by message_processor)
    if !account.data.is_empty() || !system_program::check_id(&account.owner) {
        debug!(
            "Allocate: invalid argument; account {:?} already in use",
            address
        );
        return Err(SystemError::AccountAlreadyInUse.into());
    }

    if space > MAX_PERMITTED_DATA_LENGTH {
        debug!(
            "Allocate: requested space: {} is more than maximum allowed",
            space
        );
        return Err(SystemError::InvalidAccountDataLength.into());
    }

    account.data = vec![0; space as usize];

    Ok(())
}

fn assign(
    account: &mut Account,
    address: &Address,
    owner: &Pubkey,
    signers: &HashSet<Pubkey>,
) -> Result<(), InstructionError> {
    // no work to do, just return
    if account.owner == *owner {
        return Ok(());
    }

    if !address.is_signer(&signers) {
        debug!("Assign: account must sign");
        return Err(InstructionError::MissingRequiredSignature);
    }

    // guard against sysvars being made
    if sysvar::check_id(&owner) {
        debug!("Assign: program id {} invalid", owner);
        return Err(SystemError::InvalidProgramId.into());
    }

    account.owner = *owner;
    Ok(())
}

fn allocate_and_assign(
    to: &mut Account,
    to_address: &Address,
    space: u64,
    owner: &Pubkey,
    signers: &HashSet<Pubkey>,
) -> Result<(), InstructionError> {
    allocate(to, to_address, space, signers)?;
    assign(to, to_address, owner, signers)
}

fn create_account(
    from: &KeyedAccount,
    to: &KeyedAccount,
    to_address: &Address,
    lamports: u64,
    space: u64,
    owner: &Pubkey,
    signers: &HashSet<Pubkey>,
) -> Result<(), InstructionError> {
    // if it looks like the `to` account is already in use, bail
    {
        let to = &mut to.try_account_ref_mut()?;
        if to.lamports > 0 {
            debug!(
                "Create Account: invalid argument; account {:?} already in use",
                to_address
            );
            return Err(SystemError::AccountAlreadyInUse.into());
        }

        allocate_and_assign(to, to_address, space, owner, signers)?;
    }
    transfer(from, to, lamports)
}

fn transfer_verified(
    from: &KeyedAccount,
    to: &KeyedAccount,
    lamports: u64,
) -> Result<(), InstructionError> {
    if !from.data_is_empty()? {
        debug!("Transfer: `from` must not carry data");
        return Err(InstructionError::InvalidArgument);
    }
    if lamports > from.lamports()? {
        debug!(
            "Transfer: insufficient lamports ({}, need {})",
            from.lamports()?,
            lamports
        );
        return Err(SystemError::ResultWithNegativeLamports.into());
    }

    from.try_account_ref_mut()?.lamports -= lamports;
    to.try_account_ref_mut()?.lamports += lamports;
    Ok(())
}

fn transfer(from: &KeyedAccount, to: &KeyedAccount, lamports: u64) -> Result<(), InstructionError> {
    if lamports == 0 {
        return Ok(());
    }

    if from.signer_key().is_none() {
        debug!("Transfer: from must sign");
        return Err(InstructionError::MissingRequiredSignature);
    }

    transfer_verified(from, to, lamports)
}

fn transfer_with_seed(
    from: &KeyedAccount,
    from_base: &KeyedAccount,
    from_seed: &str,
    from_owner: &Pubkey,
    to: &KeyedAccount,
    lamports: u64,
) -> Result<(), InstructionError> {
    if lamports == 0 {
        return Ok(());
    }

    if from_base.signer_key().is_none() {
        debug!("Transfer: from must sign");
        return Err(InstructionError::MissingRequiredSignature);
    }

    if *from.unsigned_key()
        != Pubkey::create_with_seed(from_base.unsigned_key(), from_seed, from_owner)?
    {
        return Err(SystemError::AddressWithSeedMismatch.into());
    }

    transfer_verified(from, to, lamports)
}

pub fn process_instruction(
    _owner: &Pubkey,
    keyed_accounts: &[KeyedAccount],
    instruction_data: &[u8],
    _invoke_context: &mut dyn InvokeContext,
) -> Result<(), InstructionError> {
    let instruction = limited_deserialize(instruction_data)?;

    trace!("process_instruction: {:?}", instruction);
    trace!("keyed_accounts: {:?}", keyed_accounts);

    let signers = get_signers(keyed_accounts);
    let keyed_accounts_iter = &mut keyed_accounts.iter();

    match instruction {
        SystemInstruction::CreateAccount {
            lamports,
            space,
            owner,
        } => {
            let from = next_keyed_account(keyed_accounts_iter)?;
            let to = next_keyed_account(keyed_accounts_iter)?;
            let to_address = Address::create(to.unsigned_key(), None)?;
            create_account(from, to, &to_address, lamports, space, &owner, &signers)
        }
        SystemInstruction::CreateAccountWithSeed {
            base,
            seed,
            lamports,
            space,
            owner,
        } => {
            let from = next_keyed_account(keyed_accounts_iter)?;
            let to = next_keyed_account(keyed_accounts_iter)?;
            let to_address = Address::create(&to.unsigned_key(), Some((&base, &seed, &owner)))?;
            create_account(from, &to, &to_address, lamports, space, &owner, &signers)
        }
        SystemInstruction::Assign { owner } => {
            let keyed_account = next_keyed_account(keyed_accounts_iter)?;
            let mut account = keyed_account.try_account_ref_mut()?;
            let address = Address::create(keyed_account.unsigned_key(), None)?;
            assign(&mut account, &address, &owner, &signers)
        }
        SystemInstruction::Transfer { lamports } => {
            let from = next_keyed_account(keyed_accounts_iter)?;
            let to = next_keyed_account(keyed_accounts_iter)?;
            transfer(from, to, lamports)
        }
        SystemInstruction::TransferWithSeed {
            lamports,
            from_seed,
            from_owner,
        } => {
            let from = next_keyed_account(keyed_accounts_iter)?;
            let base = next_keyed_account(keyed_accounts_iter)?;
            let to = next_keyed_account(keyed_accounts_iter)?;
            transfer_with_seed(from, base, &from_seed, &from_owner, to, lamports)
        }
        SystemInstruction::AdvanceNonceAccount => {
            let me = &mut next_keyed_account(keyed_accounts_iter)?;
            me.advance_nonce_account(
                &from_keyed_account::<RecentBlockhashes>(next_keyed_account(keyed_accounts_iter)?)?,
                &signers,
            )
        }
        SystemInstruction::WithdrawNonceAccount(lamports) => {
            let me = &mut next_keyed_account(keyed_accounts_iter)?;
            let to = &mut next_keyed_account(keyed_accounts_iter)?;
            me.withdraw_nonce_account(
                lamports,
                to,
                &from_keyed_account::<RecentBlockhashes>(next_keyed_account(keyed_accounts_iter)?)?,
                &from_keyed_account::<Rent>(next_keyed_account(keyed_accounts_iter)?)?,
                &signers,
            )
        }
        SystemInstruction::InitializeNonceAccount(authorized) => {
            let me = &mut next_keyed_account(keyed_accounts_iter)?;
            me.initialize_nonce_account(
                &authorized,
                &from_keyed_account::<RecentBlockhashes>(next_keyed_account(keyed_accounts_iter)?)?,
                &from_keyed_account::<Rent>(next_keyed_account(keyed_accounts_iter)?)?,
            )
        }
        SystemInstruction::AuthorizeNonceAccount(nonce_authority) => {
            let me = &mut next_keyed_account(keyed_accounts_iter)?;
            me.authorize_nonce_account(&nonce_authority, &signers)
        }
        SystemInstruction::Allocate { space } => {
            let keyed_account = next_keyed_account(keyed_accounts_iter)?;
            let mut account = keyed_account.try_account_ref_mut()?;
            let address = Address::create(keyed_account.unsigned_key(), None)?;
            allocate(&mut account, &address, space, &signers)
        }
        SystemInstruction::AllocateWithSeed {
            base,
            seed,
            space,
            owner,
        } => {
            let keyed_account = next_keyed_account(keyed_accounts_iter)?;
            let mut account = keyed_account.try_account_ref_mut()?;
            let address =
                Address::create(keyed_account.unsigned_key(), Some((&base, &seed, &owner)))?;
            allocate_and_assign(&mut account, &address, space, &owner, &signers)
        }
        SystemInstruction::AssignWithSeed { base, seed, owner } => {
            let keyed_account = next_keyed_account(keyed_accounts_iter)?;
            let mut account = keyed_account.try_account_ref_mut()?;
            let address =
                Address::create(keyed_account.unsigned_key(), Some((&base, &seed, &owner)))?;

            assign(&mut account, &address, &owner, &signers)
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SystemAccountKind {
    System,
    Nonce,
}

pub fn get_system_account_kind(account: &Account) -> Option<SystemAccountKind> {
    if system_program::check_id(&account.owner) {
        if account.data.is_empty() {
            Some(SystemAccountKind::System)
        } else if account.data.len() == nonce::State::size() {
            match account.state().ok()? {
                nonce::state::Versions::Current(state) => match *state {
                    nonce::State::Initialized(_) => Some(SystemAccountKind::Nonce),
                    _ => None,
                },
            }
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bank::Bank, bank_client::BankClient};
    use bincode::serialize;
    use solana_sdk::{
        account::{self, Account},
        client::SyncClient,
        fee_calculator::FeeCalculator,
        genesis_config::create_genesis_config,
        hash::{hash, Hash},
        instruction::{AccountMeta, Instruction, InstructionError},
        message::Message,
        nonce, nonce_account,
        process_instruction::MockInvokeContext,
        recent_blockhashes_account,
        signature::{Keypair, Signer},
        system_instruction, system_program, sysvar,
        sysvar::recent_blockhashes::IterItem,
        transaction::TransactionError,
    };
    use std::cell::RefCell;
    use std::sync::Arc;

    impl From<Pubkey> for Address {
        fn from(address: Pubkey) -> Self {
            Self {
                address,
                base: None,
            }
        }
    }

    fn process_instruction(
        owner: &Pubkey,
        keyed_accounts: &[KeyedAccount],
        instruction_data: &[u8],
    ) -> Result<(), InstructionError> {
        super::process_instruction(
            owner,
            keyed_accounts,
            instruction_data,
            &mut MockInvokeContext::default(),
        )
    }

    fn create_default_account() -> RefCell<Account> {
        RefCell::new(Account::default())
    }
    fn create_default_recent_blockhashes_account() -> RefCell<Account> {
        RefCell::new(recent_blockhashes_account::create_account_with_data(
            1,
            vec![
                IterItem(0u64, &Hash::default(), &FeeCalculator::default());
                sysvar::recent_blockhashes::CFG.RECENT_BLOCKHASHES_MAX_ENTRIES
            ]
            .into_iter(),
        ))
    }
    fn create_default_rent_account() -> RefCell<Account> {
        RefCell::new(account::create_account(&Rent::free(), 1))
    }

    #[test]
    fn test_create_account() {
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let to = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &system_program::id());
        let to_account = Account::new_ref(0, 0, &Pubkey::default());

        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&from, true, &from_account),
                    KeyedAccount::new(&to, true, &to_account)
                ],
                &bincode::serialize(&SystemInstruction::CreateAccount {
                    lamports: 50,
                    space: 2,
                    owner: new_owner
                })
                .unwrap()
            ),
            Ok(())
        );
        assert_eq!(from_account.borrow().lamports, 50);
        assert_eq!(to_account.borrow().lamports, 50);
        assert_eq!(to_account.borrow().owner, new_owner);
        assert_eq!(to_account.borrow().data, [0, 0]);
    }

    #[test]
    fn test_create_account_with_seed() {
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let seed = "shiny pepper";
        let to = Pubkey::create_with_seed(&from, seed, &new_owner).unwrap();

        let from_account = Account::new_ref(100, 0, &system_program::id());
        let to_account = Account::new_ref(0, 0, &Pubkey::default());

        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&from, true, &from_account),
                    KeyedAccount::new(&to, false, &to_account)
                ],
                &bincode::serialize(&SystemInstruction::CreateAccountWithSeed {
                    base: from,
                    seed: seed.to_string(),
                    lamports: 50,
                    space: 2,
                    owner: new_owner
                })
                .unwrap()
            ),
            Ok(())
        );
        assert_eq!(from_account.borrow().lamports, 50);
        assert_eq!(to_account.borrow().lamports, 50);
        assert_eq!(to_account.borrow().owner, new_owner);
        assert_eq!(to_account.borrow().data, [0, 0]);
    }

    #[test]
    fn test_address_create_with_seed_mismatch() {
        let from = solana_sdk::pubkey::new_rand();
        let seed = "dull boy";
        let to = solana_sdk::pubkey::new_rand();
        let owner = solana_sdk::pubkey::new_rand();

        assert_eq!(
            Address::create(&to, Some((&from, seed, &owner))),
            Err(SystemError::AddressWithSeedMismatch.into())
        );
    }

    #[test]
    fn test_create_account_with_seed_missing_sig() {
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let seed = "dull boy";
        let to = Pubkey::create_with_seed(&from, seed, &new_owner).unwrap();

        let from_account = Account::new_ref(100, 0, &system_program::id());
        let to_account = Account::new_ref(0, 0, &Pubkey::default());
        let to_address = Address::create(&to, Some((&from, seed, &new_owner))).unwrap();

        assert_eq!(
            create_account(
                &KeyedAccount::new(&from, false, &from_account),
                &KeyedAccount::new(&to, false, &to_account),
                &to_address,
                50,
                2,
                &new_owner,
                &HashSet::new(),
            ),
            Err(InstructionError::MissingRequiredSignature)
        );
        assert_eq!(from_account.borrow().lamports, 100);
        assert_eq!(*to_account.borrow(), Account::default());
    }

    #[test]
    fn test_create_with_zero_lamports() {
        // create account with zero lamports tranferred
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 1, &solana_sdk::pubkey::new_rand()); // not from system account

        let to = solana_sdk::pubkey::new_rand();
        let to_account = Account::new_ref(0, 0, &Pubkey::default());

        assert_eq!(
            create_account(
                &KeyedAccount::new(&from, false, &from_account), // no signer
                &KeyedAccount::new(&to, false, &to_account),
                &to.into(),
                0,
                2,
                &new_owner,
                &[to].iter().cloned().collect::<HashSet<_>>(),
            ),
            Ok(())
        );

        let from_lamports = from_account.borrow().lamports;
        let to_lamports = to_account.borrow().lamports;
        let to_owner = to_account.borrow().owner;
        let to_data = &to_account.borrow().data;
        assert_eq!(from_lamports, 100);
        assert_eq!(to_lamports, 0);
        assert_eq!(to_owner, new_owner);
        assert_eq!(*to_data, [0, 0]);
    }

    #[test]
    fn test_create_negative_lamports() {
        // Attempt to create account with more lamports than remaining in from_account
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &system_program::id());

        let to = solana_sdk::pubkey::new_rand();
        let to_account = Account::new_ref(0, 0, &Pubkey::default());

        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&from, false, &to_account),
            &to.into(),
            150,
            2,
            &new_owner,
            &[from, to].iter().cloned().collect::<HashSet<_>>(),
        );
        assert_eq!(result, Err(SystemError::ResultWithNegativeLamports.into()));
    }

    #[test]
    fn test_request_more_than_allowed_data_length() {
        let from_account = Account::new_ref(100, 0, &system_program::id());
        let from = solana_sdk::pubkey::new_rand();
        let to_account = Account::new_ref(0, 0, &system_program::id());
        let to = solana_sdk::pubkey::new_rand();

        let signers = &[from, to].iter().cloned().collect::<HashSet<_>>();
        let address = &to.into();

        // Trying to request more data length than permitted will result in failure
        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&to, false, &to_account),
            &address,
            50,
            MAX_PERMITTED_DATA_LENGTH + 1,
            &system_program::id(),
            &signers,
        );
        assert!(result.is_err());
        assert_eq!(
            result.err().unwrap(),
            SystemError::InvalidAccountDataLength.into()
        );

        // Trying to request equal or less data length than permitted will be successful
        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&to, false, &to_account),
            &address,
            50,
            MAX_PERMITTED_DATA_LENGTH,
            &system_program::id(),
            &signers,
        );
        assert!(result.is_ok());
        assert_eq!(to_account.borrow().lamports, 50);
        assert_eq!(
            to_account.borrow().data.len() as u64,
            MAX_PERMITTED_DATA_LENGTH
        );
    }

    #[test]
    fn test_create_already_in_use() {
        // Attempt to create system account in account already owned by another program
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &system_program::id());

        let original_program_owner = Pubkey::new(&[5; 32]);
        let owned_key = solana_sdk::pubkey::new_rand();
        let owned_account = Account::new_ref(0, 0, &original_program_owner);
        let unchanged_account = owned_account.clone();

        let signers = &[from, owned_key].iter().cloned().collect::<HashSet<_>>();
        let owned_address = owned_key.into();

        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&owned_key, false, &owned_account),
            &owned_address,
            50,
            2,
            &new_owner,
            &signers,
        );
        assert_eq!(result, Err(SystemError::AccountAlreadyInUse.into()));

        let from_lamports = from_account.borrow().lamports;
        assert_eq!(from_lamports, 100);
        assert_eq!(owned_account, unchanged_account);

        // Attempt to create system account in account that already has data
        let owned_account = Account::new_ref(0, 1, &Pubkey::default());
        let unchanged_account = owned_account.borrow().clone();
        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&owned_key, false, &owned_account),
            &owned_address,
            50,
            2,
            &new_owner,
            &signers,
        );
        assert_eq!(result, Err(SystemError::AccountAlreadyInUse.into()));
        let from_lamports = from_account.borrow().lamports;
        assert_eq!(from_lamports, 100);
        assert_eq!(*owned_account.borrow(), unchanged_account);

        // Attempt to create an account that already has lamports
        let owned_account = Account::new_ref(1, 0, &Pubkey::default());
        let unchanged_account = owned_account.borrow().clone();
        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&owned_key, false, &owned_account),
            &owned_address,
            50,
            2,
            &new_owner,
            &signers,
        );
        assert_eq!(result, Err(SystemError::AccountAlreadyInUse.into()));
        assert_eq!(from_lamports, 100);
        assert_eq!(*owned_account.borrow(), unchanged_account);
    }

    #[test]
    fn test_create_unsigned() {
        // Attempt to create an account without signing the transfer
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &system_program::id());

        let owned_key = solana_sdk::pubkey::new_rand();
        let owned_account = Account::new_ref(0, 0, &Pubkey::default());

        let owned_address = owned_key.into();

        // Haven't signed from account
        let result = create_account(
            &KeyedAccount::new(&from, false, &from_account),
            &KeyedAccount::new(&owned_key, false, &owned_account),
            &owned_address,
            50,
            2,
            &new_owner,
            &[owned_key].iter().cloned().collect::<HashSet<_>>(),
        );
        assert_eq!(result, Err(InstructionError::MissingRequiredSignature));

        // Haven't signed to account
        let owned_account = Account::new_ref(0, 0, &Pubkey::default());
        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&owned_key, true, &owned_account),
            &owned_address,
            50,
            2,
            &new_owner,
            &[from].iter().cloned().collect::<HashSet<_>>(),
        );
        assert_eq!(result, Err(InstructionError::MissingRequiredSignature));

        // support creation/assignment with zero lamports (ephemeral account)
        let owned_account = Account::new_ref(0, 0, &Pubkey::default());
        let result = create_account(
            &KeyedAccount::new(&from, false, &from_account),
            &KeyedAccount::new(&owned_key, false, &owned_account),
            &owned_address,
            0,
            2,
            &new_owner,
            &[owned_key].iter().cloned().collect::<HashSet<_>>(),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_create_sysvar_invalid_id() {
        // Attempt to create system account in account already owned by another program
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &system_program::id());

        let to = solana_sdk::pubkey::new_rand();
        let to_account = Account::new_ref(0, 0, &system_program::id());

        let signers = [from, to].iter().cloned().collect::<HashSet<_>>();
        let to_address = to.into();

        // fail to create a sysvar::id() owned account
        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&to, false, &to_account),
            &to_address,
            50,
            2,
            &sysvar::id(),
            &signers,
        );

        assert_eq!(result, Err(SystemError::InvalidProgramId.into()));
    }

    #[test]
    fn test_create_data_populated() {
        // Attempt to create system account in account with populated data
        let new_owner = Pubkey::new(&[9; 32]);
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &system_program::id());

        let populated_key = solana_sdk::pubkey::new_rand();
        let populated_account = Account {
            data: vec![0, 1, 2, 3],
            ..Account::default()
        }
        .into();

        let signers = [from, populated_key]
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let populated_address = populated_key.into();

        let result = create_account(
            &KeyedAccount::new(&from, true, &from_account),
            &KeyedAccount::new(&populated_key, false, &populated_account),
            &populated_address,
            50,
            2,
            &new_owner,
            &signers,
        );
        assert_eq!(result, Err(SystemError::AccountAlreadyInUse.into()));
    }

    #[test]
    fn test_create_from_account_is_nonce_fail() {
        let nonce = solana_sdk::pubkey::new_rand();
        let nonce_account = Account::new_ref_data(
            42,
            &nonce::state::Versions::new_current(nonce::State::Initialized(
                nonce::state::Data::default(),
            )),
            &system_program::id(),
        )
        .unwrap();
        let from = KeyedAccount::new(&nonce, true, &nonce_account);
        let new = solana_sdk::pubkey::new_rand();

        let new_account = Account::new_ref(0, 0, &system_program::id());

        let signers = [nonce, new].iter().cloned().collect::<HashSet<_>>();
        let new_address = new.into();
        let new_keyed_account = KeyedAccount::new(&new, false, &new_account);

        assert_eq!(
            create_account(
                &from,
                &new_keyed_account,
                &new_address,
                42,
                0,
                &solana_sdk::pubkey::new_rand(),
                &signers
            ),
            Err(InstructionError::InvalidArgument),
        );
    }

    #[test]
    fn test_assign() {
        let new_owner = Pubkey::new(&[9; 32]);

        let pubkey = solana_sdk::pubkey::new_rand();
        let mut account = Account::new(100, 0, &system_program::id());

        assert_eq!(
            assign(&mut account, &pubkey.into(), &new_owner, &HashSet::new()),
            Err(InstructionError::MissingRequiredSignature)
        );
        // no change, no signature needed
        assert_eq!(
            assign(
                &mut account,
                &pubkey.into(),
                &system_program::id(),
                &HashSet::new()
            ),
            Ok(())
        );

        let account = RefCell::new(account);
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(&pubkey, true, &account)],
                &bincode::serialize(&SystemInstruction::Assign { owner: new_owner }).unwrap()
            ),
            Ok(())
        );
    }

    #[test]
    fn test_assign_to_sysvar() {
        let new_owner = sysvar::id();

        let from = solana_sdk::pubkey::new_rand();
        let mut from_account = Account::new(100, 0, &system_program::id());

        assert_eq!(
            assign(
                &mut from_account,
                &from.into(),
                &new_owner,
                &[from].iter().cloned().collect::<HashSet<_>>(),
            ),
            Err(SystemError::InvalidProgramId.into())
        );
    }

    #[test]
    fn test_process_bogus_instruction() {
        // Attempt to assign with no accounts
        let instruction = SystemInstruction::Assign {
            owner: solana_sdk::pubkey::new_rand(),
        };
        let data = serialize(&instruction).unwrap();
        let result = process_instruction(&system_program::id(), &[], &data);
        assert_eq!(result, Err(InstructionError::NotEnoughAccountKeys));

        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &system_program::id());
        // Attempt to transfer with no destination
        let instruction = SystemInstruction::Transfer { lamports: 0 };
        let data = serialize(&instruction).unwrap();
        let result = process_instruction(
            &system_program::id(),
            &[KeyedAccount::new(&from, true, &from_account)],
            &data,
        );
        assert_eq!(result, Err(InstructionError::NotEnoughAccountKeys));
    }

    #[test]
    fn test_transfer_lamports() {
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref(100, 0, &Pubkey::new(&[2; 32])); // account owner should not matter
        let to = Pubkey::new(&[3; 32]);
        let to_account = Account::new_ref(1, 0, &to); // account owner should not matter
        let from_keyed_account = KeyedAccount::new(&from, true, &from_account);
        let to_keyed_account = KeyedAccount::new(&to, false, &to_account);
        transfer(&from_keyed_account, &to_keyed_account, 50).unwrap();
        let from_lamports = from_keyed_account.account.borrow().lamports;
        let to_lamports = to_keyed_account.account.borrow().lamports;
        assert_eq!(from_lamports, 50);
        assert_eq!(to_lamports, 51);

        // Attempt to move more lamports than remaining in from_account
        let from_keyed_account = KeyedAccount::new(&from, true, &from_account);
        let result = transfer(&from_keyed_account, &to_keyed_account, 100);
        assert_eq!(result, Err(SystemError::ResultWithNegativeLamports.into()));
        assert_eq!(from_keyed_account.account.borrow().lamports, 50);
        assert_eq!(to_keyed_account.account.borrow().lamports, 51);

        // test unsigned transfer of zero
        let from_keyed_account = KeyedAccount::new(&from, false, &from_account);
        assert!(transfer(&from_keyed_account, &to_keyed_account, 0,).is_ok(),);
        assert_eq!(from_keyed_account.account.borrow().lamports, 50);
        assert_eq!(to_keyed_account.account.borrow().lamports, 51);
    }

    #[test]
    fn test_transfer_with_seed() {
        let base = solana_sdk::pubkey::new_rand();
        let base_account = Account::new_ref(100, 0, &Pubkey::new(&[2; 32])); // account owner should not matter
        let from_base_keyed_account = KeyedAccount::new(&base, true, &base_account);
        let from_seed = "42";
        let from_owner = system_program::id();
        let from = Pubkey::create_with_seed(&base, from_seed, &from_owner).unwrap();
        let from_account = Account::new_ref(100, 0, &Pubkey::new(&[2; 32])); // account owner should not matter
        let to = Pubkey::new(&[3; 32]);
        let to_account = Account::new_ref(1, 0, &to); // account owner should not matter
        let from_keyed_account = KeyedAccount::new(&from, true, &from_account);
        let to_keyed_account = KeyedAccount::new(&to, false, &to_account);
        transfer_with_seed(
            &from_keyed_account,
            &from_base_keyed_account,
            &from_seed,
            &from_owner,
            &to_keyed_account,
            50,
        )
        .unwrap();
        let from_lamports = from_keyed_account.account.borrow().lamports;
        let to_lamports = to_keyed_account.account.borrow().lamports;
        assert_eq!(from_lamports, 50);
        assert_eq!(to_lamports, 51);

        // Attempt to move more lamports than remaining in from_account
        let from_keyed_account = KeyedAccount::new(&from, true, &from_account);
        let result = transfer_with_seed(
            &from_keyed_account,
            &from_base_keyed_account,
            &from_seed,
            &from_owner,
            &to_keyed_account,
            100,
        );
        assert_eq!(result, Err(SystemError::ResultWithNegativeLamports.into()));
        assert_eq!(from_keyed_account.account.borrow().lamports, 50);
        assert_eq!(to_keyed_account.account.borrow().lamports, 51);

        // test unsigned transfer of zero
        let from_keyed_account = KeyedAccount::new(&from, false, &from_account);
        assert!(transfer_with_seed(
            &from_keyed_account,
            &from_base_keyed_account,
            &from_seed,
            &from_owner,
            &to_keyed_account,
            0,
        )
        .is_ok(),);
        assert_eq!(from_keyed_account.account.borrow().lamports, 50);
        assert_eq!(to_keyed_account.account.borrow().lamports, 51);
    }

    #[test]
    fn test_transfer_lamports_from_nonce_account_fail() {
        let from = solana_sdk::pubkey::new_rand();
        let from_account = Account::new_ref_data(
            100,
            &nonce::state::Versions::new_current(nonce::State::Initialized(nonce::state::Data {
                authority: from,
                ..nonce::state::Data::default()
            })),
            &system_program::id(),
        )
        .unwrap();
        assert_eq!(
            get_system_account_kind(&from_account.borrow()),
            Some(SystemAccountKind::Nonce)
        );

        let to = Pubkey::new(&[3; 32]);
        let to_account = Account::new_ref(1, 0, &to); // account owner should not matter
        assert_eq!(
            transfer(
                &KeyedAccount::new(&from, true, &from_account),
                &KeyedAccount::new(&to, false, &to_account),
                50,
            ),
            Err(InstructionError::InvalidArgument),
        )
    }

    #[test]
    fn test_allocate() {
        let (genesis_config, mint_keypair) = create_genesis_config(100);
        let bank = Bank::new(&genesis_config);
        let bank_client = BankClient::new(bank);

        let alice_keypair = Keypair::new();
        let alice_pubkey = alice_keypair.pubkey();
        let seed = "seed";
        let owner = solana_sdk::pubkey::new_rand();
        let alice_with_seed = Pubkey::create_with_seed(&alice_pubkey, seed, &owner).unwrap();

        bank_client
            .transfer_and_confirm(50, &mint_keypair, &alice_pubkey)
            .unwrap();

        let allocate_with_seed = Message::new(
            &[system_instruction::allocate_with_seed(
                &alice_with_seed,
                &alice_pubkey,
                seed,
                2,
                &owner,
            )],
            Some(&alice_pubkey),
        );

        assert!(bank_client
            .send_and_confirm_message(&[&alice_keypair], allocate_with_seed)
            .is_ok());

        let allocate = system_instruction::allocate(&alice_pubkey, 2);

        assert!(bank_client
            .send_and_confirm_instruction(&alice_keypair, allocate)
            .is_ok());
    }

    fn with_create_zero_lamport<F>(callback: F)
    where
        F: Fn(&Bank),
    {
        solana_logger::setup();

        let alice_keypair = Keypair::new();
        let bob_keypair = Keypair::new();

        let alice_pubkey = alice_keypair.pubkey();
        let bob_pubkey = bob_keypair.pubkey();

        let program = solana_sdk::pubkey::new_rand();
        let collector = solana_sdk::pubkey::new_rand();

        let mint_lamports = 10000;
        let len1 = 123;
        let len2 = 456;

        // create initial bank and fund the alice account
        let (genesis_config, mint_keypair) = create_genesis_config(mint_lamports);
        let bank = Arc::new(Bank::new(&genesis_config));
        let bank_client = BankClient::new_shared(&bank);
        bank_client
            .transfer_and_confirm(mint_lamports, &mint_keypair, &alice_pubkey)
            .unwrap();

        // create zero-lamports account to be cleaned
        let bank = Arc::new(Bank::new_from_parent(&bank, &collector, bank.slot() + 1));
        let bank_client = BankClient::new_shared(&bank);
        let ix = system_instruction::create_account(&alice_pubkey, &bob_pubkey, 0, len1, &program);
        let message = Message::new(&[ix], Some(&alice_keypair.pubkey()));
        let r = bank_client.send_and_confirm_message(&[&alice_keypair, &bob_keypair], message);
        assert!(r.is_ok());

        // transfer some to bogus pubkey just to make previous bank (=slot) really cleanable
        let bank = Arc::new(Bank::new_from_parent(&bank, &collector, bank.slot() + 1));
        let bank_client = BankClient::new_shared(&bank);
        bank_client
            .transfer_and_confirm(50, &alice_keypair, &solana_sdk::pubkey::new_rand())
            .unwrap();

        // super fun time; callback chooses to .clean_accounts(None) or not
        callback(&*bank);

        // create a normal account at the same pubkey as the zero-lamports account
        let bank = Arc::new(Bank::new_from_parent(&bank, &collector, bank.slot() + 1));
        let bank_client = BankClient::new_shared(&bank);
        let ix = system_instruction::create_account(&alice_pubkey, &bob_pubkey, 1, len2, &program);
        let message = Message::new(&[ix], Some(&alice_pubkey));
        let r = bank_client.send_and_confirm_message(&[&alice_keypair, &bob_keypair], message);
        assert!(r.is_ok());
    }

    #[test]
    fn test_create_zero_lamport_with_clean() {
        with_create_zero_lamport(|bank| {
            bank.squash();
            // do clean and assert that it actually did its job
            assert_eq!(3, bank.get_snapshot_storages().len());
            bank.clean_accounts(false);
            assert_eq!(2, bank.get_snapshot_storages().len());
        });
    }

    #[test]
    fn test_create_zero_lamport_without_clean() {
        with_create_zero_lamport(|_| {
            // just do nothing; this should behave identically with test_create_zero_lamport_with_clean
        });
    }

    #[test]
    fn test_assign_with_seed() {
        let (genesis_config, mint_keypair) = create_genesis_config(100);
        let bank = Bank::new(&genesis_config);
        let bank_client = BankClient::new(bank);

        let alice_keypair = Keypair::new();
        let alice_pubkey = alice_keypair.pubkey();
        let seed = "seed";
        let owner = solana_sdk::pubkey::new_rand();
        let alice_with_seed = Pubkey::create_with_seed(&alice_pubkey, seed, &owner).unwrap();

        bank_client
            .transfer_and_confirm(50, &mint_keypair, &alice_pubkey)
            .unwrap();

        let assign_with_seed = Message::new(
            &[system_instruction::assign_with_seed(
                &alice_with_seed,
                &alice_pubkey,
                seed,
                &owner,
            )],
            Some(&alice_pubkey),
        );

        assert!(bank_client
            .send_and_confirm_message(&[&alice_keypair], assign_with_seed)
            .is_ok());
    }

    #[test]
    fn test_system_unsigned_transaction() {
        let (genesis_config, alice_keypair) = create_genesis_config(100);
        let alice_pubkey = alice_keypair.pubkey();
        let mallory_keypair = Keypair::new();
        let mallory_pubkey = mallory_keypair.pubkey();

        // Fund to account to bypass AccountNotFound error
        let bank = Bank::new(&genesis_config);
        let bank_client = BankClient::new(bank);
        bank_client
            .transfer_and_confirm(50, &alice_keypair, &mallory_pubkey)
            .unwrap();

        // Erroneously sign transaction with recipient account key
        // No signature case is tested by bank `test_zero_signatures()`
        let account_metas = vec![
            AccountMeta::new(alice_pubkey, false),
            AccountMeta::new(mallory_pubkey, true),
        ];
        let malicious_instruction = Instruction::new(
            system_program::id(),
            &SystemInstruction::Transfer { lamports: 10 },
            account_metas,
        );
        assert_eq!(
            bank_client
                .send_and_confirm_instruction(&mallory_keypair, malicious_instruction)
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
        );
        assert_eq!(bank_client.get_balance(&alice_pubkey).unwrap(), 50);
        assert_eq!(bank_client.get_balance(&mallory_pubkey).unwrap(), 50);
    }

    fn process_nonce_instruction(instruction: &Instruction) -> Result<(), InstructionError> {
        let accounts: Vec<_> = instruction
            .accounts
            .iter()
            .map(|meta| {
                RefCell::new(if sysvar::recent_blockhashes::check_id(&meta.pubkey) {
                    create_default_recent_blockhashes_account().into_inner()
                } else if sysvar::rent::check_id(&meta.pubkey) {
                    account::create_account(&Rent::free(), 1)
                } else {
                    Account::default()
                })
            })
            .collect();

        {
            let keyed_accounts: Vec<_> = instruction
                .accounts
                .iter()
                .zip(accounts.iter())
                .map(|(meta, account)| KeyedAccount::new(&meta.pubkey, meta.is_signer, account))
                .collect();
            process_instruction(&Pubkey::default(), &keyed_accounts, &instruction.data)
        }
    }

    #[test]
    fn test_process_nonce_ix_no_acc_data_fail() {
        assert_eq!(
            process_nonce_instruction(&system_instruction::advance_nonce_account(
                &Pubkey::default(),
                &Pubkey::default()
            )),
            Err(InstructionError::InvalidAccountData),
        );
    }

    #[test]
    fn test_process_nonce_ix_no_keyed_accs_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[],
                &serialize(&SystemInstruction::AdvanceNonceAccount).unwrap()
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_process_nonce_ix_only_nonce_acc_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(
                    &Pubkey::default(),
                    true,
                    &create_default_account(),
                ),],
                &serialize(&SystemInstruction::AdvanceNonceAccount).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_process_nonce_ix_bad_recent_blockhash_state_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), true, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &create_default_account(),
                    ),
                ],
                &serialize(&SystemInstruction::AdvanceNonceAccount).unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );
    }

    #[test]
    fn test_process_nonce_ix_ok() {
        let nonce_acc = nonce_account::create_account(1_000_000);
        process_instruction(
            &Pubkey::default(),
            &[
                KeyedAccount::new(&Pubkey::default(), true, &nonce_acc),
                KeyedAccount::new(
                    &sysvar::recent_blockhashes::id(),
                    false,
                    &create_default_recent_blockhashes_account(),
                ),
                KeyedAccount::new(&sysvar::rent::id(), false, &create_default_rent_account()),
            ],
            &serialize(&SystemInstruction::InitializeNonceAccount(Pubkey::default())).unwrap(),
        )
        .unwrap();
        let new_recent_blockhashes_account = RefCell::new(
            solana_sdk::recent_blockhashes_account::create_account_with_data(
                1,
                vec![
                    IterItem(
                        0u64,
                        &hash(&serialize(&0).unwrap()),
                        &FeeCalculator::default()
                    );
                    sysvar::recent_blockhashes::CFG.RECENT_BLOCKHASHES_MAX_ENTRIES
                ]
                .into_iter(),
            ),
        );
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), true, &nonce_acc,),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &new_recent_blockhashes_account,
                    ),
                ],
                &serialize(&SystemInstruction::AdvanceNonceAccount).unwrap(),
            ),
            Ok(()),
        );
    }

    #[test]
    fn test_process_withdraw_ix_no_acc_data_fail() {
        assert_eq!(
            process_nonce_instruction(&system_instruction::withdraw_nonce_account(
                &Pubkey::default(),
                &Pubkey::default(),
                &Pubkey::default(),
                1,
            )),
            Err(InstructionError::InvalidAccountData),
        );
    }

    #[test]
    fn test_process_withdraw_ix_no_keyed_accs_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[],
                &serialize(&SystemInstruction::WithdrawNonceAccount(42)).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_process_withdraw_ix_only_nonce_acc_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(
                    &Pubkey::default(),
                    true,
                    &create_default_account()
                ),],
                &serialize(&SystemInstruction::WithdrawNonceAccount(42)).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_process_withdraw_ix_bad_recent_blockhash_state_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), true, &create_default_account()),
                    KeyedAccount::new(&Pubkey::default(), false, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &create_default_account()
                    ),
                ],
                &serialize(&SystemInstruction::WithdrawNonceAccount(42)).unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );
    }

    #[test]
    fn test_process_withdraw_ix_bad_rent_state_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(
                        &Pubkey::default(),
                        true,
                        &nonce_account::create_account(1_000_000),
                    ),
                    KeyedAccount::new(&Pubkey::default(), true, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &create_default_recent_blockhashes_account(),
                    ),
                    KeyedAccount::new(&sysvar::rent::id(), false, &create_default_account()),
                ],
                &serialize(&SystemInstruction::WithdrawNonceAccount(42)).unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );
    }

    #[test]
    fn test_process_withdraw_ix_ok() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(
                        &Pubkey::default(),
                        true,
                        &nonce_account::create_account(1_000_000),
                    ),
                    KeyedAccount::new(&Pubkey::default(), true, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &create_default_recent_blockhashes_account(),
                    ),
                    KeyedAccount::new(&sysvar::rent::id(), false, &create_default_rent_account()),
                ],
                &serialize(&SystemInstruction::WithdrawNonceAccount(42)).unwrap(),
            ),
            Ok(()),
        );
    }

    #[test]
    fn test_process_initialize_ix_no_keyed_accs_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[],
                &serialize(&SystemInstruction::InitializeNonceAccount(Pubkey::default())).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_process_initialize_ix_only_nonce_acc_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(
                    &Pubkey::default(),
                    true,
                    &nonce_account::create_account(1_000_000),
                ),],
                &serialize(&SystemInstruction::InitializeNonceAccount(Pubkey::default())).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_process_initialize_bad_recent_blockhash_state_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(
                        &Pubkey::default(),
                        true,
                        &nonce_account::create_account(1_000_000),
                    ),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &create_default_account()
                    ),
                ],
                &serialize(&SystemInstruction::InitializeNonceAccount(Pubkey::default())).unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );
    }

    #[test]
    fn test_process_initialize_ix_bad_rent_state_fail() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(
                        &Pubkey::default(),
                        true,
                        &nonce_account::create_account(1_000_000),
                    ),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &create_default_recent_blockhashes_account(),
                    ),
                    KeyedAccount::new(&sysvar::rent::id(), false, &create_default_account()),
                ],
                &serialize(&SystemInstruction::InitializeNonceAccount(Pubkey::default())).unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );
    }

    #[test]
    fn test_process_initialize_ix_ok() {
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(
                        &Pubkey::default(),
                        true,
                        &nonce_account::create_account(1_000_000),
                    ),
                    KeyedAccount::new(
                        &sysvar::recent_blockhashes::id(),
                        false,
                        &create_default_recent_blockhashes_account(),
                    ),
                    KeyedAccount::new(&sysvar::rent::id(), false, &create_default_rent_account()),
                ],
                &serialize(&SystemInstruction::InitializeNonceAccount(Pubkey::default())).unwrap(),
            ),
            Ok(()),
        );
    }

    #[test]
    fn test_process_authorize_ix_ok() {
        let nonce_acc = nonce_account::create_account(1_000_000);
        process_instruction(
            &Pubkey::default(),
            &[
                KeyedAccount::new(&Pubkey::default(), true, &nonce_acc),
                KeyedAccount::new(
                    &sysvar::recent_blockhashes::id(),
                    false,
                    &create_default_recent_blockhashes_account(),
                ),
                KeyedAccount::new(&sysvar::rent::id(), false, &create_default_rent_account()),
            ],
            &serialize(&SystemInstruction::InitializeNonceAccount(Pubkey::default())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(&Pubkey::default(), true, &nonce_acc,),],
                &serialize(&SystemInstruction::AuthorizeNonceAccount(Pubkey::default(),)).unwrap(),
            ),
            Ok(()),
        );
    }

    #[test]
    fn test_process_authorize_bad_account_data_fail() {
        assert_eq!(
            process_nonce_instruction(&system_instruction::authorize_nonce_account(
                &Pubkey::default(),
                &Pubkey::default(),
                &Pubkey::default(),
            )),
            Err(InstructionError::InvalidAccountData),
        );
    }

    #[test]
    fn test_get_system_account_kind_system_ok() {
        let system_account = Account::default();
        assert_eq!(
            get_system_account_kind(&system_account),
            Some(SystemAccountKind::System)
        );
    }

    #[test]
    fn test_get_system_account_kind_nonce_ok() {
        let nonce_account = Account::new_data(
            42,
            &nonce::state::Versions::new_current(nonce::State::Initialized(
                nonce::state::Data::default(),
            )),
            &system_program::id(),
        )
        .unwrap();
        assert_eq!(
            get_system_account_kind(&nonce_account),
            Some(SystemAccountKind::Nonce)
        );
    }

    #[test]
    fn test_get_system_account_kind_uninitialized_nonce_account_fail() {
        assert_eq!(
            get_system_account_kind(&nonce_account::create_account(42).borrow()),
            None
        );
    }

    #[test]
    fn test_get_system_account_kind_system_owner_nonzero_nonnonce_data_fail() {
        let other_data_account = Account::new_data(42, b"other", &Pubkey::default()).unwrap();
        assert_eq!(get_system_account_kind(&other_data_account), None);
    }

    #[test]
    fn test_get_system_account_kind_nonsystem_owner_with_nonce_data_fail() {
        let nonce_account = Account::new_data(
            42,
            &nonce::state::Versions::new_current(nonce::State::Initialized(
                nonce::state::Data::default(),
            )),
            &solana_sdk::pubkey::new_rand(),
        )
        .unwrap();
        assert_eq!(get_system_account_kind(&nonce_account), None);
    }
}
