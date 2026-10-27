//! Project-scoped secrets.
//!
//! A secret is a value the Manifest names but never contains. The value lives in
//! the OS credential manager (Windows Credential Manager, through the `keyring`
//! crate); SQLite holds metadata only — Project, name, where the value came from,
//! and when — so the state database is safe to copy and the value never touches
//! disk in plaintext.
//!
//! Three sources, and only three:
//!
//! * `generate` — the daemon mints the value: 32 bytes from the OS random source,
//!   URL-safe base64, so it can be pasted into a connection string.
//! * `fromEnv` — imported once at Project creation from the daemon's own
//!   environment, falling back to `HKCU\Environment` for a daemon started by a
//!   GUI login item that never inherited the user's variables. Missing is a
//!   refusal (`SecretSourceMissing`), not an empty value.
//! * `literal` — a value an Agent supplies through the API. Audited like any
//!   other mutation, and never echoed back.
//!
//! The store is a seam: [`KeyringStore`] is the real implementation, and
//! [`MemoryStore`] is what a test-configured daemon uses, so no automated test
//! can read or write the developer's real credentials.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::redact::Redactor;

/// Keyring service name. One name per installation keeps Agentify's credentials
/// apart from every other application's.
pub const DEFAULT_SERVICE: &str = "agentify-v0";

/// Bytes of entropy in a generated value.
pub const GENERATED_BYTES: usize = 32;

/// A value the daemon minted.
pub const SOURCE_GENERATE: &str = "generate";

/// A value imported once from the host environment at Project creation.
pub const SOURCE_FROM_ENV: &str = "fromEnv";

/// A value an Agent supplied through the API.
pub const SOURCE_LITERAL: &str = "literal";

/// Longest accepted secret name. Matches the Manifest's name limit.
pub const MAX_NAME_LEN: usize = 48;

/// File, inside the data root, a test-configured daemon keeps [`MemoryStore`]
/// values in. A restart test drives two real processes over one data root, and a
/// store that forgot its values would make the second one unable to start the
/// Environment it just created. The shipped daemon never uses this store.
pub const TEST_STORE_FILE: &str = "test-secret-store.json";

/// Why the credential manager could not be used.
#[derive(Debug)]
pub enum SecretStoreError {
    /// No OS credential manager is reachable on this machine.
    Unavailable(String),
    /// The credential manager was reachable and refused, or failed.
    Failed(String),
}

impl std::fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretStoreError::Unavailable(detail) => {
                write!(f, "the OS credential manager is unavailable: {detail}")
            }
            SecretStoreError::Failed(detail) => {
                write!(f, "the OS credential manager failed: {detail}")
            }
        }
    }
}

impl std::error::Error for SecretStoreError {}

/// Where a Project's secret values are kept.
///
/// Every method is scoped by Project id as well as name, so two Projects may use
/// the same secret name without colliding, and one Project's value can never be
/// read with another's name.
pub trait SecretStore: Send + Sync {
    fn put(&self, project_id: &str, name: &str, value: &str) -> Result<(), SecretStoreError>;

    fn get(&self, project_id: &str, name: &str) -> Result<Option<String>, SecretStoreError>;

    fn delete(&self, project_id: &str, name: &str) -> Result<(), SecretStoreError>;

    /// Short name for diagnostics: `windows-credential-manager`, `memory`.
    fn describe(&self) -> &'static str;
}

/// The real store: the OS credential manager through the `keyring` crate.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// The credential manager this build actually reaches, for diagnostics.
    pub fn describe_target(&self) -> String {
        self.service.clone()
    }

    /// The account name one Project's secret is stored under.
    fn account(project_id: &str, name: &str) -> String {
        format!("{project_id}/{name}")
    }

    fn entry(&self, project_id: &str, name: &str) -> Result<keyring::Entry, SecretStoreError> {
        keyring::Entry::new(&self.service, &Self::account(project_id, name)).map_err(|err| {
            SecretStoreError::Unavailable(format!(
                "no credential manager could be opened for service {:?}: {err}",
                self.service
            ))
        })
    }
}

impl SecretStore for KeyringStore {
    fn put(&self, project_id: &str, name: &str, value: &str) -> Result<(), SecretStoreError> {
        self.entry(project_id, name)?
            .set_password(value)
            .map_err(|err| SecretStoreError::Failed(format!("storing {name:?}: {err}")))
    }

