use crate::{
    client_error::{ClientError, ClientErrorKind, Result as ClientResult},
    http_sender::HttpSender,
    mock_sender::{MockSender, Mocks},
    rpc_config::RpcAccountInfoConfig,
    rpc_config::{
        RpcGetConfirmedSignaturesForAddress2Config, RpcLargestAccountsConfig,
        RpcProgramAccountsConfig, RpcSendTransactionConfig, RpcSimulateTransactionConfig,
        RpcTokenAccountsFilter,
    },
    rpc_request::{RpcError, RpcRequest, TokenAccountsFilter},
    rpc_response::*,
    rpc_sender::RpcSender,
};
use bincode::serialize;
use indicatif::{ProgressBar, ProgressStyle};
use log::*;
use serde_json::{json, Value};
use solana_account_decoder::{
    parse_token::{TokenAccountType, UiTokenAccount, UiTokenAmount},
    UiAccount, UiAccountData, UiAccountEncoding,
};
use solana_sdk::{
    account::Account,
    clock::{Slot, UnixTimestamp, CFG as CLOCK_CFG},
    commitment_config::{CommitmentConfig, CommitmentLevel},
    epoch_info::EpochInfo,
    epoch_schedule::EpochSchedule,
    fee_calculator::{FeeCalculator, FeeRateGovernor},
    hash::Hash,
    pubkey::Pubkey,
    signature::Signature,
    transaction::{self, uses_durable_nonce, Transaction},
};
use solana_transaction_status::{
    EncodedConfirmedBlock, EncodedConfirmedTransaction, TransactionStatus, UiTransactionEncoding,
};
use solana_vote_program::vote_state::MAX_LOCKOUT_HISTORY;
use std::{
    net::SocketAddr,
    sync::RwLock,
    thread::sleep,
    time::{Duration, Instant},
};

pub struct RpcClient {
    sender: Box<dyn RpcSender + Send + Sync + 'static>,
    default_cluster_transaction_encoding: RwLock<Option<UiTransactionEncoding>>,
}

fn serialize_encode_transaction(
    transaction: &Transaction,
    encoding: UiTransactionEncoding,
) -> ClientResult<String> {
    let serialized = serialize(transaction)
        .map_err(|e| ClientErrorKind::Custom(format!("transaction serialization failed: {}", e)))?;
    let encoded = match encoding {
        UiTransactionEncoding::Base58 => bs58::encode(serialized).into_string(),
        UiTransactionEncoding::Base64 => base64::encode(serialized),
        _ => {
            return Err(ClientErrorKind::Custom(format!(
                "unsupported transaction encoding: {}. Supported encodings: base58, base64",
                encoding
            ))
            .into())
        }
    };
    Ok(encoded)
}

