//! Saved Discord accounts under `<config-dir>/accounts`, the default account, and the one-time move of the legacy token (ADR 0005).

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, io_err, read_token_file, write_private};

pub const MAX_NAME_LEN: usize = 32;
/// The account a legacy token moves into when its user can't be looked up.
pub const FALLBACK_NAME: &str = "default";

const ACCOUNTS_DIR: &str = "accounts";
const DEFAULT_FILE: &str = "default-account";
const MIGRATION_LOCK: &str = "accounts.lock";
const LEGACY_TOKEN: &str = "token";
const TOKEN_FILE: &str = "token";
const USER_FILE: &str = "account.toml";

/// The Discord user an account belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUser {
    pub id: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub name: String,
    pub user: Option<AccountUser>,
    pub has_token: bool,
}

/// Where a token came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// `DSTREAMY_TOKEN`: no account management.
    Env,
    Account(String),
    /// `<config-dir>/token`, while its migration has not succeeded.
    Legacy,
}

impl TokenSource {
    pub fn account(&self) -> Option<&str> {
        match self {
            Self::Account(name) => Some(name),
            Self::Env | Self::Legacy => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedToken {
    pub source: TokenSource,
    pub token: String,
}

pub fn validate_name(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let rest_ok = chars.all(name_char);
    if first_ok && rest_ok && name.len() <= MAX_NAME_LEN {
        Ok(())
    } else {
        Err(ConfigError::InvalidAccountName(name.to_owned()))
    }
}

fn name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-')
}

/// A Discord username as an account name: lower case, foreign characters dropped, `account` if nothing is left.
pub fn name_from_username(username: &str) -> String {
    let name: String = username
        .to_lowercase()
        .chars()
        .filter(|&c| name_char(c))
        .skip_while(|&c| c == '.' || c == '-')
        .take(MAX_NAME_LEN)
        .collect();
    if name.is_empty() {
        "account".to_owned()
    } else {
        name
    }
}

fn with_suffix(base: &str, n: u32) -> String {
    let suffix = format!("-{n}");
    let keep = MAX_NAME_LEN - suffix.len();
    let base: String = base.chars().take(keep).collect();
    format!("{base}{suffix}")
}

/// The saved accounts of one config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accounts {
    dir: PathBuf,
}

impl Accounts {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            dir: config_dir.to_owned(),
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.dir
    }

    fn root(&self) -> PathBuf {
        self.dir.join(ACCOUNTS_DIR)
    }