    fn get(&self, project_id: &str, name: &str) -> Result<Option<String>, SecretStoreError> {
        match self.entry(project_id, name)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(SecretStoreError::Failed(format!(
                "reading {name:?}: {err}"
            ))),
        }
    }

    fn delete(&self, project_id: &str, name: &str) -> Result<(), SecretStoreError> {
        match self.entry(project_id, name)?.delete_credential() {
            Ok(()) => Ok(()),
            // Nothing stored is the state the caller asked for.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(SecretStoreError::Failed(format!(
                "deleting {name:?}: {err}"
            ))),
        }
    }

    fn describe(&self) -> &'static str {
        "os-keyring"
    }
}

/// An in-process store. A test-configured daemon uses it, so an automated test
/// can never touch the developer's real credentials.
///
/// It is in memory *and*, when a test daemon gives it a path, mirrored to one
/// JSON file inside that daemon's data root. The file is what makes the restart
/// acceptance test honest: two real `agentifyd` processes share a data root, and
/// the second has to find the values the first one stored, exactly as the
/// credential manager would. It exists only for `AGENTIFY_SECRET_STORE=memory`,
/// which the shipped daemon never selects, and it is not the state database:
/// SQLite stays metadata-only either way.
#[derive(Default)]
pub struct MemoryStore {
    state: Mutex<MemoryState>,
    /// `None` for a store that lives only as long as this process.
    path: Option<PathBuf>,
}

#[derive(Default)]
struct MemoryState {
    values: BTreeMap<(String, String), String>,
    /// Whether `path` has been read yet. Loading lazily means constructing a
    /// store cannot fail before there is a caller to report it to.
    loaded: bool,
}

/// One value, as it is written to the test store's file.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredValue {
    project_id: String,
    name: String,
    value: String,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that keeps its values in `path`, so a daemon restarted over the
    /// same data root sees them.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            state: Mutex::new(MemoryState::default()),
            path: Some(path.into()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().expect("the in-memory secret store lock")
    }

    /// The values, read from the file the first time they are asked for.
    fn values(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, SecretStoreError> {
        let mut state = self.guard();
        if state.loaded {
            return Ok(state);
        }
        state.loaded = true;
        let Some(path) = &self.path else {
            return Ok(state);
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(state),
            Err(err) => {
                return Err(SecretStoreError::Failed(format!(
                    "reading {}: {err}",
                    path.display()
                )))
            }
        };
        let stored: Vec<StoredValue> = serde_json::from_str(&raw).map_err(|err| {
            SecretStoreError::Failed(format!("reading {}: {err}", path.display()))
        })?;
        for entry in stored {
            state
                .values
                .insert((entry.project_id, entry.name), entry.value);
        }
        Ok(state)
    }

    /// Write the values back out, when this store has a file.
    fn flush(&self, state: &MemoryState) -> Result<(), SecretStoreError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let stored: Vec<StoredValue> = state
            .values
            .iter()
            .map(|((project_id, name), value)| StoredValue {
                project_id: project_id.clone(),
                name: name.clone(),
                value: value.clone(),
            })
            .collect();
        let rendered = serde_json::to_vec_pretty(&stored).map_err(|err| {
            SecretStoreError::Failed(format!("rendering the test secret store: {err}"))
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                SecretStoreError::Failed(format!("creating {}: {err}", parent.display()))
            })?;
        }
        std::fs::write(path, rendered).map_err(|err| {
            SecretStoreError::Failed(format!("writing {}: {err}", path.display()))
        })
    }
}

impl SecretStore for MemoryStore {
    fn put(&self, project_id: &str, name: &str, value: &str) -> Result<(), SecretStoreError> {
        let mut state = self.values()?;
        state.values.insert(
            (project_id.to_string(), name.to_string()),
            value.to_string(),
        );
        self.flush(&state)
    }

    fn get(&self, project_id: &str, name: &str) -> Result<Option<String>, SecretStoreError> {
        let state = self.values()?;
        Ok(state
            .values
            .get(&(project_id.to_string(), name.to_string()))
            .cloned())
    }

    fn delete(&self, project_id: &str, name: &str) -> Result<(), SecretStoreError> {
        let mut state = self.values()?;
        state
            .values
            .remove(&(project_id.to_string(), name.to_string()));
        self.flush(&state)
    }

    fn describe(&self) -> &'static str {
        "memory"
    }
}

/// Which store a daemon uses. `AGENTIFY_SECRET_STORE` selects it; the shipped
/// daemon uses the credential manager and a test-configured one the memory
/// store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStoreKind {
    Keyring,
    Memory,
}

