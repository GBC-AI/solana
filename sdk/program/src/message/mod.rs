//! A library for generating a message from a sequence of instructions

pub mod legacy;

#[cfg(not(target_arch = "bpf"))]
#[path = ""]
mod non_bpf_modules {
    mod sanitized;
    mod versions;

    pub use {sanitized::*, versions::*};
}

pub use legacy::Message;
#[cfg(not(target_arch = "bpf"))]
pub use non_bpf_modules::*;

/// The length of a message header in bytes
pub const MESSAGE_HEADER_LENGTH: usize = 3;

#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq, Clone, Copy, AbiExample)]
#[serde(rename_all = "camelCase")]
pub struct MessageHeader {
    /// The number of signatures required for this message to be considered valid. The
    /// signatures must match the first `num_required_signatures` of `account_keys`.
    /// NOTE: Serialization-related changes must be paired with the direct read at sigverify.
    pub num_required_signatures: u8,

    /// The last num_readonly_signed_accounts of the signed keys are read-only accounts. Programs
    /// may process multiple transactions that load read-only accounts within a single PoH entry,
    /// but are not permitted to credit or debit lamports or modify account data. Transactions
    /// targeting the same read-write account are evaluated sequentially.
    pub num_readonly_signed_accounts: u8,

    /// The last num_readonly_unsigned_accounts of the unsigned keys are read-only accounts.
    pub num_readonly_unsigned_accounts: u8,
}
