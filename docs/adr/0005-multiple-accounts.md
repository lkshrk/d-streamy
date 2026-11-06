# 0005 — Several saved Discord accounts

Status: proposed (2026-11-06). Amends 0004 (the session lock is per account; new control commands).

## Context

D-Streamy kept one token at `<config-dir>/token`. Users with a second Discord account (a burner for streaming, a friend's account on the same machine) had to log in again to switch, and could not stream from OBS through `serve` with one account while `dstreamy stream` streams another. Discord allows one voice connection per account, so two accounts can stream at once; the same account twice evicts the first (`StopReason::VoiceTakenOver`).

The scope is deliberately low-effort: `serve` still runs one session at a time. Two accounts stream at once only as two processes, for example OBS through `serve` with account A and `dstreamy stream --account B`.

## Decision

### Storage

```
<config-dir>/
  config.toml                  global settings, unchanged
  default-account              the default account's name and a newline
  accounts.lock                held while the legacy token is migrated
  accounts/<name>/token        0600, written atomically (temp file + rename)
  accounts/<name>/account.toml 0600: id, username, global_name of the Discord user
  accounts/<name>/session.lock the account's session lock (and session.pid)
  session.lock                 the session lock when the token comes from DSTREAMY_TOKEN
```

- The `accounts` directory and each account directory are created owner-only (0700 on Unix); saving into an existing account directory sets it back to 0700. Every file write is a temp file, rename, and an fsync of the directory.
- `account.toml` is written by the login, and filled in on the first Discord connection of an account that lacks it (an account migrated offline, or saved by `set_token`). A missing or broken `account.toml` only costs the user name in lists.
- **Names** are 1 to 32 characters of `a-z`, `0-9`, `_`, `.` and `-`, starting with a letter, digit or `_`. Upper case is refused rather than folded, because macOS file systems ignore case and two names differing only in case would share a directory. Discord usernames already fit (lower case, digits, `_`, `.`).
- **Default account:** `default-account` when it names an existing account, else the first account in name order. The first account saved becomes the default. Removing the default account moves the default to the first remaining one, or deletes the file when none is left.
- **Choosing an account**, for every command that needs a token:
  1. `DSTREAMY_TOKEN`, when set and not blank, wins and means "no account management": no account is chosen, `--account` is refused with an error saying so, and the session lock is `<config-dir>/session.lock`, as before. `login` and the `account` commands still manage the saved accounts.
  2. `--account <name>`, which must exist. An unknown name fails with `unknown account "<name>"`, followed by the saved names, or by `no account is saved; run dstreamy login` when there are none.
  3. The default account.
  4. The legacy `<config-dir>/token`, only while its migration has failed (below).
  5. Otherwise `not logged in: run dstreamy login`.

### Login naming

- `dstreamy login --account <name>` saves into that account, creating it or replacing its token. If the account already belongs to another Discord user (its `account.toml` names a different id), the login is refused with `signed in as X, but account Y belongs to Z: …` and nothing is saved; remove the account first to reuse the name. It is how a user names an account, or logs an account in again.
- `dstreamy login` without `--account` saves into the account whose `account.toml` has the same Discord user id, so logging in again never duplicates an account. Otherwise it creates an account named after the Discord username: lower-cased, characters outside the name alphabet dropped, leading `.` and `-` dropped, cut to 32 characters, `account` if nothing is left. If that name is taken by another user, it gets `-2`, `-3` and so on (the base is shortened so the suffix fits).
- A CLI login never changes the default, except that the first account becomes it. The CLI prints the account it saved into and, when that is not the default, how to use it (`--account` or `dstreamy account use`). A login through serve does make its account the default (see `serve`).

### Migration of `<config-dir>/token`

Runs once, at the start of any command that reads a token or the accounts (`login`, `guilds`, `channels`, `stream`, `tui`, `daemon`, `serve`, `account …`), only when `<config-dir>/token` exists:

1. Take `accounts.lock`, so a `serve` and a CLI starting together don't both migrate, and check the file still exists. The lock is polled for at most the command's migration limit (15 s; 5 s for `serve` and `daemon`); when another process holds it longer, the migration is skipped and the command uses the legacy token (choice 4).
2. Read the legacy token with the usual checks. An insecure mode or a read error stops here with that error, as before.
3. If a saved account already holds the same token (a migration that stopped after the copy), only step 6 is left.
4. Look the user up with one brief Discord connection, within what is left of the limit; the lock wait leaves the lookup at least 3 s. The account is named after the user as in a login without `--account`; offline, rejected or timed out, it is named `default` (with a suffix if taken), and its `account.toml` is filled in on its first successful connection.
5. Write the account's token and `account.toml` (fsynced), read the token back and compare it. Make it the default if there is none.
6. Only then delete `<config-dir>/token`, and only if it still holds the token that was copied (re-read under the lock); a file that changed meanwhile is kept and migrated next time.

If any step before 6 fails, the account directory it created is removed again, `<config-dir>/token` stays, the command warns once and goes on with the legacy token (choice 4 above, with the config-directory lock). The migration is retried on the next command. So the token can be lost by none of these failures: the original is deleted only after a verified copy exists.

Older builds read only `<config-dir>/token`, so after a migration they report "not logged in". Copying `accounts/<name>/token` back to `<config-dir>/token` works only until the next command of a new build, which finds that token already saved and deletes the copy again. The lasting ways back are to stop running the new build, or to give the old build its token through the environment: `DSTREAMY_TOKEN="$(cat <config-dir>/accounts/<name>/token)" dstreamy …` (or the service unit's environment), which no build migrates or deletes.

### Session lock

- The lock of ADR 0004 moves into the account: `accounts/<name>/session.lock` and `session.pid`. A session takes the lock of the account its Discord connection was made with. So `serve` streaming A and `dstreamy stream --account B` run at once, while A twice is still refused with `already_running` and the PID.
- `stream`, `tui`, `daemon` and `serve` all take it at the same point as before. With `DSTREAMY_TOKEN` or the legacy fallback the lock stays `<config-dir>/session.lock`.
- Not protected, as before with different `--config-dir`s: the same Discord user saved under two names, `DSTREAMY_TOKEN` holding a saved account's token, and an older build streaming next to a newer one.
- `account remove` and serve's `logout_account` take the account's lock before deleting it, so an account streaming in another process is refused (`already_running`) instead of being deleted under it. Removal locks the account directory without needing its token, so an account whose token is gone can still be removed. Taking a session lock never creates a missing account directory, and after locking it checks that the account still has its token; an account removed meanwhile fails with `account <name> was removed meanwhile` (`unknown_account` in serve) instead of reappearing as an empty directory.

### Per account and global

- Per account: the token, the Discord user, the session lock, and the OBS plugin's saved server and channel.
- Every change of the saved accounts or the default (`login`, `account use`, `account remove`, serve's `select_account` and `logout_account`, the migration) holds `accounts.lock`, waiting at most 2 s outside the migration.
- Global, unchanged: `config.toml` (stream settings, capture target and crop, audio source, `[serve]`), and the other OBS plugin settings (the dock's tracks and sources, the managed track).
- The plugin keeps the server and channel per account in its dock config. On a switch it restores that account's last server and channel, or none if it never picked one. The first time it sees an active account, the old global pick moves to that account. A saved server the account isn't in shows as not available in the picker, with "Pick the server again" and Start disabled.

### `serve`

- serve has one **active account**: `serve --account <name>` pins it; otherwise it is the default account, read at each connection. `select_account` (from the panel or dialog) writes the default under `accounts.lock` (waiting at most 2 s), so the pick persists across serve restarts and the CLI uses it without `--account`. A successful `login` through serve also makes its account the default.
- serve still runs one session at a time. The connection remembers the account it was made with; a command that needs Discord and finds the connection on another account than the active one (after a select, a login, a logout, or a changed default while serve follows it) replaces the idle connection, as `Engine::token_changed` does for a new token. A live session keeps its connection; the change applies when it is over.
- New feature flag `accounts` in `hello` `features`, absent when serve takes its token from `DSTREAMY_TOKEN` (as `browser_login` is). New control commands, each refused with `accounts_unavailable` in that case:

| `cmd` | fields | reply `data` |
|---|---|---|
| `accounts` | — | `{"active": name\|null, "accounts": [{"name", "username", "global_name", "has_token", "active"}]}`, in name order; `username` and `global_name` are `null` while unknown |
| `select_account` | `account` | the `accounts` reply. Makes the account the default. `not_logged_in` for an account without a saved token, so the default never points at one; `unknown_account` (the message lists the saved names), `streaming` while a session is live, `config` when another process holds `accounts.lock` longer than 2 s |
| `logout_account` | `account` | the `accounts` reply. Deletes the account's token and directory. `unknown_account`; `streaming` while serve's live session uses it; `already_running` (with the PID) while another process streams it. Logging out the active account makes serve follow the default again |

- `login` takes an optional `account`: the window's login is saved there (`dstreamy login --account <name>`), and refused with `login_failed` and the message above if the browser signed in as another user than the account's; without it, the login is named as in `dstreamy login`. The reply adds `account`, the name it was saved under, and serve makes that account the default and active one. An invalid name is `invalid_params`. The plugin's "Log in again" sends the active account, so it repairs that account; `Add account…` sends none. An older client sends no `account`; logging in again as the same Discord user then lands in the same account.
- `status` (and so `hello`'s `status`) adds `account`: `{"name", "username", "global_name"}` of the live session's account, or of the active account while idle; `null` with no account or with `DSTREAMY_TOKEN`.
- An older plugin ignores the new fields and keeps working with the active account. A newer plugin hides the account row when `features` lacks `accounts`.

### OBS plugin

- The panel and the settings dialog get an always-visible account row: `Account  [name ▾]  [Log out]`. The picker lists the saved accounts and ends with `Add account…`, which opens serve's login window for a new account (`login` without `account`). Picking an account sends `select_account` (it becomes the default), the picker shows the pending pick until serve answers, then the lists load again. Log out asks first (`Log out <name>?  [Log out] [Not now]`), then sends `logout_account`; with no account left the row is replaced by the existing `Log in` button.
- While live, the picker and Log out are disabled with the tooltip `Stop the stream to switch accounts` / `Stop the stream to log out`; while a login window waits, with `Finish or cancel the login first`.
- The panel's host table gains `select_account(ctx, name)`, `add_account(ctx)`, `logout_account(ctx, name)` (which asks for the confirm) and `confirm_logout(ctx, yes)`, and the view JSON an `account` object; the panel ABI goes to 6. A library and plugin of different ABIs refuse each other and the settings dialog remains, as at every ABI bump.

### CLI

- Global `--account <name>`.
- `dstreamy account list`: the accounts with the default marked (`--json`: `[{name, username, global_name, default, has_token}]`), and a note when `DSTREAMY_TOKEN` overrides them.
- `dstreamy account use <name>`: makes it the default.
- `dstreamy account remove <name>`: deletes its token and directory, refused while that account streams.
- `login`, `guilds`, `channels`, `stream`, `tui` and `daemon` use the chosen account. `daemon`'s `set_token` (protocol 1) saves into the chosen account, or, when none is saved, into a new account named after the token's user.

## Consequences

- Two accounts stream at once without a multi-session serve.
- The legacy token moves on first use; going back to an older build needs `DSTREAMY_TOKEN` or a new login (above).
- The account picked in OBS is the default for the CLI as well; `--account` or `dstreamy account use` overrides it.

- Going back to an older plugin drops the per-account picks: it rewrites the dock config without the `account` and `picks` fields, keeping only the current server and channel. Accepted.

## Decided by the user (2026-11-06)

- The panel's pick becomes the default and persists across serve restarts.
- The plugin remembers the server and channel per account.
- Login while live stays refused, including `Add account…`, as before.
