# Project: Tool to help handle mirroring of projects to Gitea

## General Instructions

This is a rust library, so ensure you are writing idiomatic rust code (using mutability as rarely as possible, borrowing where needed, etc). Always run `cargo check` and `cargo test` and `cargo clippy` after any changes to maintain general project quality.

Look at the `vibe_coding_log` directory (especially the `README.md` file) where you will track conversations for audit/archival purposes.

Any changes you do should be as minimal as possible to the underlying code (to make code review easier). Also, follow coding styles that already exist and do not deviate from them. If a larger refactor is strongly encouraged at this point, either to make changes easier to implement or things are getting "messy", then say so and ask for permission first.

When creating commits, look at past commit messages and try to follow that style (so no smileys, don't start titles with "feat:", etc).
