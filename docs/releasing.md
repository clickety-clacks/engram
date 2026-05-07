# Releasing Engram

Engram releases are tag-driven.

## Required repository secrets

Set these on `clickety-clacks/engram` before cutting a release:

- `NPM_TOKEN`: npm automation or granular token with publish access to `@clickety-clacks/engram`.
- `HOMEBREW_TAP_TOKEN`: GitHub token with write access to `clickety-clacks/homebrew-tap`.

Do not commit either token.

## Cut a release

1. Update the version in both files:
   - `Cargo.toml`
   - `package.json`
2. Commit and push the version bump.
3. Tag the release:

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

The GitHub Action will:

1. verify the tag matches `Cargo.toml` and `package.json`
2. build release binaries for macOS arm64, Linux x64, and Windows x64
3. create the GitHub Release and upload those binaries
4. publish `@clickety-clacks/engram` to npm
5. update `clickety-clacks/homebrew-tap/Formula/engram.rb` with the new version and SHA256 values

## Verify after release

```bash
npm view @clickety-clacks/engram version
npm install -g @clickety-clacks/engram
brew update
brew reinstall clickety-clacks/tap/engram
engram --help
```

## Notes

The npm package downloads the matching GitHub Release binary in `postinstall`, so npm publishing intentionally runs after the GitHub Release asset upload.