    pub fn account_dir(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    pub fn token_file(&self, name: &str) -> PathBuf {
        self.account_dir(name).join(TOKEN_FILE)
    }

    pub fn legacy_token_file(&self) -> PathBuf {
        self.dir.join(LEGACY_TOKEN)
    }

    fn default_file(&self) -> PathBuf {
        self.dir.join(DEFAULT_FILE)
    }

    /// Where the session lock of a session with this token lives.
    pub fn lock_dir(&self, source: &TokenSource) -> PathBuf {
        match source {
            TokenSource::Account(name) => self.account_dir(name),
            TokenSource::Env | TokenSource::Legacy => self.dir.clone(),
        }
    }

    /// The saved account names, in order.
    pub fn names(&self) -> Result<Vec<String>, ConfigError> {
        let root = self.root();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(io_err(&root)(e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_err(&root))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if validate_name(&name).is_ok() && entry.path().is_dir() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn exists(&self, name: &str) -> bool {
        validate_name(name).is_ok() && self.account_dir(name).is_dir()
    }

    /// Fails with the saved names when `name` isn't one of them.
    pub fn require(&self, name: &str) -> Result<(), ConfigError> {
        if self.exists(name) {
            return Ok(());
        }
        Err(ConfigError::UnknownAccount {
            name: name.to_owned(),
            available: self.names()?,
        })
    }

    pub fn list(&self) -> Result<Vec<AccountInfo>, ConfigError> {
        Ok(self
            .names()?
            .into_iter()
            .map(|name| AccountInfo {
                user: self.user(&name),
                has_token: self.load_token(&name).ok().flatten().is_some(),
                name,
            })
            .collect())
    }

    /// `None` while unknown; a broken `account.toml` only costs the name in lists.
    pub fn user(&self, name: &str) -> Option<AccountUser> {
        let text = fs::read_to_string(self.account_dir(name).join(USER_FILE)).ok()?;
        toml::from_str(&text).ok()
    }

    /// Records the user of an existing account; does nothing for an account that is gone.
    pub fn remember_user(&self, name: &str, user: &AccountUser) -> Result<(), ConfigError> {
        if !self.exists(name) || self.user(name).as_ref() == Some(user) {
            return Ok(());
        }
        self.write_user(name, user)
    }

    fn write_user(&self, name: &str, user: &AccountUser) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(user)?;
        write_private(&self.account_dir(name).join(USER_FILE), text.as_bytes())
    }

    /// The default-account file when it names a saved account, else the first saved account.
    pub fn default_name(&self) -> Result<Option<String>, ConfigError> {
        if let Some(name) = self.marked_default()
            && self.exists(&name)
        {
            return Ok(Some(name));
        }
        Ok(self.names()?.into_iter().next())
    }

    fn marked_default(&self) -> Option<String> {
        let text = fs::read_to_string(self.default_file()).ok()?;
        Some(text.trim().to_owned()).filter(|n| validate_name(n).is_ok())
    }

    pub fn set_default(&self, name: &str) -> Result<(), ConfigError> {
        self.require(name)?;
        write_private(&self.default_file(), format!("{name}\n").as_bytes())
    }

    pub fn load_token(&self, name: &str) -> Result<Option<String>, ConfigError> {
        validate_name(name)?;
        read_token_file(&self.token_file(name))
    }

    /// Creates the account or replaces its token, and its user when given; the first account becomes the default.
    pub fn save(
        &self,
        name: &str,
        token: &str,
        user: Option<&AccountUser>,
    ) -> Result<(), ConfigError> {
        validate_name(name)?;
        self.create_dir(name)?;
        write_private(
            &self.token_file(name),
            format!("{}\n", token.trim()).as_bytes(),
        )?;
        if let Some(user) = user {
            self.write_user(name, user)?;
        }
        if self.marked_default().is_none_or(|n| !self.exists(&n)) {
            self.set_default(name)?;
        }
        Ok(())
    }

    fn create_dir(&self, name: &str) -> Result<(), ConfigError> {
        fs::create_dir_all(&self.dir).map_err(io_err(&self.dir))?;
        for dir in [self.root(), self.account_dir(name)] {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                            .map_err(io_err(&dir))?;
                    }
                }
                Err(e) => return Err(io_err(&dir)(e)),
            }
        }
        Ok(())
    }

    /// Deletes the account; the default moves to the first remaining account.
    pub fn remove(&self, name: &str) -> Result<(), ConfigError> {
        self.require(name)?;
        let dir = self.account_dir(name);
        fs::remove_dir_all(&dir).map_err(io_err(&dir))?;
        if self.marked_default().is_some_and(|n| self.exists(&n)) {
            return Ok(());
        }
        match self.names()?.into_iter().next() {
            Some(next) => self.set_default(&next),
            None => match fs::remove_file(self.default_file()) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(&self.default_file())(e)),
            },
        }
    }

    /// The account a login without a chosen name saves into: the one of the same user, else one named after them.
    pub fn name_for(&self, user: &AccountUser) -> Result<String, ConfigError> {
        let names = self.names()?;
        if let Some(name) = names
            .iter()
            .find(|n| self.user(n).is_some_and(|u| u.id == user.id))
        {
            return Ok(name.clone());
        }
        Ok(free_name(&name_from_username(&user.username), &names))
    }

    /// `env` is `DSTREAMY_TOKEN`; `requested` is `--account`. `None` means not logged in.
    pub fn resolve(
        &self,
        requested: Option<&str>,
        env: Option<&str>,
    ) -> Result<Option<ResolvedToken>, ConfigError> {
        if let Some(token) = env.map(str::trim).filter(|t| !t.is_empty()) {
            if requested.is_some() {
                return Err(ConfigError::AccountWithEnvToken);
            }
            return Ok(Some(ResolvedToken {
                source: TokenSource::Env,
                token: token.to_owned(),
            }));
        }
        let name = match requested {
            Some(name) => {
                validate_name(name)?;
                self.require(name)?;
                Some(name.to_owned())
            }
            None => self.default_name()?,
        };
        if let Some(name) = name {
            let token = self
                .load_token(&name)?
                .ok_or_else(|| ConfigError::AccountWithoutToken(name.clone()))?;
            return Ok(Some(ResolvedToken {
                source: TokenSource::Account(name),
                token,
            }));
        }
        Ok(
            read_token_file(&self.legacy_token_file())?.map(|token| ResolvedToken {
                source: TokenSource::Legacy,
                token,
            }),
        )
    }

    /// The lock that orders migrations and default changes; `None` if another process holds it past `wait`.
    pub fn store_lock(&self, wait: Duration) -> Result<Option<StoreLock>, ConfigError> {
        fs::create_dir_all(&self.dir).map_err(io_err(&self.dir))?;
        let lock_path = self.dir.join(MIGRATION_LOCK);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io_err(&lock_path))?;
        let deadline = Instant::now() + wait;
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(Some(StoreLock(lock))),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fs::TryLockError::WouldBlock) => return Ok(None),
                Err(fs::TryLockError::Error(e)) => return Err(io_err(&lock_path)(e)),
            }
        }
    }

    /// Makes `name` the default while holding the store lock.
    pub fn set_default_locked(&self, name: &str, wait: Duration) -> Result<(), ConfigError> {
        let _lock = self
            .store_lock(wait)?
            .ok_or_else(|| ConfigError::StoreBusy(self.dir.join(MIGRATION_LOCK)))?;
        self.set_default(name)
    }

    /// Starts moving `<config-dir>/token` into an account, waiting at most `wait` for another process's move.
    pub fn begin_migration(&self, wait: Duration) -> Result<Option<Migration<'_>>, ConfigError> {
        if !self.legacy_token_file().exists() {
            return Ok(None);
        }
        let lock = self
            .store_lock(wait)?
            .ok_or_else(|| ConfigError::StoreBusy(self.dir.join(MIGRATION_LOCK)))?;
        let Some(token) = read_token_file(&self.legacy_token_file())? else {
            return Ok(None);
        };
        Ok(Some(Migration {
            accounts: self,
            token,
            _lock: lock,
        }))
    }
}

