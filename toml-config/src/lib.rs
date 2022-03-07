use std::{env, fs, io};

#[macro_export]
macro_rules! package_config {
    ($($const:ident: $ty:ty,)+) => {
        #[allow(non_snake_case)]
        #[derive(serde_derive::Deserialize)]
        pub struct PackageConfig {
            $(pub $const: $ty),+
        }

        lazy_static::lazy_static! {
            pub static ref CFG: PackageConfig = toml_config::parse_config(env!("CARGO_PKG_NAME"))
                .unwrap_or_else(|err| panic!("Unable to read toml config for {}, error: {:?}", env!("CARGO_PKG_NAME"), err));
            // $( pub static ref $const: $ty = CFG.$const; )+
        }
    };
}

// TODO: single constant macro

#[macro_export]
macro_rules! derived_values {
    ($($const:ident: $ty:ty = $expr:expr;)+) => {
        lazy_static::lazy_static! {
            $( pub static ref $const: $ty = $expr; )+
        }
    };
}

const TOML_CONFIG_ENV_VAR: &str = "TOML_CONFIG";

#[derive(Debug, thiserror::Error)]
pub enum TomlConfigErr {
    #[error("Check enironment variable {}: {0}", TOML_CONFIG_ENV_VAR)]
    EnvVar(#[from] env::VarError),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Unable to parse toml from file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("Bad config structure: {0}")]
    BadConfig(String),
}

pub fn parse_config<'a, T: serde::Deserialize<'a>>(pkg_name: &str) -> Result<T, TomlConfigErr> {
    let toml_file = env::var(TOML_CONFIG_ENV_VAR)?;
    let content = fs::read_to_string(toml_file)?;
    let value: toml::Value = content.parse()?;

    if let toml::Value::Table(table) = value {
        let value = table.get(pkg_name).ok_or_else(|| {
            TomlConfigErr::BadConfig(format!(
                "Table doesn't contains required section for package {}",
                pkg_name
            ))
        })?;
        value.clone().try_into().map_err(TomlConfigErr::Parse)
    } else {
        Err(TomlConfigErr::BadConfig(format!(
            "Expected table at toml top level, but got: {:?}",
            value
        )))
    }
}

#[cfg(test)]
mod tests {
    use crate as toml_config;

    package_config! {
        FOO: usize,
        BAR: usize,
    }

    #[test]
    fn it_works() {
        assert_eq!(CFG.FOO, 42);
        assert_eq!(CFG.BAR, 13);
    }
}
