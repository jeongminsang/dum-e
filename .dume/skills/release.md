---
name: release
description: Prepare, verify, publish, and recover native Rust DUM-E releases.
---

# Releasing DUM-E

The product release pipeline is native Rust. Do not use the retained TypeScript package release/publish scripts to release the Rust executable. Do not commit, tag, push, or publish without explicit user authorization.

## Prepare and verify

1. Set the intended version in `[workspace.package]` in `Cargo.toml` and update `Cargo.lock` with Cargo. All Rust crates inherit this version. Preserve the root license assets.
2. Run `cargo check --workspace --locked` and `cargo test --workspace --locked`.
3. Build a local native release with `bash scripts/build-binaries.sh --out /tmp/dume-native-release`. The destination must not contain an existing archive with the same name. Add `--offline` only with an already hydrated Cargo cache.
4. The build script tests the workspace, builds the release executable, packages licenses, extracts the archive, and runs `--help` and `--version` outside the checkout. Python 3.11+ and the native Rust toolchain are build tools, not installed-product dependencies.
5. On macOS/Linux, verify the installer using the real archive: `python3 scripts/test-native-install.py --archive /tmp/dume-native-release/dume-<platform>-<arch>.tar.gz --version <version>`.
6. Create the native source archive with `bash scripts/create-source-archive.sh --version <version> --out /tmp/dume-native-release/dume-<version>-source.tar.gz`. It contains Cargo manifests/lockfile, crates and fixtures, native build/install scripts and installer tests, and licenses, not TypeScript packages. Extract into a fresh directory and run locked Cargo tests there.

## Publish

After explicit approval, commit the reviewed changes including `Cargo.lock`, create the matching `v<workspace.version>` tag, and push it. `.github/workflows/release.yml` is the sole tag publisher. It validates the tag/version, calls the reusable native build workflow, and tests six native platform/architecture combinations before publishing checksummed archives.

No npm publishing or announcement service is part of native distribution. Do not claim hosted runner success from local tests alone.

## Recover

Use the release workflow's manual `tag` input for an existing tag. It always builds the exact tagged commit. A published release is immutable: never clobber its assets or move its tag. A matching complete draft may be verified and published by rerunning the publication job. An incomplete or different draft fails closed; inspect it and obtain approval before destructive cleanup. Do not silently replace it.
