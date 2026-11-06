//! Resolve the environment passed to a sidecar without persisting its values.
//!
//! Clean mode retains only a finite baseline plus explicitly selected names.
//! It does not guarantee escape from process containment or parent supervision.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;

use crate::model::spec::EnvMode;

#[cfg(unix)]
const BASELINE: &[&str] = &[
    "HOME", "PATH", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TZ",
];
#[cfg(windows)]
const BASELINE: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "WINDIR",
    "COMSPEC",
    "USERPROFILE",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "TEMP",
    "TMP",
    "APPDATA",
    "LOCALAPPDATA",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
];

/// Ephemeral spawn environment; never serialize or expose its values in Debug.
pub enum SidecarEnvironment {
    Inherit,
    Clean(BTreeMap<OsString, OsString>),
}

impl fmt::Debug for SidecarEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inherit => f.write_str("Inherit"),
            Self::Clean(_) => f.write_str("Clean(<redacted>)"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("--pass-env requires --env-mode clean")]
    PassRequiresClean,
    #[error("invalid --pass-env name")]
    InvalidName,
    #[error("--pass-env variable {0} is not set")]
    Missing(String),
}

/// Validate request shape without inspecting the host's environment.
pub fn validate(mode: EnvMode, names: &[String]) -> Result<(), EnvironmentError> {
    if mode == EnvMode::Inherit && !names.is_empty() {
        return Err(EnvironmentError::PassRequiresClean);
    }
    if names
        .iter()
        .any(|name| name.is_empty() || name.contains(['=', '\0']))
    {
        return Err(EnvironmentError::InvalidName);
    }
    Ok(())
}

/// Canonical names form part of the launch identity, not their current values.
pub fn canonical_names(names: &[String]) -> BTreeSet<String> {
    #[cfg(windows)]
    let names = names.iter().map(|name| {
        String::from_utf16(&windows_name_key(std::ffi::OsStr::new(name)))
            .expect("Windows uppercase preserves valid UTF-16")
    });
    #[cfg(not(windows))]
    let names = names.iter().cloned();
    names.collect()
}

/// Windows compares environment names by fixed-width uppercase UTF-16,
/// not Unicode's expanding mapping (`ß` and `SS` remain distinct).
#[cfg(windows)]
pub(crate) fn windows_name_key(name: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::CharUpperBuffW;

    let mut wide: Vec<u16> = name.encode_wide().collect();
    for chunk in wide.chunks_mut(u32::MAX as usize) {
        let len = chunk.len() as u32;
        // SAFETY: the writable buffer contains exactly `len` UTF-16 units.
        // This native, language-independent mapping operates in place.
        let processed = unsafe { CharUpperBuffW(chunk.as_mut_ptr(), len) };
        assert_eq!(processed, len, "Windows environment-name uppercase failed");
    }
    wide
}

/// Resolve on the host doing the launch, before any session mutation.
pub fn resolve(mode: EnvMode, names: &[String]) -> Result<SidecarEnvironment, EnvironmentError> {
    validate(mode, names)?;
    if mode == EnvMode::Inherit {
        return Ok(SidecarEnvironment::Inherit);
    }
    let mut env = BTreeMap::new();
    for name in BASELINE {
        if let Some(value) = std::env::var_os(name) {
            env.insert(OsString::from(name), value);
        }
    }
    for name in canonical_names(names) {
        let value =
            std::env::var_os(&name).ok_or_else(|| EnvironmentError::Missing(name.clone()))?;
        #[cfg(windows)]
        env.retain(|key, _| !key.to_string_lossy().eq_ignore_ascii_case(&name));
        env.insert(OsString::from(name), value);
    }
    Ok(SidecarEnvironment::Clean(env))
}
