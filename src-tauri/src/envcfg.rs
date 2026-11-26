//! ScreenExtend's own environment variables (`SE_*`, `SCREENEXTEND_*`) are a developer's tool:
//! switches, traces, test hooks and the test-only data directory. A shipped build reads none of
//! them, so nothing in the environment changes how it behaves. What a user can change is in the
//! app's settings or on the command line.

use std::env::VarError;
use std::ffi::OsString;

/// Whether this build reads them: debug builds, the crate's own tests, and builds with the
/// `probe` feature (which `bench` implies). Neither feature ships.
pub const DEV_ENV: bool = cfg!(any(debug_assertions, test, feature = "probe"));

/// [`std::env::var`] in a build that reads the variables, absent in one that does not.
pub fn var(name: &str) -> Result<String, VarError> {
    var_in(DEV_ENV, name)
}

/// [`std::env::var_os`] in a build that reads the variables, absent in one that does not.
pub fn var_os(name: &str) -> Option<OsString> {
    if DEV_ENV {
        std::env::var_os(name)
    } else {
        None
    }
}

fn var_in(dev: bool, name: &str) -> Result<String, VarError> {
    if dev {
        std::env::var(name)
    } else {
        Err(VarError::NotPresent)
    }
}

#[cfg(test)]
mod tests {
    use super::var_in;
    use std::env::VarError;

    #[test]
    fn a_shipped_build_reads_nothing() {
        // PATH is set wherever the tests run
        assert!(var_in(true, "PATH").is_ok());
        assert_eq!(var_in(false, "PATH"), Err(VarError::NotPresent));
    }
}