impl SecretStoreKind {
    /// Parse `AGENTIFY_SECRET_STORE`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => None,
            "keyring" | "os" | "credential-manager" | "windows" => Some(SecretStoreKind::Keyring),
            "memory" | "fake" | "in-memory" => Some(SecretStoreKind::Memory),
            _ => None,
        }
    }

    /// The store this kind names. A memory store is anchored to the data root so
    /// a restarted daemon over the same root finds the same values.
    pub fn store(self, service: &str, data_root: &Path) -> Arc<dyn SecretStore> {
        match self {
            SecretStoreKind::Keyring => Arc::new(KeyringStore::new(service)),
            SecretStoreKind::Memory => Arc::new(MemoryStore::at(data_root.join(TEST_STORE_FILE))),
        }
    }
}

/// Where a `fromEnv` import reads from.
#[derive(Debug, Clone, Default)]
pub enum SecretEnv {
    /// The daemon's own process environment, then `HKCU\Environment`.
    #[default]
    Process,
    /// An explicit map: a test-configured daemon is hermetic, so a developer's
    /// shell cannot change what a test sees.
    Fixed(BTreeMap<String, String>),
}

impl SecretEnv {
    pub fn fixed(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        SecretEnv::Fixed(pairs.into_iter().collect())
    }

    /// The value of a host variable, or `None` when it is not set.
    pub fn get(&self, var: &str) -> Option<String> {
        match self {
            SecretEnv::Process => process_env(var).or_else(|| user_environment(var)),
            SecretEnv::Fixed(map) => map.get(var).cloned(),
        }
    }
}

/// A variable the daemon process inherited.
fn process_env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// The user's own environment block, as a GUI-launched daemon sees it after a
/// logon that did not come through a shell.
#[cfg(windows)]
fn user_environment(var: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegQueryValueExW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_EXPAND_SZ, REG_SZ,
    };

    fn wide(raw: &str) -> Vec<u16> {
        std::ffi::OsStr::new(raw)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    unsafe {
        let subkey = wide("Environment");
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key)
            != ERROR_SUCCESS
        {
            return None;
        }
        let name = wide(var);
        // Environment values cap at 32767 UTF-16 units; 16 Ki units is what any
        // real secret uses, and a longer one is not a value worth importing.
        let mut buffer = vec![0u16; 16 * 1024];
        let mut length = (buffer.len() * 2) as u32;
        let mut kind: u32 = 0;
        let code = RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buffer.as_mut_ptr() as *mut u8,
            &mut length,
        );
        RegCloseKey(key);
        if code != ERROR_SUCCESS || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
            return None;
        }
        let units = (length as usize / 2).saturating_sub(1);
        let value = String::from_utf16_lossy(&buffer[..units.min(buffer.len())]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    }
}

#[cfg(not(windows))]
fn user_environment(_var: &str) -> Option<String> {
    // macOS and Linux daemons are started from a login session that inherits the
    // user's environment, so there is no second place to look in v0.
    None
}

/// A secret as the daemon will store it: the metadata SQLite keeps and the value
/// that goes to the store.
#[derive(Debug, Clone)]
pub struct Planned {
    pub name: String,
    pub source_kind: &'static str,
    /// The host variable a `fromEnv` import came from.
    pub source_ref: Option<String>,
    pub value: String,
}

/// A Manifest asked for a value the host does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSource {
    pub name: String,
    pub var: String,
}

impl MissingSource {
    /// The sentence an Agent gets: exactly what a person has to set.
    pub fn detail(&self) -> String {
        format!(
            "secret {:?} imports from the environment variable {} but it is not set \
             (and not in HKCU\\Environment); set it and create the Project again",
            self.name, self.var
        )
    }
}

/// The daemon's secret machinery: one store and one place imports read from.
#[derive(Clone)]
pub struct Secrets {
    pub store: Arc<dyn SecretStore>,
    pub env: SecretEnv,
}

impl Secrets {
    pub fn new(store: Arc<dyn SecretStore>, env: SecretEnv) -> Self {
        Self { store, env }
    }

