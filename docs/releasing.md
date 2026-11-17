# Releases

Spotledger Runtime is pre-release software. This page says what a release is, how to pin to one, how to check a download and what is not protected.

## What a release is

A release is an **annotated git tag** (`vX.Y.Z`, or `vX.Y.Z-rc.N` for a release candidate) on a commit of `main`. A maintainer starts the release run by hand in the Actions tab. Nothing releases automatically: there is no tag-push trigger and no schedule.

The run refuses unless the tag exists and is annotated, its commit is on `main`, the CI workflow is green on that commit, the tag equals the version in `Cargo.toml` and the licence check passes. It then builds and checks the packages and creates a GitHub Release with:

- `spot-<version>-linux-<arch>.tar.gz`, the tarball;
- the `.deb` and the `.rpm` package;
- `spot-<version>.cdx.json`, a CycloneDX software bill of materials (SBOM);
- `SHA256SUMS`, the checksum of every file above.

No container image is published. Release candidates are marked as pre-releases on GitHub.

**Version `0.x` means pre-release.** The software is in active development, is not ready to run a live public service and has had no independent security audit.

**The head of `main` is a development build.** It is whatever the latest export produced. It has had CI and nothing else: it is not packaged, not checksummed and has no SBOM. Only a tagged release has been through the release run.

## Pinning

Do not track `main`. Pin to a release tag, or to a full commit SHA for a source build.

A release tag:

```sh
git clone --branch vX.Y.Z --depth 1 https://github.com/Spotledger/spot.git
```

A full commit SHA (all 40 characters; a short SHA is not enough):

```sh
git clone https://github.com/Spotledger/spot.git
cd spot
git checkout <full sha>
```

For packages, download the files of one release and keep that version until you choose to move.

## Verifying a download

Put every file of the release in one folder and run, from that folder:

```sh
sha256sum -c SHA256SUMS
```

Every line must say `OK`. Next, the script `tools/release/verify_download.py` checks the whole folder: the checksums, that no file is missing or unlisted, that the tarball, `.deb`, `.rpm` and SBOM are present and carry the version, and that the SBOM is a CycloneDX document. Take the script from the same tag as the release:

```sh
git show vX.Y.Z:tools/release/verify_download.py > verify_download.py
python3 verify_download.py DIR --tag vX.Y.Z
```

It exits 0 when everything passes and 1 when it lists a finding. Run `python3 verify_download.py --help` for the options.

To check that a tag points at the commit you expect:

```sh
git rev-parse vX.Y.Z^{commit}
```

Compare the output with the source commit stated in the release notes.

To inspect the SBOM, open `spot-<version>.cdx.json` with any JSON viewer or CycloneDX tool. It lists the components the build used.

**Releases are unsigned development versions.** The checksums prove that your download matches what the release run built. They do not prove who built it. Signing may be added later.

## What is not signed

Nothing is signed in 0.x. Tags are annotated, not signed. The packages and the tarball carry checksums only. There are no build attestations.

## Cutting a release (maintainers)

1. Set the release version in `Cargo.toml` (and `Cargo.lock`), and let that change reach `main`. The tag and the version must be equal.
2. Wait for CI to be green on the `main` commit you want to release.
3. **Dry run.** Start the `release` workflow with `tag` set to the new tag and `publish` left off (Actions tab, or `gh workflow run release.yml -f tag=vX.Y.Z -f publish=false`). It runs the checks that need no tag, builds and checks the packages, and keeps them as a workflow artefact for 14 days. It creates no tag and no release.
4. **Tag.** In a fresh clone, create an annotated tag on the commit and push it. Pushing a tag starts nothing.

   ```sh
   git tag -a vX.Y.Z -m "Spotledger Runtime vX.Y.Z" <full sha>
   git push origin vX.Y.Z
   ```

5. **Publish.** Start the workflow again with the same tag and `publish` on (`gh workflow run release.yml -f tag=vX.Y.Z -f publish=true`). The run refuses a tag that already has a release, and a tag whose commit moved during the run. Release files are never overwritten.
6. Verify the published files as described above before telling anyone.

## If a release is bad

A bad release is superseded by the next version (`rc.2`, `0.1.1`), never edited. Tags are not moved, and the release files are not replaced. Its release notes may point to the version that replaces it.

## Reporting

Report a suspected vulnerability privately; see [SECURITY.md](../SECURITY.md).
