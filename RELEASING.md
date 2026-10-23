# Releasing

One version number lives in three places that must agree: `Cargo.toml`, `herdr-plugin.toml`
(generated from `src/setup.rs`, which reads the crate version) and the `v<version>` tag. The
release workflow refuses a tag that does not match.

## A release

1. Bump `version` in `Cargo.toml`, then regenerate the plugin manifest and run the checks:

   ```sh
   UPDATE_MANIFEST=1 cargo test         # rewrites herdr-plugin.toml (new version) and config.example.env
   cargo build --release && cargo test
   cargo package --list --allow-dirty   # what crates.io would get; the bump is not committed yet
   ```

2. Commit and tag, then push **only the tag** (once the repository is public; for the first
   release see [The first release](#the-first-release)); `.github/workflows/release.yml` builds
   `claude-deck-v<version>-<target>.tar.gz` (+ `.sha256`) for `aarch64-apple-darwin`,
   `x86_64-apple-darwin`, `aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl`, and
   creates the GitHub release:

   ```sh
   git commit -am "v0.3.0"
   git tag v0.3.0
   git push origin v0.3.0
   gh run watch
   gh release view v0.3.0 --json assets --jq '.assets[].name'   # all 8: 4 .tar.gz, 4 .sha256
   ```

   Push `main` only once all eight are there:

   ```sh
   git push origin main
   ```

   `herdr plugin install` and `herdr plugin update` read the manifest on the default branch,
   and its build step downloads the release of the version that manifest names. Pushed before
   the release exists, a new version on `main` breaks installs until the workflow finishes, or
   for good if it fails.

   If the workflow fails, or the release is missing files, take the tag back, fix the cause
   and tag again (`main` has not moved, so installs are unaffected):

   ```sh
   gh release view v0.3.0 && gh release delete v0.3.0 --yes   # only if a release was created
   git push --delete origin v0.3.0
   git tag -d v0.3.0
   # fix, commit, then tag again:
   git tag v0.3.0
   git push origin v0.3.0
   ```

   The Linux binaries are static (musl), so they do not depend on the distribution's glibc version.

3. Update the Homebrew tap (`bigsan/homebrew-tap`, formula at `Formula/claude-deck.rb`):

   ```sh
   scripts/brew-formula.sh 0.3.0 > ../homebrew-tap/Formula/claude-deck.rb
   cd ../homebrew-tap && git commit -am "claude-deck 0.3.0" && git push
   brew update && brew upgrade claude-deck && brew test claude-deck
   ```

4. Publish the crate: `cargo publish` (after `cargo publish --dry-run`).

The herdr marketplace picks the new manifest up by itself (see below), and
`herdr plugin update` installs the release binary of the new version.

## The first release

While the repository is private nobody else can install it, so the first release pushes
`main` first and then the tag; the release workflow runs from the tag either way:

```sh
gh repo create bigsan/claude-deck --private --source . --remote origin
git push -u origin main
git tag v0.2.0
git push origin v0.2.0
gh run watch
gh release view v0.2.0 --json assets --jq '.assets[].name'   # all 8: 4 .tar.gz, 4 .sha256
```

Pushing only the tag, then `main` once the release is complete, applies to releases after the
repository is public.

## The herdr marketplace

From the herdr 0.9.5 docs ([Marketplace](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/marketplace.mdx),
[Plugins](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/plugins.mdx)):

- [herdr.dev/plugins](https://herdr.dev/plugins/) lists **public** GitHub repositories with the
  topic `herdr-plugin` whose default branch has at least one `herdr-plugin.toml` with parseable
  required metadata (`id`, `name`, `version`, `min_herdr_version`). There is no submission or
  review; forks, archived repositories and malformed manifests are left out.
- The index refreshes every 30 minutes and rescans a repository when its default branch moves.
- `herdr plugin install owner/repo[/subdir]` clones the repository with `git`, shows a preview,
  runs the `[[build]]` commands in the new installation directory and registers the plugin.
  It is documented for public repositories.

## While the repository is private

- `herdr plugin install bigsan/claude-deck` is not available to others, and the build step's
  download and the Homebrew formula's URLs need anonymous access to the release assets, so
  both fail (the build step then falls back to a `claude-deck` already installed).
- The marketplace does not list it, topic or not.
- The release workflow runs, on the account's Actions minutes (macOS minutes count extra).
- What works: `cargo install --path .` from a clone, then `claude-deck setup herdr`.

Once it is public: add the `herdr-plugin` topic (`gh repo edit bigsan/claude-deck --add-topic
herdr-plugin`), create the Homebrew tap from the first release (step 3 above) and publish the
crate (step 4); later releases follow [A release](#a-release).