    /// Turn a Manifest's `secrets:` into the values to store. Every source is
    /// resolved here, before anything is written, so a missing one leaves
    /// nothing behind.
    pub fn plan(
        &self,
        declared: &BTreeMap<String, agentify_manifest::Secret>,
    ) -> Result<Vec<Planned>, MissingSource> {
        let mut planned = Vec::with_capacity(declared.len());
        for (name, secret) in declared {
            // The Manifest parser guarantees exactly one of the two; a document
            // that reached here with neither is treated as a missing source.
            let planned_secret = match (&secret.generate, &secret.from_env) {
                // `password` and `token` are both 256 bits of OS randomness in
                // URL-safe base64: the label records intent, not strength.
                (Some(_), _) => Planned {
                    name: name.clone(),
                    source_kind: SOURCE_GENERATE,
                    source_ref: None,
                    value: generate().map_err(|err| MissingSource {
                        name: name.clone(),
                        var: format!("the OS random source ({err})"),
                    })?,
                },
                (None, Some(var)) => {
                    let value = self.env.get(var).ok_or_else(|| MissingSource {
                        name: name.clone(),
                        var: var.clone(),
                    })?;
                    Planned {
                        name: name.clone(),
                        source_kind: SOURCE_FROM_ENV,
                        source_ref: Some(var.clone()),
                        value,
                    }
                }
                (None, None) => {
                    return Err(MissingSource {
                        name: name.clone(),
                        var: String::new(),
                    })
                }
            };
            planned.push(planned_secret);
        }
        Ok(planned)
    }

    pub fn put(&self, project_id: &str, name: &str, value: &str) -> Result<(), SecretStoreError> {
        self.store.put(project_id, name, value)
    }

    pub fn get(&self, project_id: &str, name: &str) -> Result<Option<String>, SecretStoreError> {
        self.store.get(project_id, name)
    }

    pub fn delete(&self, project_id: &str, name: &str) -> Result<(), SecretStoreError> {
        self.store.delete(project_id, name)
    }

    /// A redactor that covers the values of the named secrets of one Project.
    pub fn redactor<'a>(
        &self,
        project_id: &str,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Redactor {
        let mut values = Vec::new();
        for name in names {
            if let Ok(Some(value)) = self.store.get(project_id, name) {
                values.push(value);
            }
        }
        Redactor::new(values)
    }
}

/// A fresh value: 32 bytes from the OS random source, base64url without padding.
///
/// URL-safe so it can sit in a connection string or a header without quoting,
/// and 256 bits of entropy so guessing is not a threat model we have to refine.
pub fn generate() -> Result<String, SecretStoreError> {
    let mut bytes = [0u8; GENERATED_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| {
        SecretStoreError::Failed(format!("the OS random source failed: {err}"))
    })?;
    Ok(base64_url(&bytes))
}

/// base64url (RFC 4648 §5) without padding.
fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map(|b| u32::from(*b));
        let b2 = chunk.get(2).map(|b| u32::from(*b));
        let triple = (b0 << 16) | (b1.unwrap_or(0) << 8) | b2.unwrap_or(0);
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if b1.is_some() {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        }
        if b2.is_some() {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        }
    }
    out
}

