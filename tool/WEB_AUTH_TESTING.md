# Web verification testing

## Production implementation — 2026-09-08

The application now includes a separate Konachan.com source and site verification independent of account login. Listing and image errors classified as `ChallengeRequired` offer a browser button. Returning from the embedded browser or closing the Windows window captures native cookies and the browser User-Agent. Rust validates a public listing in an isolated candidate context before atomically saving and publishing the session. Successful publication refreshes the waterfall and invalidates prepared media requests through revision notifications.

Settings → Network → Web verification provides per-source verification, per-source clearing and clearing all browser verification data. Account credentials are separate. Failed/cancelled attempts preserve an existing verified session; clearing or changing network configuration invalidates pending candidates. Saved sessions are revalidated after engine recreation. Windows records use current-user DPAPI encryption; Unix records use a private directory and mode-0600 files. Diagnostics exclude cookie values and browser identity.

Windows builds enable `web-challenge-window` through the native-assets hook. One owning UI thread runs WebView2 with isolated temporary profiles; native browsing-data cleanup finishes before the profile is removed. Android uses native `CookieManager.getCookie(url)` for bounded root-URL snapshots without fabricated metadata. iOS/macOS use the WebView plugin; Linux collection is unavailable. Mobile custom proxies are rejected because equivalent browser routing is not implemented.

Automated production checks:

```powershell
cargo test --manifest-path rust/Cargo.toml --features web-challenge-window --locked --lib
cargo test --manifest-path rust/Cargo.toml --features web-challenge-window --locked --lib repeated_native_capture_and_cleanup -- --ignored
flutter analyze
flutter test
flutter build windows --debug --no-pub
```

Run Windows tests as the normal desktop user: a restricted execution identity can fail DPAPI operations. Local native tests use fixture HTML, verify HttpOnly/parent-domain/path/value preservation and completed clearing, then repeat on the same event loop.

The manual production acceptance test opens Konachan.com in an isolated temporary profile. Solve any challenge manually and **close the window**. The test checks real candidate validation and persistence, engine recreation with network revalidation, clearing and another recreation. It prints only fixed stage names, removes temporary records, and never uses the application's saved credentials:

```powershell
cargo test --manifest-path rust/Cargo.toml --features web-challenge-window --locked --lib live_challenge_capture_restore_clear -- --ignored --nocapture
```

Limitations: no Android SDK is installed on this machine, so Android build/device validation is pending. iOS/macOS, custom proxies and DNS fallback combinations need platform acceptance. Account login is outside this feature's scope. The historical spike and its earlier gating statements below describe the prototype, not the production implementation above.

Observed production results on this machine: Flutter static analysis passed; all 145 Flutter tests passed (2 skipped); 93 core Rust tests passed (9 live tests skipped in the default build); the local native repeated-capture test passed; the final Windows debug application built successfully. The atomic replacement test forces a Windows file-sharing failure and verifies unchanged disk bytes, live browser identity and revision before retrying successfully. The fresh manual production CF attempt timed out after 600 seconds without completing the close/capture flow; it cleaned its temporary browser data and did not establish real CF persistence/restart acceptance. The earlier successful CF listing/preview/original replay belongs to the spike.

## Historical compatibility spike

Run from a regular Windows desktop PowerShell session. Microsoft Edge WebView2 Runtime and the Rust toolchain listed in `DEVELOPMENT.md` are required.

```powershell
# Local WebView2 test; does not navigate to a remote site or solve a challenge.
./tool/test_web_auth_windows.ps1 -SelfTest

# User-driven Anime-Pictures compatibility test.
./tool/test_web_auth_windows.ps1

# Cloudflare-focused test against konachan.com (no account login required).
./tool/test_web_auth_windows.ps1 -Source konachan.com
```

In the test window, complete any challenge and log in manually. Cloudflare may not show a challenge on every visit. Select **会话测试 → 验证并测试 Rust 请求** to capture the browser session and test the existing Rust network path. Rejected attempts leave the window open for another try. Close the window to cancel; it also closes after ten minutes.

The console and window title show stage results for the account endpoint (Anime-Pictures only), listing API, a general-rated preview and an original image. Media is downloaded to a temporary directory using the production download validation, then deleted. A successful listing alone is not reported as a successful account login. The account response parser in this spike remains provisional until checked against a real logged-in response. Konachan.com uses an isolated Moebooru configuration; the application's existing Konachan.net source is unchanged.

This opt-in example uses an isolated browser profile and an isolated in-memory SessionStore. It does not read or replace the application's saved credentials. It currently uses system proxy settings with network fallback off; it does not yet reproduce custom proxy/fallback settings, durable session import or application restart recovery. These remain separate acceptance checks before enabling the feature in Settings.

The Windows native Cookie reader preserves the raw domain, HttpOnly, secure, path and expiry fields. Using Wry's `Cookie::domain()` directly loses the leading dot required by the current SessionStore's domain semantics. Browsing-data cleanup waits for the native completion callback before closing the WebView.

Only fixed stage names, error codes and HTTP status are printed. Do not add Cookie values, User-Agent, response bodies or raw WebView results to diagnostics.

## Observed results — 2026-09-08

| Check | Result |
| --- | --- |
| Windows native WebView2 local fixture | Passed: HttpOnly, parent domain, API path, expiry, values containing `=`, completed deletion |
| Konachan.com challenge | User confirmed the challenge appeared and was completed manually |
| Konachan.com Rust replay | Listing API, complete preview download and complete original download passed in the same attempt |
| Anime-Pictures | Browser opened; a real authenticated account response and complete authenticated replay remain unverified |
| Flutter Windows debug build | Passed with the spike dependencies excluded from the application build |
| Android / other desktop platforms | Not tested |
| Custom proxy / fallback / durable restart | Not tested |

The Konachan.com result establishes one successful replay on this Windows machine. It does not satisfy the Anime-Pictures account, restart, Android or custom-network acceptance gates in the implementation plan. Keep the Settings entry disabled until its relevant gates pass.

SessionStore regression coverage also verifies that same-name cookies keep their path scopes, empty values and embedded equals survive, malformed/oversized replacements leave the old session intact, and concurrent updates return distinct revisions published with their corresponding sessions.

## Transaction prototype

`rust/src/web_auth_spike/transactions.rs` is an opt-in backend prototype tested with synthetic credentials. Its tests cover detached candidate validation, exact import domains, cancellation, timeout, network invalidation, revision supersession, duplicate completion, atomic Windows replacement failure and retry, detached/unverified restore, corrupt-record retention and logout tombstones. The real browser harness continues to keep captured credentials in memory; it does not call this persistence prototype.

This is not the production import API. Before integration it still needs the shared application owner across engine recreation, globally exclusive collectors, source-specific credential sending rules, URL snapshot material, confirmed account parsing, versioned capture rules and verification metadata, private production storage permissions, Hive migration and platform IPC. Prototype records must not be migrated as production credentials. Account-dependent tests are deferred because no Anime-Pictures account was available for this test session.
