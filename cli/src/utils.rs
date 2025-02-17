use ethers::{
    abi::token::{LenientTokenizer, Tokenizer},
    solc::EvmVersion,
    types::U256,
};
use forge::executor::{opts::EvmOpts, Fork, SpecId};
use foundry_config::{caching::StorageCachingConfig, Config};
use std::{
    future::Future,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;

// reexport all `foundry_config::utils`
#[doc(hidden)]
pub use foundry_config::utils::*;

/// The version message for the current program, like
/// `forge 0.1.0 (f01b232bc 2022-01-22T23:28:39.493201+00:00)`
pub(crate) const VERSION_MESSAGE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("VERGEN_GIT_SHA_SHORT"),
    " ",
    env!("VERGEN_BUILD_TIMESTAMP"),
    ")"
);

/// Useful extensions to [`std::path::Path`].
pub trait FoundryPathExt {
    /// Returns true if the [`Path`] ends with `.t.sol`
    fn is_sol_test(&self) -> bool;

    /// Returns true if the  [`Path`] has a `sol` extension
    fn is_sol(&self) -> bool;

    /// Returns true if the  [`Path`] has a `yul` extension
    fn is_yul(&self) -> bool;
}

impl<T: AsRef<Path>> FoundryPathExt for T {
    fn is_sol_test(&self) -> bool {
        self.as_ref()
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(".t.sol"))
            .unwrap_or_default()
    }

    fn is_sol(&self) -> bool {
        self.as_ref().extension() == Some(std::ffi::OsStr::new("sol"))
    }

    fn is_yul(&self) -> bool {
        self.as_ref().extension() == Some(std::ffi::OsStr::new("yul"))
    }
}

/// Initializes a tracing Subscriber for logging
#[allow(dead_code)]
pub fn subscriber() {
    tracing_subscriber::Registry::default()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .init()
}

pub fn evm_spec(evm: &EvmVersion) -> SpecId {
    match evm {
        EvmVersion::Istanbul => SpecId::ISTANBUL,
        EvmVersion::Berlin => SpecId::BERLIN,
        EvmVersion::London => SpecId::LONDON,
        _ => panic!("Unsupported EVM version"),
    }
}

/// Securely reads a secret from stdin, or proceeds to return a fallback value
/// which was provided in cleartext via CLI or env var
#[allow(dead_code)]
pub fn read_secret(secret: bool, unsafe_secret: Option<String>) -> eyre::Result<String> {
    Ok(if secret {
        println!("Insert secret:");
        rpassword::read_password()?
    } else {
        // guaranteed to be Some(..)
        unsafe_secret.unwrap()
    })
}

/// Artifact/Contract identifier can take the following form:
/// `<artifact file name>:<contract name>`, the `artifact file name` is the name of the json file of
/// the contract's artifact and the contract name is the name of the solidity contract, like
/// `SafeTransferLibTest.json:SafeTransferLibTest`
///
/// This returns the `contract name` part
///
/// # Example
///
/// ```
/// assert_eq!(
///     "SafeTransferLibTest",
///     utils::get_contract_name("SafeTransferLibTest.json:SafeTransferLibTest")
/// );
/// ```
pub fn get_contract_name(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}

/// This returns the `file name` part, See [`get_contract_name`]
///
/// # Example
///
/// ```
/// assert_eq!(
///     "SafeTransferLibTest.json",
///     utils::get_file_name("SafeTransferLibTest.json:SafeTransferLibTest")
/// );
/// ```
pub fn get_file_name(id: &str) -> &str {
    id.split(':').next().unwrap_or(id)
}

/// parse a hex str or decimal str as U256
pub fn parse_u256(s: &str) -> eyre::Result<U256> {
    Ok(if s.starts_with("0x") { U256::from_str(s)? } else { U256::from_dec_str(s)? })
}

/// Parses an ether value from a string.
///
/// The amount can be tagged with a unit, e.g. "1ether".
///
/// If the string represents an untagged amount (e.g. "100") then
/// it is interpreted as wei.
pub fn parse_ether_value(value: &str) -> eyre::Result<U256> {
    Ok(if value.starts_with("0x") {
        U256::from_str(value)?
    } else {
        U256::from(LenientTokenizer::tokenize_uint(value)?)
    })
}

/// Parses a `Duration` from a &str
pub fn parse_delay(delay: &str) -> eyre::Result<Duration> {
    let delay = if delay.ends_with("ms") {
        let d: u64 = delay.trim_end_matches("ms").parse()?;
        Duration::from_millis(d)
    } else {
        let d: f64 = delay.parse()?;
        let delay = (d * 1000.0).round();
        if delay.is_infinite() || delay.is_nan() || delay.is_sign_negative() {
            eyre::bail!("delay must be finite and non-negative");
        }

        Duration::from_millis(delay as u64)
    };
    Ok(delay)
}

/// Runs the `future` in a new [`tokio::runtime::Runtime`]
#[allow(unused)]
pub fn block_on<F: Future>(future: F) -> F::Output {
    let rt = tokio::runtime::Runtime::new().expect("could not start tokio rt");
    rt.block_on(future)
}

/// Helper function that returns the [Fork] to use, if any.
///
/// storage caching for the [Fork] will be enabled if
///   - `fork_url` is present
///   - `fork_block_number` is present
///   - [StorageCachingConfig] allows the `fork_url` +  chain id pair
///   - storage is allowed (`no_storage_caching = false`)
///
/// If all these criteria are met, then storage caching is enabled and storage info will be written
/// to [Config::foundry_cache_dir()]/<str(chainid)>/<block>/storage.json
///
/// for `mainnet` and `--fork-block-number 14435000` on mac the corresponding storage cache will be
/// at `~/.foundry/cache/mainnet/14435000/storage.json`
pub fn get_fork(evm_opts: &EvmOpts, config: &StorageCachingConfig) -> Option<Fork> {
    /// Returns the path where the cache file should be stored
    ///
    /// or `None` if caching should not be enabled
    ///
    /// See also [ Config::foundry_block_cache_file()]
    fn get_block_storage_path(
        evm_opts: &EvmOpts,
        config: &StorageCachingConfig,
        chain_id: u64,
    ) -> Option<PathBuf> {
        if evm_opts.no_storage_caching {
            // storage caching explicitly opted out of
            return None
        }
        let url = evm_opts.fork_url.as_ref()?;
        // cache only if block explicitly pinned
        let block = evm_opts.fork_block_number?;

        if config.enable_for_endpoint(url) && config.enable_for_chain_id(chain_id) {
            return Config::foundry_block_cache_file(chain_id, block)
        }

        None
    }

    if let Some(ref url) = evm_opts.fork_url {
        let chain_id = evm_opts.get_chain_id();
        let cache_storage = get_block_storage_path(evm_opts, config, chain_id);
        let fork = Fork {
            url: url.clone(),
            pin_block: evm_opts.fork_block_number,
            cache_path: cache_storage,
            chain_id,
        };
        return Some(fork)
    }

    None
}

/// Conditionally print a message
///
/// This macro accepts a predicate and the message to print if the predicate is tru
///
/// ```rust
/// let quiet = true;
/// p_println!(!quiet => "message");
/// ```
macro_rules! p_println {
    ($p:expr => $($arg:tt)*) => {{
        if $p {
            println!($($arg)*)
        }
    }}
}
pub(crate) use p_println;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foundry_path_ext_works() {
        let p = Path::new("contracts/MyTest.t.sol");
        assert!(p.is_sol_test());
        assert!(p.is_sol());
        let p = Path::new("contracts/Greeter.sol");
        assert!(!p.is_sol_test());
    }
}
