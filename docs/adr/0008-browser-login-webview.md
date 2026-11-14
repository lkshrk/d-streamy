# 0008 — Browser login through an embedded webview

Status: accepted (2026-11-06). Details 0004 (`login` through serve) and 0005 (accounts).

## Context

`dstreamy login` needs a Discord user token. The QR login works with the Discord mobile app, and `--token` needs the user to copy the token out of a browser's developer tools, which is error-prone and teaches careless token handling. OBS users have no terminal in front of them when the plugin asks them to log in.

## Decision

- **`dstreamy login --browser` opens Discord's own login page in a window** (`rust/crates/dstreamy-cli/src/browser_login/`): `wry` and `tao`, which is WebKitGTK 4.1 on Linux and WKWebView on macOS. The token is read from the page's API requests by an injected script, validated against the gateway, and only then saved into an account (ADR 0005). It is never shown.
- **The feature is opt-in at build time** (`browser-login` in `rust/crates/dstreamy-cli/Cargo.toml`) and on in the Linux and macOS release tarballs. A build without it says so and points to the QR or token login. QR login stays the default `dstreamy login`.
- **serve opens the window as a child** (`dstreamy login --browser --window-only`, ADR 0004's `login` command) with the client's graphical session variables, so the OBS panel's Log in works even when serve runs under systemd without a display. The window closes after 10 minutes or when the client that asked for it disconnects.
- **The page is not trusted.** Messages from the webview carry a per-window nonce, because wry on WebKitGTK labels every message with the top page's URI; a message without the nonce is dropped. The webview only navigates to Discord's hosts, `about:blank`/`about:srcdoc`, Discord `blob:` URLs and the captcha frames (`hcaptcha.com` and its subdomains, `challenges.cloudflare.com`, reCAPTCHA under `www.google.com/recaptcha/` and `www.recaptcha.net/recaptcha/`); wry can't tell frames apart, so the captcha hosts are allowed in the top frame too. Other navigations are blocked and logged by host. The page gets no script clipboard access; keyboard paste still works. A navigation that still leaves Discord's origin, a failed load or a cancelled login ends with no token saved and a recovery page that offers to retry or cancel.

## Consequences

- The Linux release binary links WebKitGTK 4.1 and fails to start without its runtime packages (README); a source build without the feature needs none of them.
- A login that works in the window proves the page flow, not Discord's rules: the token is used like any other user token (README, Disclaimer).
- macOS window focus and page-load reporting are verified less often than Linux, because the manual check runs on Linux.

## Rejected

- Opening the system browser: there is no way to read the token back.
- Linking the webview into every build: it adds WebKitGTK to builds that only need the CLI.
- Making the token login the only path: see Context.