impl RpcClient {
    pub fn new_sender<T: RpcSender + Send + Sync + 'static>(sender: T) -> Self {
        Self {
            sender: Box::new(sender),
            default_cluster_transaction_encoding: RwLock::new(None),
        }
    }

    pub fn new(url: String) -> Self {
        Self::new_sender(HttpSender::new(url))
    }

    pub fn new_with_timeout(url: String, timeout: Duration) -> Self {
        Self::new_sender(HttpSender::new_with_timeout(url, timeout))
    }

    pub fn new_mock(url: String) -> Self {
        Self::new_sender(MockSender::new(url))
    }

    pub fn new_mock_with_mocks(url: String, mocks: Mocks) -> Self {
        Self::new_sender(MockSender::new_with_mocks(url, mocks))
    }

    pub fn new_socket(addr: SocketAddr) -> Self {
        Self::new(get_rpc_request_str(addr, false))
    }

    pub fn new_socket_with_timeout(addr: SocketAddr, timeout: Duration) -> Self {
        let url = get_rpc_request_str(addr, false);
        Self::new_with_timeout(url, timeout)
    }

    pub fn confirm_transaction(&self, signature: &Signature) -> ClientResult<bool> {
        Ok(self
            .confirm_transaction_with_commitment(signature, CommitmentConfig::default())?
            .value)
    }

    pub fn confirm_transaction_with_commitment(
        &self,
        signature: &Signature,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<bool> {
        let Response { context, value } = self.get_signature_statuses(&[*signature])?;

        Ok(Response {
            context,
            value: value[0]
                .as_ref()
                .filter(|result| result.satisfies_commitment(commitment_config))
                .map(|result| result.status.is_ok())
                .unwrap_or_default(),
        })
    }

    pub fn send_transaction(&self, transaction: &Transaction) -> ClientResult<Signature> {
        self.send_transaction_with_config(transaction, RpcSendTransactionConfig::default())
    }

    fn default_cluster_transaction_encoding(&self) -> Result<UiTransactionEncoding, RpcError> {
        let default_cluster_transaction_encoding =
            self.default_cluster_transaction_encoding.read().unwrap();
        if let Some(encoding) = *default_cluster_transaction_encoding {
            Ok(encoding)
        } else {
            drop(default_cluster_transaction_encoding);
            let cluster_version = self.get_version().map_err(|e| {
                RpcError::RpcRequestError(format!("cluster version query failed: {}", e))
            })?;
            let cluster_version =
                semver::Version::parse(&cluster_version.solana_core).map_err(|e| {
                    RpcError::RpcRequestError(format!("failed to parse cluster version: {}", e))
                })?;
            // Prefer base64 since 1.3.16
            let encoding = if cluster_version < semver::Version::new(1, 3, 16) {
                UiTransactionEncoding::Base58
            } else {
                UiTransactionEncoding::Base64
            };
            *self.default_cluster_transaction_encoding.write().unwrap() = Some(encoding);
            Ok(encoding)
        }
    }

    pub fn send_transaction_with_config(
        &self,
        transaction: &Transaction,
        config: RpcSendTransactionConfig,
    ) -> ClientResult<Signature> {
        let encoding = if let Some(encoding) = config.encoding {
            encoding
        } else {
            self.default_cluster_transaction_encoding()?
        };
        let config = RpcSendTransactionConfig {
            encoding: Some(encoding),
            ..config
        };
        let serialized_encoded = serialize_encode_transaction(transaction, encoding)?;
        let signature_base58_str: String = self.send(
            RpcRequest::SendTransaction,
            json!([serialized_encoded, config]),
        )?;

        let signature = signature_base58_str
            .parse::<Signature>()
            .map_err(|err| Into::<ClientError>::into(RpcError::ParseError(err.to_string())))?;
        // A mismatching RPC response signature indicates an issue with the RPC node, and
        // should not be passed along to confirmation methods. The transaction may or may
        // not have been submitted to the cluster, so callers should verify the success of
        // the correct transaction signature independently.
        if signature != transaction.signatures[0] {
            Err(RpcError::RpcRequestError(format!(
                "RPC node returned mismatched signature {:?}, expected {:?}",
                signature, transaction.signatures[0]
            ))
            .into())
        } else {
            Ok(transaction.signatures[0])
        }
    }

    pub fn simulate_transaction(
        &self,
        transaction: &Transaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        self.simulate_transaction_with_config(transaction, RpcSimulateTransactionConfig::default())
    }

    pub fn simulate_transaction_with_config(
        &self,
        transaction: &Transaction,
        config: RpcSimulateTransactionConfig,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        let encoding = if let Some(encoding) = config.encoding {
            encoding
        } else {
            self.default_cluster_transaction_encoding()?
        };
        let config = RpcSimulateTransactionConfig {
            encoding: Some(encoding),
            ..config
        };
        let serialized_encoded = serialize_encode_transaction(transaction, encoding)?;
        self.send(
            RpcRequest::SimulateTransaction,
            json!([serialized_encoded, config]),
        )
    }

    pub fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> ClientResult<Option<transaction::Result<()>>> {
        self.get_signature_status_with_commitment(signature, CommitmentConfig::default())
    }

    pub fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        let signatures: Vec<_> = signatures.iter().map(|s| s.to_string()).collect();
        self.send(RpcRequest::GetSignatureStatuses, json!([signatures]))
    }

    pub fn get_signature_statuses_with_history(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        let signatures: Vec<_> = signatures.iter().map(|s| s.to_string()).collect();
        self.send(
            RpcRequest::GetSignatureStatuses,
            json!([signatures, {
                "searchTransactionHistory": true
            }]),
        )
    }

    pub fn get_signature_status_with_commitment(
        &self,
        signature: &Signature,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<Option<transaction::Result<()>>> {
        let result: Response<Vec<Option<TransactionStatus>>> = self.send(
            RpcRequest::GetSignatureStatuses,
            json!([[signature.to_string()]]),
        )?;
        Ok(result.value[0]
            .clone()
            .filter(|result| result.satisfies_commitment(commitment_config))
            .map(|status_meta| status_meta.status))
    }

    pub fn get_signature_status_with_commitment_and_history(
        &self,
        signature: &Signature,
        commitment_config: CommitmentConfig,
        search_transaction_history: bool,
    ) -> ClientResult<Option<transaction::Result<()>>> {
        let result: Response<Vec<Option<TransactionStatus>>> = self.send(
            RpcRequest::GetSignatureStatuses,
            json!([[signature.to_string()], {
                "searchTransactionHistory": search_transaction_history
            }]),
        )?;
        Ok(result.value[0]
            .clone()
            .filter(|result| result.satisfies_commitment(commitment_config))
            .map(|status_meta| status_meta.status))
    }

    pub fn get_slot(&self) -> ClientResult<Slot> {
        self.get_slot_with_commitment(CommitmentConfig::default())
    }

    pub fn get_slot_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<Slot> {
        self.send(RpcRequest::GetSlot, json!([commitment_config]))
    }

    pub fn supply_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<RpcSupply> {
        self.send(RpcRequest::GetSupply, json!([commitment_config]))
    }

    pub fn total_supply(&self) -> ClientResult<u64> {
        self.total_supply_with_commitment(CommitmentConfig::default())
    }

    pub fn total_supply_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<u64> {
        self.send(RpcRequest::GetTotalSupply, json!([commitment_config]))
    }

    pub fn get_largest_accounts_with_config(
        &self,
        config: RpcLargestAccountsConfig,
    ) -> RpcResult<Vec<RpcAccountBalance>> {
        self.send(RpcRequest::GetLargestAccounts, json!([config]))
    }

    pub fn get_vote_accounts(&self) -> ClientResult<RpcVoteAccountStatus> {
        self.get_vote_accounts_with_commitment(CommitmentConfig::default())
    }

    pub fn get_vote_accounts_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<RpcVoteAccountStatus> {
        self.send(RpcRequest::GetVoteAccounts, json!([commitment_config]))
    }

    pub fn get_cluster_nodes(&self) -> ClientResult<Vec<RpcContactInfo>> {
        self.send(RpcRequest::GetClusterNodes, Value::Null)
    }

    pub fn get_confirmed_block(&self, slot: Slot) -> ClientResult<EncodedConfirmedBlock> {
        self.get_confirmed_block_with_encoding(slot, UiTransactionEncoding::Json)
    }

    pub fn get_confirmed_block_with_encoding(
        &self,
        slot: Slot,
        encoding: UiTransactionEncoding,
    ) -> ClientResult<EncodedConfirmedBlock> {
        self.send(RpcRequest::GetConfirmedBlock, json!([slot, encoding]))
    }

    pub fn get_confirmed_blocks(
        &self,
        start_slot: Slot,
        end_slot: Option<Slot>,
    ) -> ClientResult<Vec<Slot>> {
        self.send(
            RpcRequest::GetConfirmedBlocks,
            json!([start_slot, end_slot]),
        )
    }

    pub fn get_confirmed_blocks_with_limit(
        &self,
        start_slot: Slot,
        limit: usize,
    ) -> ClientResult<Vec<Slot>> {
        self.send(
            RpcRequest::GetConfirmedBlocksWithLimit,
            json!([start_slot, limit]),
        )
    }

    pub fn get_confirmed_signatures_for_address(
        &self,
        address: &Pubkey,
        start_slot: Slot,
        end_slot: Slot,
    ) -> ClientResult<Vec<Signature>> {
        let signatures_base58_str: Vec<String> = self.send(
            RpcRequest::GetConfirmedSignaturesForAddress,
            json!([address.to_string(), start_slot, end_slot]),
        )?;

        let mut signatures = vec![];
        for signature_base58_str in signatures_base58_str {
            signatures.push(
                signature_base58_str.parse::<Signature>().map_err(|err| {
                    Into::<ClientError>::into(RpcError::ParseError(err.to_string()))
                })?,
            );
        }
        Ok(signatures)
    }

    pub fn get_confirmed_signatures_for_address2(
        &self,
        address: &Pubkey,
    ) -> ClientResult<Vec<RpcConfirmedTransactionStatusWithSignature>> {
        self.get_confirmed_signatures_for_address2_with_config(
            address,
            GetConfirmedSignaturesForAddress2Config::default(),
        )
    }

    pub fn get_confirmed_signatures_for_address2_with_config(
        &self,
        address: &Pubkey,
        config: GetConfirmedSignaturesForAddress2Config,
    ) -> ClientResult<Vec<RpcConfirmedTransactionStatusWithSignature>> {
        let config = RpcGetConfirmedSignaturesForAddress2Config {
            before: config.before.map(|signature| signature.to_string()),
            until: config.until.map(|signature| signature.to_string()),
            limit: config.limit,
        };

        let result: Vec<RpcConfirmedTransactionStatusWithSignature> = self.send(
            RpcRequest::GetConfirmedSignaturesForAddress2,
            json!([address.to_string(), config]),
        )?;

        Ok(result)
    }

    pub fn get_confirmed_transaction(
        &self,
        signature: &Signature,
        encoding: UiTransactionEncoding,
    ) -> ClientResult<EncodedConfirmedTransaction> {
        self.send(
            RpcRequest::GetConfirmedTransaction,
            json!([signature.to_string(), encoding]),
        )
    }

    pub fn get_block_time(&self, slot: Slot) -> ClientResult<UnixTimestamp> {
        let request = RpcRequest::GetBlockTime;
        let response = self.sender.send(request, json!([slot]));

        response
            .map(|result_json| {
                if result_json.is_null() {
                    return Err(RpcError::ForUser(format!("Block Not Found: slot={}", slot)).into());
                }
                let result = serde_json::from_value(result_json)
                    .map_err(|err| ClientError::new_with_request(err.into(), request))?;
                trace!("Response block timestamp {:?} {:?}", slot, result);
                Ok(result)
            })
            .map_err(|err| err.into_with_request(request))?
    }

    pub fn get_epoch_info(&self) -> ClientResult<EpochInfo> {
        self.get_epoch_info_with_commitment(CommitmentConfig::default())
    }

    pub fn get_epoch_info_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<EpochInfo> {
        self.send(RpcRequest::GetEpochInfo, json!([commitment_config]))
    }

    pub fn get_leader_schedule(
        &self,
        slot: Option<Slot>,
    ) -> ClientResult<Option<RpcLeaderSchedule>> {
        self.get_leader_schedule_with_commitment(slot, CommitmentConfig::default())
    }

    pub fn get_leader_schedule_with_commitment(
        &self,
        slot: Option<Slot>,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<Option<RpcLeaderSchedule>> {
        self.send(
            RpcRequest::GetLeaderSchedule,
            json!([slot, commitment_config]),
        )
    }

    pub fn get_epoch_schedule(&self) -> ClientResult<EpochSchedule> {
        self.send(RpcRequest::GetEpochSchedule, Value::Null)
    }

    pub fn get_identity(&self) -> ClientResult<Pubkey> {
        let rpc_identity: RpcIdentity = self.send(RpcRequest::GetIdentity, Value::Null)?;

        rpc_identity.identity.parse::<Pubkey>().map_err(|_| {
            ClientError::new_with_request(
                RpcError::ParseError("Pubkey".to_string()).into(),
                RpcRequest::GetIdentity,
            )
        })
    }

    pub fn get_inflation_governor(&self) -> ClientResult<RpcInflationGovernor> {
        self.send(RpcRequest::GetInflationGovernor, Value::Null)
    }

    pub fn get_inflation_rate(&self) -> ClientResult<RpcInflationRate> {
        self.send(RpcRequest::GetInflationRate, Value::Null)
    }

    pub fn get_version(&self) -> ClientResult<RpcVersionInfo> {
        self.send(RpcRequest::GetVersion, Value::Null)
    }

    pub fn minimum_ledger_slot(&self) -> ClientResult<Slot> {
        self.send(RpcRequest::MinimumLedgerSlot, Value::Null)
    }

    pub fn send_and_confirm_transaction(
        &self,
        transaction: &Transaction,
    ) -> ClientResult<Signature> {
        let signature = self.send_transaction(transaction)?;
        let recent_blockhash = if uses_durable_nonce(transaction).is_some() {
            self.get_recent_blockhash_with_commitment(CommitmentConfig::recent())?
                .value
                .0
        } else {
            transaction.message.recent_blockhash
        };
        let status = loop {
            let status = self.get_signature_status(&signature)?;
            if status.is_none() {
                if self
                    .get_fee_calculator_for_blockhash_with_commitment(
                        &recent_blockhash,
                        CommitmentConfig::recent(),
                    )?
                    .value
                    .is_none()
                {
                    break status;
                }
            } else {
                break status;
            }
            if cfg!(not(test)) {
                // Retry twice a second
                sleep(Duration::from_millis(500));
            }
        };
        if let Some(result) = status {
            match result {
                Ok(_) => Ok(signature),
                Err(err) => Err(err.into()),
            }
        } else {
            Err(RpcError::ForUser(
                "unable to confirm transaction. \
                                  This can happen in situations such as transaction expiration \
                                  and insufficient fee-payer funds"
                    .to_string(),
            )
            .into())
        }
    }

    pub fn get_account(&self, pubkey: &Pubkey) -> ClientResult<Account> {
        self.get_account_with_commitment(pubkey, CommitmentConfig::default())?
            .value
            .ok_or_else(|| RpcError::ForUser(format!("AccountNotFound: pubkey={}", pubkey)).into())
    }

    pub fn get_account_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<Option<Account>> {
        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            commitment: Some(commitment_config),
            data_slice: None,
        };
        let response = self.sender.send(
            RpcRequest::GetAccountInfo,
            json!([pubkey.to_string(), config]),
        );

        response
            .map(|result_json| {
                if result_json.is_null() {
                    return Err(
                        RpcError::ForUser(format!("AccountNotFound: pubkey={}", pubkey)).into(),
                    );
                }
                let Response {
                    context,
                    value: rpc_account,
                } = serde_json::from_value::<Response<Option<UiAccount>>>(result_json)?;
                trace!("Response account {:?} {:?}", pubkey, rpc_account);
                let account = rpc_account.and_then(|rpc_account| rpc_account.decode());
                Ok(Response {
                    context,
                    value: account,
                })
            })
            .map_err(|err| {
                Into::<ClientError>::into(RpcError::ForUser(format!(
                    "AccountNotFound: pubkey={}: {}",
                    pubkey, err
                )))
            })?
    }

    pub fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> ClientResult<Vec<Option<Account>>> {
        Ok(self
            .get_multiple_accounts_with_commitment(pubkeys, CommitmentConfig::default())?
            .value)
    }

    pub fn get_multiple_accounts_with_commitment(
        &self,
        pubkeys: &[Pubkey],
        commitment_config: CommitmentConfig,
    ) -> RpcResult<Vec<Option<Account>>> {
        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            commitment: Some(commitment_config),
            data_slice: None,
        };
        let pubkeys: Vec<_> = pubkeys.iter().map(|pubkey| pubkey.to_string()).collect();
        let response = self.send(RpcRequest::GetMultipleAccounts, json!([pubkeys, config]))?;
        let Response {
            context,
            value: accounts,
        } = serde_json::from_value::<Response<Vec<Option<UiAccount>>>>(response)?;
        let accounts: Vec<Option<Account>> = accounts
            .into_iter()
            .map(|rpc_account| rpc_account.map(|a| a.decode()).flatten())
            .collect();
        Ok(Response {
            context,
            value: accounts,
        })
    }

    pub fn get_account_data(&self, pubkey: &Pubkey) -> ClientResult<Vec<u8>> {
        Ok(self.get_account(pubkey)?.data)
    }

    pub fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> ClientResult<u64> {
        let request = RpcRequest::GetMinimumBalanceForRentExemption;
        let minimum_balance_json = self
            .sender
            .send(request, json!([data_len]))
            .map_err(|err| err.into_with_request(request))?;

        let minimum_balance: u64 = serde_json::from_value(minimum_balance_json)
            .map_err(|err| ClientError::new_with_request(err.into(), request))?;
        trace!(
            "Response minimum balance {:?} {:?}",
            data_len,
            minimum_balance
        );
        Ok(minimum_balance)
    }

    /// Request the balance of the account `pubkey`.
    pub fn get_balance(&self, pubkey: &Pubkey) -> ClientResult<u64> {
        Ok(self
            .get_balance_with_commitment(pubkey, CommitmentConfig::default())?
            .value)
    }

    pub fn get_balance_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<u64> {
        self.send(
            RpcRequest::GetBalance,
            json!([pubkey.to_string(), commitment_config]),
        )
    }

    pub fn get_program_accounts(&self, pubkey: &Pubkey) -> ClientResult<Vec<(Pubkey, Account)>> {
        self.get_program_accounts_with_config(
            pubkey,
            RpcProgramAccountsConfig {
                filters: None,
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..RpcAccountInfoConfig::default()
                },
            },
        )
    }

    pub fn get_program_accounts_with_config(
        &self,
        pubkey: &Pubkey,
        config: RpcProgramAccountsConfig,
    ) -> ClientResult<Vec<(Pubkey, Account)>> {
        let accounts: Vec<RpcKeyedAccount> = self.send(
            RpcRequest::GetProgramAccounts,
            json!([pubkey.to_string(), config]),
        )?;
        parse_keyed_accounts(accounts, RpcRequest::GetProgramAccounts)
    }

    /// Request the transaction count.
    pub fn get_transaction_count(&self) -> ClientResult<u64> {
        self.get_transaction_count_with_commitment(CommitmentConfig::default())
    }

    pub fn get_transaction_count_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<u64> {
        self.send(RpcRequest::GetTransactionCount, json!([commitment_config]))
    }

    pub fn get_recent_blockhash(&self) -> ClientResult<(Hash, FeeCalculator)> {
        let (blockhash, fee_calculator, _last_valid_slot) = self
            .get_recent_blockhash_with_commitment(CommitmentConfig::default())?
            .value;
        Ok((blockhash, fee_calculator))
    }

    pub fn get_recent_blockhash_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<(Hash, FeeCalculator, Slot)> {
        let (context, blockhash, fee_calculator, last_valid_slot) = if let Ok(Response {
            context,
            value:
                RpcFees {
                    blockhash,
                    fee_calculator,
                    last_valid_slot,
                },
        }) =
            self.send::<Response<RpcFees>>(RpcRequest::GetFees, json!([commitment_config]))
        {
            (context, blockhash, fee_calculator, last_valid_slot)
        } else if let Ok(Response {
            context,
            value:
                RpcBlockhashFeeCalculator {
                    blockhash,
                    fee_calculator,
                },
        }) = self.send::<Response<RpcBlockhashFeeCalculator>>(
            RpcRequest::GetRecentBlockhash,
            json!([commitment_config]),
        ) {
            (context, blockhash, fee_calculator, 0)
        } else {
            return Err(ClientError::new_with_request(
                RpcError::ParseError("RpcBlockhashFeeCalculator or RpcFees".to_string()).into(),
                RpcRequest::GetRecentBlockhash,
            ));
        };

        let blockhash = blockhash.parse().map_err(|_| {
            ClientError::new_with_request(
                RpcError::ParseError("Hash".to_string()).into(),
                RpcRequest::GetRecentBlockhash,
            )
        })?;
        Ok(Response {
            context,
            value: (blockhash, fee_calculator, last_valid_slot),
        })
    }

    pub fn get_fee_calculator_for_blockhash(
        &self,
        blockhash: &Hash,
    ) -> ClientResult<Option<FeeCalculator>> {
        Ok(self
            .get_fee_calculator_for_blockhash_with_commitment(
                blockhash,
                CommitmentConfig::default(),
            )?
            .value)
    }

    pub fn get_fee_calculator_for_blockhash_with_commitment(
        &self,
        blockhash: &Hash,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<Option<FeeCalculator>> {
        let Response { context, value } = self.send::<Response<Option<RpcFeeCalculator>>>(
            RpcRequest::GetFeeCalculatorForBlockhash,
            json!([blockhash.to_string(), commitment_config]),
        )?;

        Ok(Response {
            context,
            value: value.map(|rf| rf.fee_calculator),
        })
    }

    pub fn get_fee_rate_governor(&self) -> RpcResult<FeeRateGovernor> {
        let Response {
            context,
            value: RpcFeeRateGovernor { fee_rate_governor },
        } =
            self.send::<Response<RpcFeeRateGovernor>>(RpcRequest::GetFeeRateGovernor, Value::Null)?;

        Ok(Response {
            context,
            value: fee_rate_governor,
        })
    }

    pub fn get_new_blockhash(&self, blockhash: &Hash) -> ClientResult<(Hash, FeeCalculator)> {
        let mut num_retries = 0;
        let start = Instant::now();
        while start.elapsed().as_secs() < 5 {
            if let Ok((new_blockhash, fee_calculator)) = self.get_recent_blockhash() {
                if new_blockhash != *blockhash {
                    return Ok((new_blockhash, fee_calculator));
                }
            }
            debug!("Got same blockhash ({:?}), will retry...", blockhash);

            // Retry ~twice during a slot
            sleep(Duration::from_millis(
                500 * CLOCK_CFG.DEFAULT_TICKS_PER_SLOT / CLOCK_CFG.DEFAULT_TICKS_PER_SECOND,
            ));
            num_retries += 1;
        }
        Err(RpcError::ForUser(format!(
            "Unable to get new blockhash after {}ms (retried {} times), stuck at {}",
            start.elapsed().as_millis(),
            num_retries,
            blockhash
        ))
        .into())
    }

    pub fn get_first_available_block(&self) -> ClientResult<Slot> {
        self.send(RpcRequest::GetFirstAvailableBlock, Value::Null)
    }

    pub fn get_genesis_hash(&self) -> ClientResult<Hash> {
        let hash_str: String = self.send(RpcRequest::GetGenesisHash, Value::Null)?;
        let hash = hash_str.parse().map_err(|_| {
            ClientError::new_with_request(
                RpcError::ParseError("Hash".to_string()).into(),
                RpcRequest::GetGenesisHash,
            )
        })?;
        Ok(hash)
    }

    pub fn get_token_account(&self, pubkey: &Pubkey) -> ClientResult<Option<UiTokenAccount>> {
        Ok(self
            .get_token_account_with_commitment(pubkey, CommitmentConfig::default())?
            .value)
    }

    pub fn get_token_account_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<Option<UiTokenAccount>> {
        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::JsonParsed),
            commitment: Some(commitment_config),
            data_slice: None,
        };
        let response = self.sender.send(
            RpcRequest::GetAccountInfo,
            json!([pubkey.to_string(), config]),
        );

        response
            .map(|result_json| {
                if result_json.is_null() {
                    return Err(
                        RpcError::ForUser(format!("AccountNotFound: pubkey={}", pubkey)).into(),
                    );
                }
                let Response {
                    context,
                    value: rpc_account,
                } = serde_json::from_value::<Response<Option<UiAccount>>>(result_json)?;
                trace!("Response account {:?} {:?}", pubkey, rpc_account);
                let response = {
                    if let Some(rpc_account) = rpc_account {
                        if let UiAccountData::Json(account_data) = rpc_account.data {
                            let token_account_type: TokenAccountType =
                                serde_json::from_value(account_data.parsed)?;
                            if let TokenAccountType::Account(token_account) = token_account_type {
                                return Ok(Response {
                                    context,
                                    value: Some(token_account),
                                });
                            }
                        }
                    }
                    Err(Into::<ClientError>::into(RpcError::ForUser(format!(
                        "Account could not be parsed as token account: pubkey={}",
                        pubkey
                    ))))
                };
                response?
            })
            .map_err(|err| {
                Into::<ClientError>::into(RpcError::ForUser(format!(
                    "AccountNotFound: pubkey={}: {}",
                    pubkey, err
                )))
            })?
    }

    pub fn get_token_account_balance(&self, pubkey: &Pubkey) -> ClientResult<UiTokenAmount> {
        Ok(self
            .get_token_account_balance_with_commitment(pubkey, CommitmentConfig::default())?
            .value)
    }

    pub fn get_token_account_balance_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<UiTokenAmount> {
        self.send(
            RpcRequest::GetTokenAccountBalance,
            json!([pubkey.to_string(), commitment_config]),
        )
    }

    pub fn get_token_accounts_by_delegate(
        &self,
        delegate: &Pubkey,
        token_account_filter: TokenAccountsFilter,
    ) -> ClientResult<Vec<RpcKeyedAccount>> {
        Ok(self
            .get_token_accounts_by_delegate_with_commitment(
                delegate,
                token_account_filter,
                CommitmentConfig::default(),
            )?
            .value)
    }

    pub fn get_token_accounts_by_delegate_with_commitment(
        &self,
        delegate: &Pubkey,
        token_account_filter: TokenAccountsFilter,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<Vec<RpcKeyedAccount>> {
        let token_account_filter = match token_account_filter {
            TokenAccountsFilter::Mint(mint) => RpcTokenAccountsFilter::Mint(mint.to_string()),
            TokenAccountsFilter::ProgramId(program_id) => {
                RpcTokenAccountsFilter::ProgramId(program_id.to_string())
            }
        };

        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::JsonParsed),
            commitment: Some(commitment_config),
            data_slice: None,
        };

        self.send(
            RpcRequest::GetTokenAccountsByOwner,
            json!([delegate.to_string(), token_account_filter, config]),
        )
    }

    pub fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        token_account_filter: TokenAccountsFilter,
    ) -> ClientResult<Vec<RpcKeyedAccount>> {
        Ok(self
            .get_token_accounts_by_owner_with_commitment(
                owner,
                token_account_filter,
                CommitmentConfig::default(),
            )?
            .value)
    }

    pub fn get_token_accounts_by_owner_with_commitment(
        &self,
        owner: &Pubkey,
        token_account_filter: TokenAccountsFilter,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<Vec<RpcKeyedAccount>> {
        let token_account_filter = match token_account_filter {
            TokenAccountsFilter::Mint(mint) => RpcTokenAccountsFilter::Mint(mint.to_string()),
            TokenAccountsFilter::ProgramId(program_id) => {
                RpcTokenAccountsFilter::ProgramId(program_id.to_string())
            }
        };

        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::JsonParsed),
            commitment: Some(commitment_config),
            data_slice: None,
        };

        self.send(
            RpcRequest::GetTokenAccountsByOwner,
            json!([owner.to_string(), token_account_filter, config]),
        )
    }

    pub fn get_token_supply(&self, mint: &Pubkey) -> ClientResult<UiTokenAmount> {
        Ok(self
            .get_token_supply_with_commitment(mint, CommitmentConfig::default())?
            .value)
    }

    pub fn get_token_supply_with_commitment(
        &self,
        mint: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> RpcResult<UiTokenAmount> {
        self.send(
            RpcRequest::GetTokenSupply,
            json!([mint.to_string(), commitment_config]),
        )
    }

    fn poll_balance_with_timeout_and_commitment(
        &self,
        pubkey: &Pubkey,
        polling_frequency: &Duration,
        timeout: &Duration,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<u64> {
        let now = Instant::now();
        loop {
            match self.get_balance_with_commitment(&pubkey, commitment_config) {
                Ok(bal) => {
                    return Ok(bal.value);
                }
                Err(e) => {
                    sleep(*polling_frequency);
                    if now.elapsed() > *timeout {
                        return Err(e);
                    }
                }
            };
        }
    }

    pub fn poll_get_balance_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<u64> {
        self.poll_balance_with_timeout_and_commitment(
            pubkey,
            &Duration::from_millis(100),
            &Duration::from_secs(1),
            commitment_config,
        )
    }

    pub fn wait_for_balance_with_commitment(
        &self,
        pubkey: &Pubkey,
        expected_balance: Option<u64>,
        commitment_config: CommitmentConfig,
    ) -> Option<u64> {
        const LAST: usize = 30;
        for run in 0..LAST {
            let balance_result = self.poll_get_balance_with_commitment(pubkey, commitment_config);
            if expected_balance.is_none() {
                return balance_result.ok();
            }
            trace!(
                "wait_for_balance_with_commitment [{}] {:?} {:?}",
                run,
                balance_result,
                expected_balance
            );
            if let (Some(expected_balance), Ok(balance_result)) = (expected_balance, balance_result)
            {
                if expected_balance == balance_result {
                    return Some(balance_result);
                }
            }
        }
        None
    }

    /// Poll the server to confirm a transaction.
    pub fn poll_for_signature(&self, signature: &Signature) -> ClientResult<()> {
        self.poll_for_signature_with_commitment(signature, CommitmentConfig::default())
    }

    /// Poll the server to confirm a transaction.
    pub fn poll_for_signature_with_commitment(
        &self,
        signature: &Signature,
        commitment_config: CommitmentConfig,
    ) -> ClientResult<()> {
        let now = Instant::now();
        loop {
            if let Ok(Some(_)) =
                self.get_signature_status_with_commitment(&signature, commitment_config)
            {
                break;
            }
            if now.elapsed().as_secs() > 15 {
                return Err(RpcError::ForUser(format!(
                    "signature not found after {} seconds",
                    now.elapsed().as_secs()
                ))
                .into());
            }
            sleep(Duration::from_millis(250));
        }
        Ok(())
    }

    /// Poll the server to confirm a transaction.
    pub fn poll_for_signature_confirmation(
        &self,
        signature: &Signature,
        min_confirmed_blocks: usize,
    ) -> ClientResult<usize> {
        let mut now = Instant::now();
        let mut confirmed_blocks = 0;
        loop {
            let response = self.get_num_blocks_since_signature_confirmation(signature);
            match response {
                Ok(count) => {
                    if confirmed_blocks != count {
                        info!(
                            "signature {} confirmed {} out of {} after {} ms",
                            signature,
                            count,
                            min_confirmed_blocks,
                            now.elapsed().as_millis()
                        );
                        now = Instant::now();
                        confirmed_blocks = count;
                    }
                    if count >= min_confirmed_blocks {
                        break;
                    }
                }
                Err(err) => {
                    debug!("check_confirmations request failed: {:?}", err);
                }
            };
            if now.elapsed().as_secs() > 20 {
                info!(
                    "signature {} confirmed {} out of {} failed after {} ms",
                    signature,
                    confirmed_blocks,
                    min_confirmed_blocks,
                    now.elapsed().as_millis()
                );
                if confirmed_blocks > 0 {
                    return Ok(confirmed_blocks);
                } else {
                    return Err(RpcError::ForUser(format!(
                        "signature not found after {} seconds",
                        now.elapsed().as_secs()
                    ))
                    .into());
                }
            }
            sleep(Duration::from_millis(250));
        }
        Ok(confirmed_blocks)
    }

    pub fn get_num_blocks_since_signature_confirmation(
        &self,
        signature: &Signature,
    ) -> ClientResult<usize> {
        let result: Response<Vec<Option<TransactionStatus>>> = self.send(
            RpcRequest::GetSignatureStatuses,
            json!([[signature.to_string()]]),
        )?;

        let confirmations = result.value[0]
            .clone()
            .ok_or_else(|| {
                ClientError::new_with_request(
                    ClientErrorKind::Custom("signature not found".to_string()),
                    RpcRequest::GetSignatureStatuses,
                )
            })?
            .confirmations
            .unwrap_or(MAX_LOCKOUT_HISTORY + 1);
        Ok(confirmations)
    }

    pub fn send_and_confirm_transaction_with_spinner(
        &self,
        transaction: &Transaction,
    ) -> ClientResult<Signature> {
        self.send_and_confirm_transaction_with_spinner_and_config(
            transaction,
            CommitmentConfig::default(),
            RpcSendTransactionConfig::default(),
        )
    }

    pub fn send_and_confirm_transaction_with_spinner_and_commitment(
        &self,
        transaction: &Transaction,
        commitment: CommitmentConfig,
    ) -> ClientResult<Signature> {
        self.send_and_confirm_transaction_with_spinner_and_config(
            transaction,
            commitment,
            RpcSendTransactionConfig::default(),
        )
    }

    pub fn send_and_confirm_transaction_with_spinner_and_config(
        &self,
        transaction: &Transaction,
        commitment: CommitmentConfig,
        config: RpcSendTransactionConfig,
    ) -> ClientResult<Signature> {
        let desired_confirmations = match commitment.commitment {
            CommitmentLevel::Max | CommitmentLevel::Root => MAX_LOCKOUT_HISTORY + 1,
            _ => 1,
        };
        let mut confirmations = 0;

        let progress_bar = new_spinner_progress_bar();

        progress_bar.set_message(&format!(
            "[{}/{}] Finalizing transaction {}",
            confirmations, desired_confirmations, transaction.signatures[0],
        ));
        let recent_blockhash = if uses_durable_nonce(transaction).is_some() {
            self.get_recent_blockhash_with_commitment(CommitmentConfig::recent())?
                .value
                .0
        } else {
            transaction.message.recent_blockhash
        };
        let signature = self.send_transaction_with_config(transaction, config)?;
        let (signature, status) = loop {
            // Get recent commitment in order to count confirmations for successful transactions
            let status =
                self.get_signature_status_with_commitment(&signature, CommitmentConfig::recent())?;
            if status.is_none() {
                if self
                    .get_fee_calculator_for_blockhash_with_commitment(
                        &recent_blockhash,
                        CommitmentConfig::recent(),
                    )?
                    .value
                    .is_none()
                {
                    break (signature, status);
                }
            } else {
                break (signature, status);
            }

            if cfg!(not(test)) {
                sleep(Duration::from_millis(500));
            }
        };
        if let Some(result) = status {
            if let Err(err) = result {
                return Err(err.into());
            }
        } else {
            return Err(RpcError::ForUser(
                "unable to confirm transaction. \
                                      This can happen in situations such as transaction expiration \
                                      and insufficient fee-payer funds"
                    .to_string(),
            )
            .into());
        }
        let now = Instant::now();
        loop {
            match commitment.commitment {
                CommitmentLevel::Max | CommitmentLevel::Root =>
                // Return when default (max) commitment is reached
                // Failed transactions have already been eliminated, `is_some` check is sufficient
                {
                    if self.get_signature_status(&signature)?.is_some() {
                        progress_bar.set_message("Transaction confirmed");
                        progress_bar.finish_and_clear();
                        return Ok(signature);
                    }
                }
                _ => {
                    // Return when one confirmation has been reached
                    if confirmations >= desired_confirmations {
                        progress_bar.set_message("Transaction reached commitment");
                        progress_bar.finish_and_clear();
                        return Ok(signature);
                    }
                }
            }
            progress_bar.set_message(&format!(
                "[{}/{}] Finalizing transaction {}",
                confirmations + 1,
                desired_confirmations,
                signature,
            ));
            sleep(Duration::from_millis(500));
            confirmations = self
                .get_num_blocks_since_signature_confirmation(&signature)
                .unwrap_or(confirmations);
            if now.elapsed().as_secs() >= CLOCK_CFG.MAX_HASH_AGE_IN_SECONDS as u64 {
                return Err(
                    RpcError::ForUser("transaction not finalized. \
                                      This can happen when a transaction lands in an abandoned fork. \
                                      Please retry.".to_string()).into(),
                );
            }
        }
    }

    pub fn validator_exit(&self) -> ClientResult<bool> {
        self.send(RpcRequest::ValidatorExit, Value::Null)
    }

    pub fn send<T>(&self, request: RpcRequest, params: Value) -> ClientResult<T>
    where
        T: serde::de::DeserializeOwned,
    {
        assert!(params.is_array() || params.is_null());
        let response = self
            .sender
            .send(request, params)
            .map_err(|err| err.into_with_request(request))?;
        serde_json::from_value(response)
            .map_err(|err| ClientError::new_with_request(err.into(), request))
    }
}

#[derive(Debug, Default)]
pub struct GetConfirmedSignaturesForAddress2Config {
    pub before: Option<Signature>,
    pub until: Option<Signature>,
    pub limit: Option<usize>,
}

fn new_spinner_progress_bar() -> ProgressBar {
    let progress_bar = ProgressBar::new(42);
    progress_bar
        .set_style(ProgressStyle::default_spinner().template("{spinner:.green} {wide_msg}"));
    progress_bar.enable_steady_tick(100);
    progress_bar
}

pub fn get_rpc_request_str(rpc_addr: SocketAddr, tls: bool) -> String {
    if tls {
        format!("https://{}", rpc_addr)
    } else {
        format!("http://{}", rpc_addr)
    }
}

fn parse_keyed_accounts(
    accounts: Vec<RpcKeyedAccount>,
    request: RpcRequest,
) -> ClientResult<Vec<(Pubkey, Account)>> {
    let mut pubkey_accounts: Vec<(Pubkey, Account)> = Vec::new();
    for RpcKeyedAccount { pubkey, account } in accounts.into_iter() {
        let pubkey = pubkey.parse().map_err(|_| {
            ClientError::new_with_request(
                RpcError::ParseError("Pubkey".to_string()).into(),
                request,
            )
        })?;
        pubkey_accounts.push((
            pubkey,
            account.decode().ok_or_else(|| {
                ClientError::new_with_request(
                    RpcError::ParseError("Account from rpc".to_string()).into(),
                    request,
                )
            })?,
        ));
    }
    Ok(pubkey_accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{client_error::ClientErrorKind, mock_sender::PUBKEY};
    use assert_matches::assert_matches;
    use jsonrpc_core::{Error, IoHandler, Params};
    use jsonrpc_http_server::{AccessControlAllowOrigin, DomainsValidation, ServerBuilder};
    use serde_json::Number;
    use solana_sdk::{
        instruction::InstructionError, signature::Keypair, system_transaction,
        transaction::TransactionError,
    };
    use std::{io, sync::mpsc::channel, thread};

    #[test]
    fn test_send() {
        let (sender, receiver) = channel();
        thread::spawn(move || {
            let rpc_addr = "0.0.0.0:0".parse().unwrap();
            let mut io = IoHandler::default();
            // Successful request
            io.add_method("getBalance", |_params: Params| {
                Ok(Value::Number(Number::from(50)))
            });
            // Failed request
            io.add_method("getRecentBlockhash", |params: Params| {
                if params != Params::None {
                    Err(Error::invalid_request())
                } else {
                    Ok(Value::String(
                        "deadbeefXjn8o3yroDHxUtKsZZgoy4GPkPPXfouKNHhx".to_string(),
                    ))
                }
            });

            let server = ServerBuilder::new(io)
                .threads(1)
                .cors(DomainsValidation::AllowOnly(vec![
                    AccessControlAllowOrigin::Any,
                ]))
                .start_http(&rpc_addr)
                .expect("Unable to start RPC server");
            sender.send(*server.address()).unwrap();
            server.wait();
        });

        let rpc_addr = receiver.recv().unwrap();
        let rpc_client = RpcClient::new_socket(rpc_addr);

        let balance: u64 = rpc_client
            .send(
                RpcRequest::GetBalance,
                json!(["deadbeefXjn8o3yroDHxUtKsZZgoy4GPkPPXfouKNHhx"]),
            )
            .unwrap();
        assert_eq!(balance, 50);

        let blockhash: String = rpc_client
            .send(RpcRequest::GetRecentBlockhash, Value::Null)
            .unwrap();
        assert_eq!(blockhash, "deadbeefXjn8o3yroDHxUtKsZZgoy4GPkPPXfouKNHhx");

        // Send erroneous parameter
        let blockhash: ClientResult<String> =
            rpc_client.send(RpcRequest::GetRecentBlockhash, json!(["parameter"]));
        assert_eq!(blockhash.is_err(), true);
    }

    #[test]
    fn test_send_transaction() {
        let rpc_client = RpcClient::new_mock("succeeds".to_string());

        let key = Keypair::new();
        let to = solana_sdk::pubkey::new_rand();
        let blockhash = Hash::default();
        let tx = system_transaction::transfer(&key, &to, 50, blockhash);

        let signature = rpc_client.send_transaction(&tx);
        assert_eq!(signature.unwrap(), tx.signatures[0]);

        let rpc_client = RpcClient::new_mock("fails".to_string());

        let signature = rpc_client.send_transaction(&tx);
        assert!(signature.is_err());

        // Test bad signature returned from rpc node
        let rpc_client = RpcClient::new_mock("malicious".to_string());
        let signature = rpc_client.send_transaction(&tx);
        assert!(signature.is_err());
    }
    #[test]
    fn test_get_recent_blockhash() {
        let rpc_client = RpcClient::new_mock("succeeds".to_string());

        let expected_blockhash: Hash = PUBKEY.parse().unwrap();

        let (blockhash, _fee_calculator) = rpc_client.get_recent_blockhash().expect("blockhash ok");
        assert_eq!(blockhash, expected_blockhash);

        let rpc_client = RpcClient::new_mock("fails".to_string());

        assert!(rpc_client.get_recent_blockhash().is_err());
    }

    #[test]
    fn test_get_signature_status() {
        let signature = Signature::default();

        let rpc_client = RpcClient::new_mock("succeeds".to_string());
        let status = rpc_client.get_signature_status(&signature).unwrap();
        assert_eq!(status, Some(Ok(())));

        let rpc_client = RpcClient::new_mock("sig_not_found".to_string());
        let status = rpc_client.get_signature_status(&signature).unwrap();
        assert_eq!(status, None);

        let rpc_client = RpcClient::new_mock("account_in_use".to_string());
        let status = rpc_client.get_signature_status(&signature).unwrap();
        assert_eq!(status, Some(Err(TransactionError::AccountInUse)));
    }

    #[test]
    fn test_send_and_confirm_transaction() {
        let rpc_client = RpcClient::new_mock("succeeds".to_string());

        let key = Keypair::new();
        let to = solana_sdk::pubkey::new_rand();
        let blockhash = Hash::default();
        let tx = system_transaction::transfer(&key, &to, 50, blockhash);
        let result = rpc_client.send_and_confirm_transaction(&tx);
        result.unwrap();

        let rpc_client = RpcClient::new_mock("account_in_use".to_string());
        let result = rpc_client.send_and_confirm_transaction(&tx);
        assert!(result.is_err());

        let rpc_client = RpcClient::new_mock("instruction_error".to_string());
        let result = rpc_client.send_and_confirm_transaction(&tx);
        assert_matches!(
            result.unwrap_err().kind(),
            ClientErrorKind::TransactionError(TransactionError::InstructionError(
                0,
                InstructionError::UninitializedAccount
            ))
        );

        let rpc_client = RpcClient::new_mock("sig_not_found".to_string());
        let result = rpc_client.send_and_confirm_transaction(&tx);
        if let ClientErrorKind::Io(err) = result.unwrap_err().kind() {
            assert_eq!(err.kind(), io::ErrorKind::Other);
        }
    }

    #[test]
    fn test_rpc_client_thread() {
        let rpc_client = RpcClient::new_mock("succeeds".to_string());
        thread::spawn(move || rpc_client);
    }
}
