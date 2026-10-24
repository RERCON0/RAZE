# Dependency review, 2026-10-25

- No open GitHub Dependabot security alerts at review time.
- RUSTSEC-2026-0386: upgrade Tokio to 1.53.3. The affected AsyncFd API is Unix-only; RAZE runs on Windows.
- Replace yanked clipboard-win 5.4.1 with 5.4.3.
- Dependabot PR #1 proposes eframe 0.37. It is a routine 0.x API migration, not a reported vulnerability, and is kept separate from this patch release.

Verdict: Dependabot version PRs are useful maintenance leads, but an empty alert list does not replace an up-to-date RustSec audit. Findings above are checked against the actual dependency graph and application use. Informational maintenance notices are tracked with their migration trigger.
