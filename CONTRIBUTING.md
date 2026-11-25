# Contributing to cvenv

Bug reports, fixes and improvements are welcome. For larger changes, open an issue first so the
approach can be discussed before you spend time on it. Security issues are reported privately;
see [SECURITY.md](SECURITY.md).

By participating you agree to follow the [code of conduct](CODE_OF_CONDUCT.md).

## Development setup

cvenv is a Rust CLI. The toolchain comes from conda-forge through [pixi](https://pixi.sh), so
pixi is the only prerequisite.

```bash
git clone https://github.com/amarchakitus/cvenv && cd cvenv
pixi run build    # static binary in bin/cvenv
pixi run test     # unit tests
pixi run lint     # rustfmt --check and clippy -D warnings
pixi run fmt      # apply rustfmt
```

End-to-end tests: `pixi run e2e`.

`./installation.sh --from-source` builds `bin/cvenv` the same way and installs it, which is the
easiest way to try a change on a cluster. Cargo's registry and target directories are kept on
node-local scratch (`/tmp` by default, `CVENV_BUILD_TMPDIR` to change), so builds add no files to
quota'd filesystems.

CI runs lint, tests and the release build on x86_64 and aarch64 for every pull request, plus
shellcheck on `installation.sh` and `scripts/*.sh`. All checks must pass before merging.

## Pull requests

- Pull requests are merged by rebasing, without squashing, so every commit lands on `main` as
  is. Keep commits logical and self-contained (for example, a formatting-only change separate
  from a behavior change) and give each one a descriptive message.
- Describe what changed for users and how you tested it, including the backend (Apptainer or
  squashfuse) and the cluster setup where relevant.
- Document user-facing changes in the README or `docs/`, and add an entry under
  `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md). Call out behavior changes explicitly.

## Releases

1. Open a pull request that bumps `version` in `Cargo.toml` and `Cargo.lock`, and moves the
   `Unreleased` entries in `CHANGELOG.md` to a new version section.
2. After it is merged, tag the merged commit and push the tag:

   ```bash
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

CI checks that the tag matches the version the binary reports, then publishes static x86_64 and
aarch64 binaries with sha256 checksums as a GitHub release. `installation.sh` and
`cvenv self-update` install from these releases.