fn free_name(base: &str, names: &[String]) -> String {
    if !names.iter().any(|n| n == base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| with_suffix(base, n))
        .find(|candidate| !names.contains(candidate))
        .expect("some suffix is free")
}

/// Held until dropped.
pub struct StoreLock(File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// A legacy token being moved; holds the migration lock until finished or dropped.
pub struct Migration<'a> {
    accounts: &'a Accounts,
    token: String,
    _lock: StoreLock,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    #[cfg(test)]
    AfterCopy,
}

impl Migration<'_> {
    /// The legacy token, for looking its user up.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Copies the token into an account named after `user` (or `default`), verifies it, then deletes the legacy file.
    pub fn finish(self, user: Option<&AccountUser>) -> Result<String, ConfigError> {
        self.finish_with(user, Fault::None)
    }

    fn finish_with(self, user: Option<&AccountUser>, fault: Fault) -> Result<String, ConfigError> {
        let accounts = self.accounts;
        let names = accounts.names()?;
        let same = names
            .iter()
            .find(|n| accounts.load_token(n).ok().flatten().as_deref() == Some(&self.token));
        let name = match same {
            Some(name) => name.clone(),
            None => {
                let name = match user {
                    Some(user) => accounts.name_for(user)?,
                    None => free_name(FALLBACK_NAME, &names),
                };
                let created = !accounts.exists(&name);
                if let Err(e) = self.copy(&name, user, fault) {
                    if created {
                        let _ = fs::remove_dir_all(accounts.account_dir(&name));
                    }
                    return Err(e);
                }
                name
            }
        };
        let legacy = accounts.legacy_token_file();
        if read_token_file(&legacy)?.as_deref() != Some(self.token.as_str()) {
            return Ok(name);
        }
        match fs::remove_file(&legacy) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&legacy)(e)),
        }
        Ok(name)
    }

    fn copy(
        &self,
        name: &str,
        user: Option<&AccountUser>,
        fault: Fault,
    ) -> Result<(), ConfigError> {
        let accounts = self.accounts;
        accounts.save(name, &self.token, user)?;
        #[cfg(test)]
        if fault == Fault::AfterCopy {
            return Err(ConfigError::Migration("injected fault".into()));
        }
        let _ = fault;
        if accounts.load_token(name)?.as_deref() != Some(self.token.as_str()) {
            return Err(ConfigError::Migration(format!(
                "the copy in {} differs from the original",
                accounts.token_file(name).display()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn store() -> (tempfile::TempDir, Accounts) {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(&dir.path().join("dstreamy"));
        (dir, accounts)
    }

    fn user(id: &str, username: &str) -> AccountUser {
        AccountUser {
            id: id.into(),
            username: username.into(),
            global_name: None,
        }
    }

    fn write_legacy(accounts: &Accounts, token: &str) {
        fs::create_dir_all(accounts.config_dir()).unwrap();
        write_private(&accounts.legacy_token_file(), token.as_bytes()).unwrap();
    }

    #[test]
    fn names_follow_the_alphabet() {
        for ok in ["a", "h_cloud_streamy", "_x", "0", "a.b-c", &"a".repeat(32)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "A",
            ".hidden",
            "-flag",
            "a/b",
            "..",
            "a b",
            "ä",
            &"a".repeat(33),
        ] {
            assert!(
                matches!(validate_name(bad), Err(ConfigError::InvalidAccountName(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn usernames_become_valid_names() {
        assert_eq!(name_from_username("h_cloud_streamy"), "h_cloud_streamy");
        assert_eq!(name_from_username("Mixed.Case"), "mixed.case");
        assert_eq!(name_from_username("..lead"), "lead");
        assert_eq!(name_from_username("ünï"), "n");
        assert_eq!(name_from_username("!!!"), "account");
        assert_eq!(name_from_username(&"x".repeat(40)).len(), MAX_NAME_LEN);
        for username in ["Mixed.Case", "..lead", "!!!", "_u", &"Q".repeat(40)] {
            validate_name(&name_from_username(username)).unwrap();
        }
    }

    #[test]
    fn a_login_without_a_name_reuses_the_users_account_or_takes_a_free_name() {
        let (_dir, accounts) = store();
        let alice = user("1", "alice");
        assert_eq!(accounts.name_for(&alice).unwrap(), "alice");
        accounts.save("alice", "t1", Some(&alice)).unwrap();
        assert_eq!(accounts.name_for(&alice).unwrap(), "alice");
        let other_alice = user("2", "alice");
        assert_eq!(accounts.name_for(&other_alice).unwrap(), "alice-2");
        accounts.save("alice-2", "t2", Some(&other_alice)).unwrap();
        assert_eq!(accounts.name_for(&user("3", "alice")).unwrap(), "alice-3");
        let renamed = user("1", "alice_new");
        assert_eq!(accounts.name_for(&renamed).unwrap(), "alice");
    }

    #[test]
    fn a_suffix_still_fits_a_long_name() {
        let (_dir, accounts) = store();
        let long = "l".repeat(32);
        accounts.save(&long, "t", Some(&user("1", &long))).unwrap();
        let name = accounts.name_for(&user("2", &long)).unwrap();
        assert_eq!(name, format!("{}-2", "l".repeat(30)));
        validate_name(&name).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saved_accounts_are_private_and_the_first_is_the_default() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, accounts) = store();
        assert_eq!(accounts.default_name().unwrap(), None);
        accounts
            .save("b", " tok-b \n", Some(&user("2", "b")))
            .unwrap();
        accounts.save("a", "tok-a", None).unwrap();
        assert_eq!(accounts.names().unwrap(), ["a", "b"]);
        assert_eq!(accounts.default_name().unwrap().as_deref(), Some("b"));
        assert_eq!(accounts.load_token("b").unwrap().as_deref(), Some("tok-b"));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&accounts.token_file("b")), 0o600);
        assert_eq!(mode(&accounts.account_dir("b")), 0o700);
        assert_eq!(mode(&accounts.root()), 0o700);
        assert_eq!(mode(&accounts.account_dir("b").join(USER_FILE)), 0o600);
        let list = accounts.list().unwrap();
        assert_eq!(list[1].user, Some(user("2", "b")));
        assert_eq!(list[0].user, None);
        assert!(list.iter().all(|a| a.has_token));
    }

    #[test]
    fn removing_the_default_moves_it_and_the_last_removes_the_marker() {
        let (_dir, accounts) = store();
        accounts.save("a", "1", None).unwrap();
        accounts.save("b", "2", None).unwrap();
        accounts.save("c", "3", None).unwrap();
        accounts.set_default("b").unwrap();
        accounts.remove("a").unwrap();
        assert_eq!(accounts.default_name().unwrap().as_deref(), Some("b"));
        accounts.remove("b").unwrap();
        assert_eq!(accounts.default_name().unwrap().as_deref(), Some("c"));
        assert_eq!(accounts.marked_default().as_deref(), Some("c"));
        accounts.remove("c").unwrap();
        assert!(!accounts.default_file().exists());
        assert_eq!(accounts.default_name().unwrap(), None);
        assert!(matches!(
            accounts.remove("c"),
            Err(ConfigError::UnknownAccount { .. })
        ));
    }

    #[test]
    fn a_stale_default_marker_falls_back_to_the_first_account() {
        let (_dir, accounts) = store();
        accounts.save("x", "1", None).unwrap();
        accounts.save("y", "2", None).unwrap();
        write_private(&accounts.default_file(), b"gone\n").unwrap();
        assert_eq!(accounts.default_name().unwrap().as_deref(), Some("x"));
        accounts.save("z", "3", None).unwrap();
        assert_eq!(accounts.marked_default().as_deref(), Some("z"));
    }

    #[test]
    fn resolution_order_is_env_then_chosen_then_default_then_legacy() {
        let (_dir, accounts) = store();
        assert_eq!(accounts.resolve(None, None).unwrap(), None);
        write_legacy(&accounts, "legacy");
        assert_eq!(
            accounts.resolve(None, None).unwrap().unwrap().source,
            TokenSource::Legacy
        );
        accounts.save("a", "tok-a", None).unwrap();
        accounts.save("b", "tok-b", None).unwrap();
        let got = accounts.resolve(None, None).unwrap().unwrap();
        assert_eq!(
            (got.source, got.token.as_str()),
            (TokenSource::Account("a".into()), "tok-a")
        );
        let got = accounts.resolve(Some("b"), None).unwrap().unwrap();
        assert_eq!(got.token, "tok-b");
        let got = accounts.resolve(None, Some(" env ")).unwrap().unwrap();
        assert_eq!((got.source, got.token.as_str()), (TokenSource::Env, "env"));
        assert_eq!(
            accounts.resolve(None, Some("  ")).unwrap().unwrap().token,
            "tok-a"
        );
        assert!(matches!(
            accounts.resolve(Some("b"), Some("env")),
            Err(ConfigError::AccountWithEnvToken)
        ));
    }

    #[test]
    fn an_unknown_account_names_the_saved_ones() {
        let (_dir, accounts) = store();
        let none = accounts.resolve(Some("x"), None).unwrap_err();
        assert_eq!(
            none.to_string(),
            "unknown account \"x\"; no account is saved: run `dstreamy login`"
        );
        accounts.save("a", "1", None).unwrap();
        accounts.save("b", "2", None).unwrap();
        let err = accounts.resolve(Some("x"), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown account \"x\"; saved accounts: a, b (see `dstreamy account list`)"
        );
        assert!(matches!(
            accounts.resolve(Some("Bad"), None),
            Err(ConfigError::InvalidAccountName(_))
        ));
    }

    #[test]
    fn an_account_without_its_token_says_how_to_log_in() {
        let (_dir, accounts) = store();
        accounts.save("a", "1", None).unwrap();
        fs::remove_file(accounts.token_file("a")).unwrap();
        let err = accounts.resolve(None, None).unwrap_err();
        assert!(
            err.to_string().contains("dstreamy login --account a"),
            "{err}"
        );
    }

    #[test]
    fn remembering_a_user_skips_removed_accounts() {
        let (_dir, accounts) = store();
        accounts.remember_user("gone", &user("1", "u")).unwrap();
        assert!(!accounts.account_dir("gone").exists());
        accounts.save("a", "1", None).unwrap();
        accounts.remember_user("a", &user("1", "u")).unwrap();
        assert_eq!(accounts.user("a"), Some(user("1", "u")));
    }

    #[test]
    fn migration_moves_the_legacy_token_into_the_users_account() {
        let (_dir, accounts) = store();
        assert!(accounts.begin_migration(WAIT).unwrap().is_none());
        write_legacy(&accounts, "legacy-tok\n");
        let migration = accounts.begin_migration(WAIT).unwrap().unwrap();
        assert_eq!(migration.token(), "legacy-tok");
        let name = migration
            .finish(Some(&user("9", "H_Cloud_Streamy")))
            .unwrap();
        assert_eq!(name, "h_cloud_streamy");
        assert_eq!(
            accounts.load_token(&name).unwrap().as_deref(),
            Some("legacy-tok")
        );
        assert_eq!(accounts.user(&name).unwrap().id, "9");
        assert_eq!(accounts.default_name().unwrap(), Some(name));
        assert!(!accounts.legacy_token_file().exists());
        assert!(accounts.begin_migration(WAIT).unwrap().is_none());
    }

    #[test]
    fn migration_offline_uses_default_and_avoids_a_taken_name() {
        let (_dir, accounts) = store();
        accounts.save("default", "other", None).unwrap();
        write_legacy(&accounts, "legacy");
        let name = accounts
            .begin_migration(WAIT)
            .unwrap()
            .unwrap()
            .finish(None)
            .unwrap();
        assert_eq!(name, "default-2");
        assert_eq!(
            accounts.load_token("default").unwrap().as_deref(),
            Some("other")
        );
        assert_eq!(accounts.default_name().unwrap().as_deref(), Some("default"));
    }

    #[test]
    fn migration_of_an_already_copied_token_only_removes_the_original() {
        let (_dir, accounts) = store();
        accounts.save("kept", "same", None).unwrap();
        write_legacy(&accounts, "same");
        let name = accounts
            .begin_migration(WAIT)
            .unwrap()
            .unwrap()
            .finish(Some(&user("1", "someone")))
            .unwrap();
        assert_eq!(name, "kept");
        assert_eq!(accounts.names().unwrap(), ["kept"]);
        assert!(!accounts.legacy_token_file().exists());
    }

    #[test]
    fn a_failure_after_the_copy_keeps_the_original_and_removes_the_copy() {
        let (_dir, accounts) = store();
        write_legacy(&accounts, "precious");
        let err = accounts
            .begin_migration(WAIT)
            .unwrap()
            .unwrap()
            .finish_with(Some(&user("1", "u")), Fault::AfterCopy)
            .unwrap_err();
        assert!(matches!(err, ConfigError::Migration(_)), "{err:?}");
        assert_eq!(
            read_token_file(&accounts.legacy_token_file())
                .unwrap()
                .as_deref(),
            Some("precious")
        );
        assert!(accounts.names().unwrap().is_empty());
        assert_eq!(
            accounts.resolve(None, None).unwrap().unwrap().source,
            TokenSource::Legacy
        );
        let name = accounts
            .begin_migration(WAIT)
            .unwrap()
            .unwrap()
            .finish(Some(&user("1", "u")))
            .unwrap();
        assert_eq!(
            accounts.load_token(&name).unwrap().as_deref(),
            Some("precious")
        );
    }

    #[test]
    fn a_failure_writing_the_account_keeps_the_original() {
        let (_dir, accounts) = store();
        write_legacy(&accounts, "precious");
        fs::write(accounts.root(), "not a directory").unwrap();
        let err = accounts
            .begin_migration(WAIT)
            .unwrap()
            .unwrap()
            .finish(None)
            .unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");
        assert_eq!(
            read_token_file(&accounts.legacy_token_file())
                .unwrap()
                .as_deref(),
            Some("precious")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_insecure_legacy_token_is_not_migrated() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, accounts) = store();
        write_legacy(&accounts, "t");
        fs::set_permissions(
            accounts.legacy_token_file(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(matches!(
            accounts.begin_migration(WAIT),
            Err(ConfigError::InsecureTokenFile { .. })
        ));
        assert!(accounts.legacy_token_file().exists());
    }

    #[test]
    fn a_held_store_lock_skips_the_migration_within_the_wait() {
        let (_dir, accounts) = store();
        write_legacy(&accounts, "kept");
        let other = Accounts::new(accounts.config_dir());
        let held = other.store_lock(WAIT).unwrap().unwrap();
        let started = Instant::now();
        let err = accounts
            .begin_migration(Duration::from_millis(200))
            .err()
            .unwrap();
        assert!(matches!(err, ConfigError::StoreBusy(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            accounts.set_default_locked("x", Duration::from_millis(50)),
            Err(ConfigError::StoreBusy(_))
        ));
        assert_eq!(
            accounts.resolve(None, None).unwrap().unwrap().source,
            TokenSource::Legacy
        );
        drop(held);
        let name = accounts
            .begin_migration(WAIT)
            .unwrap()
            .unwrap()
            .finish(None)
            .unwrap();
        assert_eq!(name, "default");
    }

    #[test]
    fn a_legacy_file_that_changed_meanwhile_is_kept() {
        let (_dir, accounts) = store();
        write_legacy(&accounts, "first");
        let migration = accounts.begin_migration(WAIT).unwrap().unwrap();
        write_private(&accounts.legacy_token_file(), b"second\n").unwrap();
        migration.finish(None).unwrap();
        assert_eq!(
            read_token_file(&accounts.legacy_token_file())
                .unwrap()
                .as_deref(),
            Some("second")
        );
    }

    #[cfg(unix)]
    #[test]
    fn saving_into_an_existing_directory_makes_it_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, accounts) = store();
        fs::create_dir_all(accounts.account_dir("loose")).unwrap();
        fs::set_permissions(
            accounts.account_dir("loose"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        accounts.save("loose", "t", None).unwrap();
        let mode = fs::metadata(accounts.account_dir("loose"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
}
