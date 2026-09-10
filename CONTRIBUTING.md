# Contributing to Chop

Chop helps people review and remove Git worktrees. Contributions must preserve its confirmation and recovery guarantees.

Bug reports, documentation fixes, tests, and code changes are welcome. Follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Discuss a change

Search [issues](https://github.com/alberto-castano/chop/issues) before opening a new report.
For a new feature or a large change, open an issue first. Describe the problem and the expected behavior.
Small fixes can go directly to a pull request.

For bugs, include:

- Your operating system, terminal, Git version, and Chop version.
- The command or key sequence that triggers the problem.
- The expected result and the actual result.
- Steps to reproduce the problem with a sample repository.

Remove private paths, repository names, credentials, and file contents from reports and screenshots.
Report security problems through the private channel in [SECURITY.md](SECURITY.md).

## Set up a checkout

Use macOS or Linux. Install Git, Python 3, and the current stable Rust toolchain with `rustfmt` and Clippy.
Chop requires Rust 1.88 or later. CI uses stable Rust on both operating systems.

Fork the repository, then clone your fork:

```sh
git clone https://github.com/YOUR_USERNAME/chop.git
cd chop
git switch -c fix/short-description
rustup component add rustfmt clippy
```

Build and check the local version:

```sh
cargo build --locked
cargo run --locked -- --version
```

## Test changes safely

Use disposable sample repositories for manual testing. Do not test removal against unfinished work or your normal repository folders.

Preview a sample root without removing worktrees or saving configuration:

```sh
cargo run --locked -- all --root /path/to/sample-repositories --dry-run
```

To test the full TUI, run the same command without `--dry-run` against disposable data only.
Review the plan before confirming removal. See the [guide](docs/guide.md) for controls and recovery behavior.

## Make the change

- Keep each pull request focused on one problem.
- Use simple Rust code and the existing formatting and module structure.
- Add a regression test for a bug when practical.
- Preserve checks for local changes, protected worktrees, and changes made after inspection.
- Keep displayed paths and Git output safe for terminals.
- For TUI changes, check small terminals, keyboard controls, and behavior with color disabled.
- Use sample data for before-and-after screenshots of visible changes.
- Update the guide and the `Unreleased` changelog when user-visible behavior changes.

Keep dependencies to a minimum. Explain why a new dependency is needed in the pull request.
Commit changes to `Cargo.lock` when dependencies change. Do not change the package version for an ordinary contribution.

## Run the checks

Run the same checks as CI before submitting code:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -m unittest discover -s .github/scripts -p 'test_*.py'
```

Use `cargo fmt` to fix formatting. For documentation-only changes, check links, commands, and Markdown rendering.

## Submit a pull request

Push your branch to your fork and open a pull request against `main`.
Explain the problem, the resulting behavior, and the checks you ran. Link the related issue when one exists.
Include safe before-and-after screenshots for TUI changes. State any known limitations or checks you could not run.

Use a short Conventional Commit title, such as `fix: preserve the tree outline during scanning` or `docs: clarify setup`.
Respond to review feedback and keep CI passing. Maintainers handle releases through the [release workflow](docs/releasing.md).

## License

By submitting a contribution, you agree to license it under the project's [MIT License](LICENSE).
