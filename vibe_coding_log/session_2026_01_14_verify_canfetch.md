# Session 2026-01-14: Verify CanFetch Flag

**Date**: 2026-01-14
**Model**: Gemini Pro (CLI Agent)
**Goal**: Verify migration error detection and add a `--verify_canfetch` flag to check repository accessibility (fetching) without using the `git` executable.

## Plan
1.  Add `git2` dependency to `Cargo.toml`.
2.  Add `--verify_canfetch` flag to `Args`.
3.  Implement a helper function `verify_repo_accessible(url: &str)` using `git2`.
4.  Integrate this check into the "Existing Repos" discovery phase and the "Post-Migration" phase.
5.  Refine error handling to exit on verification failures.

## Outcome
- Added `git2` v0.19 to `Cargo.toml`.
- Implemented `verify_repo_accessible` using `git2::Remote::create_detached` and `connect_auth`.
- Added `--verify_canfetch` CLI flag.
- The tool now optionally verifies that existing repositories and newly migrated ones are reachable and non-empty (contain refs).
- Implemented strict error handling for verifications:
    - If "Existing Repos" verification fails for *any* repo, the tool exits with an error *before* calculating/printing the execution plan.
    - If "Post-Migration" verification fails for any repo, the tool continues to attempt other migrations but exits with an error at the very end to indicate failure.
