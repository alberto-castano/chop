# Releasing Chop

Chop publishes tagged source on GitHub. Homebrew compiles that source during installation.

## One-time setup

1. Merge `.github/workflows/update-chop.yml` into `alberto-castano/homebrew-tap` on `main`.
2. Merge the release workflow and Linux CI changes into Chop on `main`.
3. Create a fine-grained token scoped to the tap, with Actions read/write and Contents read permissions.
4. Save it as the `HOMEBREW_TAP_TOKEN` Actions secret in Chop.
5. Allow the tap workflow to push formula updates to `main` under the repository's branch rules.

The tap uses its own `GITHUB_TOKEN` for formula commits. Chop's token dispatches and watches the tap workflow.

## Prepare a version

1. Update `Cargo.toml` and Chop's entry in `Cargo.lock` to the same stable version.
2. Move completed `Unreleased` notes into a section such as `## 0.3.0 - 2026-09-10`.
3. Keep an empty `Unreleased` section for subsequent changes.
4. Merge these changes through a PR and wait for the `main` CI run to pass.

## Publish

Open **Actions → Release → Run workflow**. Select `main` and enter the version without `v`.

The workflow uses the commit selected when the run starts. It checks version metadata and successful push CI for that commit.
It also checks tap credentials before creating the version tag.

The workflow then installs the tagged source on macOS and Linux and checks `chop --version`.
After verification, it publishes the GitHub release with the dated changelog notes.
Finally, it dispatches the tap update and waits for its result.

The tap downloads the published source, calculates SHA-256, and updates the formula.
It removes the old formula revision when the upstream version changes.
It installs and tests the formula with Homebrew before committing the update.

## Retry a failed run

Use **Re-run failed jobs** on the original run. Existing tags must still identify that run's exact commit.
Published releases are preserved. Do not move an existing release tag to another commit.

If only the tap failed, rerun its failed job or dispatch **Update Chop** in the tap repository.
Enter the published version and a unique request identifier. The tap rejects version downgrades.
This also permits a tap retry after Chop's `main` branch advances.

A successful GitHub release followed by a failed tap job means publication succeeded but Homebrew remains outdated.
Check the tap job before announcing that the release process is complete.