/// Validate a secret name an Agent supplies. The Manifest parser applies the same
/// rules, so a name from either path means the same thing.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a secret needs a name".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!("a name is at most {MAX_NAME_LEN} characters"));
    }
    if !name.starts_with(|ch: char| ch.is_ascii_lowercase()) {
        return Err(format!("{name:?} must start with a lower-case letter"));
    }
    if let Some(bad) = name
        .chars()
        .find(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-'))
    {
        return Err(format!(
            "{name:?} must be lower-case kebab-case; {bad:?} is not allowed"
        ));
    }
    if name.ends_with('-') || name.contains("--") {
        return Err(format!(
            "{name:?} must be lower-case kebab-case without empty segments"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_value_is_long_url_safe_and_unique() {
        let first = generate().expect("generated");
        let second = generate().expect("generated");
        assert_ne!(first, second, "two values are not the same value");
        assert!(
            first.len() >= 32,
            "at least 32 bytes of randomness: {first:?} is {} chars",
            first.len()
        );
        assert!(
            first
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
            "URL-safe: {first:?}"
        );
        assert!(!first.contains('='), "no padding: {first:?}");
    }

    #[test]
    fn a_short_enough_base64_tail_is_encoded_without_the_padding_characters() {
        assert_eq!(base64_url(b""), "");
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foo"), "Zm9v");
        assert_eq!(base64_url(b"foob"), "Zm9vYg");
        assert_eq!(base64_url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_url(b"foobar"), "Zm9vYmFy");
        // Bytes that exercise the last two characters of the alphabet.
        assert_eq!(base64_url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn the_memory_store_is_scoped_by_project_and_name() {
        let store = MemoryStore::new();
        store.put("proj-a", "shared", "one").expect("put");
        store.put("proj-b", "shared", "two").expect("put");
        assert_eq!(
            store.get("proj-a", "shared").expect("get").as_deref(),
            Some("one")
        );
        assert_eq!(
            store.get("proj-b", "shared").expect("get").as_deref(),
            Some("two")
        );
        store.delete("proj-a", "shared").expect("delete");
        assert!(store.get("proj-a", "shared").expect("get").is_none());
        assert_eq!(
            store.get("proj-b", "shared").expect("get").as_deref(),
            Some("two"),
            "deleting one Project's secret leaves the other's alone"
        );
        store.delete("proj-a", "shared").expect("deleting twice is fine");
    }

    #[test]
    fn a_fixed_environment_is_the_only_place_an_import_looks() {
        let env = SecretEnv::fixed([("PRESENT".to_string(), "value".to_string())]);
        assert_eq!(env.get("PRESENT").as_deref(), Some("value"));
        assert_eq!(env.get("AGENTIFY_DEFINITELY_NOT_SET"), None);
    }

    #[test]
    fn the_user_environment_fallback_finds_nothing_for_a_variable_nobody_set() {
        // Read-only, and a name no installation would use: the fallback path is
        // exercised without writing to the user's registry.
        assert_eq!(
            user_environment("AGENTIFY_TEST_VARIABLE_NOBODY_SETS_9f3c1a"),
            None
        );
    }

    #[test]
    fn a_file_backed_store_hands_its_values_to_the_next_process_over_the_same_root() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(TEST_STORE_FILE);

        // The first "process": writes two values.
        let first = MemoryStore::at(&path);
        first.put("proj", "one", "first-value").expect("put");
        first.put("proj", "two", "second-value").expect("put");
        first.delete("proj", "two").expect("delete");

        // The second: a fresh store over the same file, as a restart would build.
        let second = MemoryStore::at(&path);
        assert_eq!(
            second.get("proj", "one").expect("get").as_deref(),
            Some("first-value")
        );
        assert!(
            second.get("proj", "two").expect("get").is_none(),
            "a value deleted before the restart stays deleted"
        );

        // A store with no path keeps nothing anywhere.
        let transient = MemoryStore::new();
        transient.put("proj", "three", "third-value").expect("put");
        assert_eq!(
            MemoryStore::at(&path).get("proj", "three").expect("get"),
            None,
            "an in-memory store is not the file-backed one"
        );
    }

    #[test]
    fn a_name_has_to_be_lower_case_kebab() {
        for good in ["db-password", "a", "pg-pass-2"] {
            assert!(validate_name(good).is_ok(), "{good} is a name");
        }
        for bad in ["", "Upper", "with space", "trailing-", "double--dash", "1leading"] {
            assert!(validate_name(bad).is_err(), "{bad:?} is not a name");
        }
    }

    /// The real credential manager, opted into by name.
    ///
    /// Off by default: a suite that writes to the developer's Windows Credential
    /// Manager would leave entries behind and need the real vault for every run.
    /// The service name is unique per run and every entry is deleted before the
    /// test ends.
    #[test]
    fn the_os_keyring_round_trips_a_value_when_a_developer_asks_for_it() {
        if std::env::var("AGENTIFY_TEST_KEYRING").as_deref() != Ok("1") {
            eprintln!(
                "skipping the real keyring test: set AGENTIFY_TEST_KEYRING=1 to run it"
            );
            return;
        }
        let service = format!("agentify-test-{}", uuid::Uuid::new_v4());
        let store = KeyringStore::new(service.clone());
        let project = "keyring-test-project";
        let name = "keyring-test-secret";
        let value = "a-value-only-the-credential-manager-knows";

        // Clean up even if an assertion below panics.
        struct Cleanup {
            service: String,
            project: String,
            name: String,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let store = KeyringStore::new(self.service.clone());
                let _ = store.delete(&self.project, &self.name);
            }
        }
        let _cleanup = Cleanup {
            service,
            project: project.to_string(),
            name: name.to_string(),
        };

        assert!(
            store.get(project, name).expect("an empty slot").is_none(),
            "nothing is stored under a fresh service name"
        );
        store.put(project, name, value).expect("the real store accepts a write");
        assert_eq!(
            store.get(project, name).expect("the real store reads it back").as_deref(),
            Some(value)
        );
        store.put(project, name, "a-replacement").expect("overwrite");
        assert_eq!(
            store.get(project, name).expect("read").as_deref(),
            Some("a-replacement")
        );
        store.delete(project, name).expect("delete");
        assert!(
            store.get(project, name).expect("read").is_none(),
            "the value is gone after a delete"
        );
    }
}
