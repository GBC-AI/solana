//! configuration for network rent

#[repr(C)]
#[derive(Serialize, Deserialize, PartialEq, Clone, Copy, Debug, AbiExample)]
pub struct Rent {
    /// Rental rate
    pub lamports_per_byte_year: u64,

    /// exemption threshold, in years
    pub exemption_threshold: f64,

    // What portion of collected rent are to be destroyed, percentage-wise
    pub burn_percent: u8,
}

toml_config::package_config! {
    DEFAULT_LAMPORTS_PER_BYTE_YEAR: u64,
    DEFAULT_EXEMPTION_THRESHOLD: f64,
    DEFAULT_BURN_PERCENT: u8,
    ACCOUNT_STORAGE_OVERHEAD: u64,
}

impl Default for Rent {
    fn default() -> Self {
        Self {
            lamports_per_byte_year: CFG.DEFAULT_LAMPORTS_PER_BYTE_YEAR,
            exemption_threshold: CFG.DEFAULT_EXEMPTION_THRESHOLD,
            burn_percent: CFG.DEFAULT_BURN_PERCENT,
        }
    }
}

impl Rent {
    /// calculate how much rent to burn from the collected rent
    pub fn calculate_burn(&self, rent_collected: u64) -> (u64, u64) {
        let burned_portion = (rent_collected * u64::from(self.burn_percent)) / 100;
        (burned_portion, rent_collected - burned_portion)
    }
    /// minimum balance due for a given size Account::data.len()
    pub fn minimum_balance(&self, data_len: usize) -> u64 {
        let bytes = data_len as u64;
        (((CFG.ACCOUNT_STORAGE_OVERHEAD + bytes) * self.lamports_per_byte_year) as f64
            * self.exemption_threshold) as u64
    }

    /// whether a given balance and data_len would be exempt
    pub fn is_exempt(&self, balance: u64, data_len: usize) -> bool {
        balance >= self.minimum_balance(data_len)
    }

    /// rent due on account's data_len with balance
    pub fn due(&self, balance: u64, data_len: usize, years_elapsed: f64) -> (u64, bool) {
        if self.is_exempt(balance, data_len) {
            (0, true)
        } else {
            (
                ((self.lamports_per_byte_year * (data_len as u64 + CFG.ACCOUNT_STORAGE_OVERHEAD))
                    as f64
                    * years_elapsed) as u64,
                false,
            )
        }
    }

    pub fn free() -> Self {
        Self {
            lamports_per_byte_year: 0,
            ..Rent::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_due() {
        let default_rent = Rent::default();

        assert_eq!(
            default_rent.due(0, 2, 1.2),
            (
                (((2 + CFG.ACCOUNT_STORAGE_OVERHEAD) * CFG.DEFAULT_LAMPORTS_PER_BYTE_YEAR) as f64
                    * 1.2) as u64,
                CFG.DEFAULT_LAMPORTS_PER_BYTE_YEAR == 0
            )
        );
        assert_eq!(
            default_rent.due(
                (((2 + CFG.ACCOUNT_STORAGE_OVERHEAD) * CFG.DEFAULT_LAMPORTS_PER_BYTE_YEAR) as f64
                    * CFG.DEFAULT_EXEMPTION_THRESHOLD) as u64,
                2,
                1.2
            ),
            (0, true)
        );

        let mut custom_rent = Rent::default();
        custom_rent.lamports_per_byte_year = 5;
        custom_rent.exemption_threshold = 2.5;

        assert_eq!(
            custom_rent.due(0, 2, 1.2),
            (
                (((2 + CFG.ACCOUNT_STORAGE_OVERHEAD) * custom_rent.lamports_per_byte_year) as f64
                    * 1.2) as u64,
                false
            )
        );

        assert_eq!(
            custom_rent.due(
                (((2 + CFG.ACCOUNT_STORAGE_OVERHEAD) * custom_rent.lamports_per_byte_year) as f64
                    * custom_rent.exemption_threshold) as u64,
                2,
                1.2
            ),
            (0, true)
        );
    }

    #[ignore]
    #[test]
    #[should_panic]
    fn show_rent_model() {
        use crate::{
            clock::{CFG as CLOCK_CFG, *},
            sysvar::Sysvar,
        };

        const SECONDS_PER_YEAR: f64 = 365.242_199 * 24.0 * 60.0 * 60.0;
        toml_config::derived_values! {
            SLOTS_PER_YEAR: f64 = SECONDS_PER_YEAR
                / (CLOCK_CFG.DEFAULT_TICKS_PER_SLOT as f64 / CLOCK_CFG.DEFAULT_TICKS_PER_SECOND as f64);
        };

        let rent = Rent::default();
        panic!(
            "\n\n\
             ==================================================\n\
             empty account, no data:\n\
             \t{} lamports per epoch, {} lamports to be rent_exempt\n\n\
             stake_history, which is {}kB of data:\n\
             \t{} lamports per epoch, {} lamports to be rent_exempt\n\
             ==================================================\n\n",
            rent.due(
                0,
                0,
                (1.0 / *SLOTS_PER_YEAR) * *DEFAULT_SLOTS_PER_EPOCH as f64,
            )
            .0,
            rent.minimum_balance(0),
            crate::sysvar::stake_history::StakeHistory::size_of() / 1024,
            rent.due(
                0,
                crate::sysvar::stake_history::StakeHistory::size_of(),
                (1.0 / *SLOTS_PER_YEAR) * *DEFAULT_SLOTS_PER_EPOCH as f64,
            )
            .0,
            rent.minimum_balance(crate::sysvar::stake_history::StakeHistory::size_of()),
        );
    }
}
