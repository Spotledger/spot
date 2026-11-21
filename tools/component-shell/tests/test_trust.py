"""Trust material: the root-signed anchor set, the revocation list, and what a build accepts.

The bytes are the test's own: every root, publisher and revocation fixture is generated here (no key
material is committed). Two tests pin the encoder to the Rust crate's output — the certificate and
revocation-list documents the vector file carries hex signed bytes and signatures for — so the file
forms this tool parses are the ones ``spot-bundle-signing`` writes, not a second opinion of them.
"""

from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import unittest
from dataclasses import replace
from pathlib import Path
from typing import Any

import support
from component_shell import ed25519, trust

TOOL = support.REPOSITORY_ROOT / "tools" / "component-shell" / "build-shell.py"


def run_tool(*arguments: str) -> subprocess.CompletedProcess[str]:
    """Runs the CLI the way a build would, from the repository root."""
    return subprocess.run(
        [sys.executable, str(TOOL), *arguments],
        cwd=support.REPOSITORY_ROOT,
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )


def certificate_from_vectors(vectors: dict[str, Any]) -> trust.Certificate:
    """The vector file's certificate, as this tool's ``Certificate``."""
    body = vectors["certificate"]["body"]
    return trust.Certificate(
        publisher_id=body["publisher_id"],
        display_name=body["display_name"],
        public_key=base64.b64decode(body["public_key_b64"]),
        valid_from=body["valid_from"],
        valid_to=body["valid_to"],
        issued_by=body["issued_by"],
        serial=body["serial"],
        signature=base64.b64decode(vectors["certificate"]["signature_b64"]),
    )


def revocation_from_vectors(vectors: dict[str, Any]) -> trust.RevocationList:
    """The vector file's revocation list, as this tool's ``RevocationList``."""
    body = vectors["revocation"]["body"]
    return trust.RevocationList(
        generation=body["generation"],
        issued_at=body["issued_at"],
        issued_by=body["issued_by"],
        keys=tuple(trust.RevokedKey(**entry) for entry in body["keys"]),
        publishers=tuple(trust.RevokedPublisher(**entry) for entry in body["publishers"]),
        packages=tuple(trust.RevokedPackage(**entry) for entry in body["bundles"]),
        signature=base64.b64decode(vectors["revocation"]["signature_b64"]),
        sha256="",
    )


def certificate_file_form(vectors: dict[str, Any]) -> dict[str, Any]:
    """The vector certificate as the JSON document an anchor set carries."""
    body = vectors["certificate"]["body"]
    return {
        "display_name": body["display_name"],
        "format": "spot-publisher-certificate",
        "issued_by": body["issued_by"],
        "public_key": body["public_key_b64"],
        "publisher_id": body["publisher_id"],
        "serial": body["serial"],
        "signature": vectors["certificate"]["signature_b64"],
        "valid_from": body["valid_from"],
        "valid_to": body["valid_to"],
        "version": 1,
    }


class DocumentEncodingTests(unittest.TestCase):
    """The two documents the store already signs: this tool's bytes must be the crate's bytes."""

    def test_the_certificate_bytes_are_the_rust_crates(self) -> None:
        vectors = support.load_vectors()
        certificate = certificate_from_vectors(vectors)

        self.assertEqual(vectors["certificate"]["signed_bytes_hex"], certificate.signed_bytes().hex())
        root = base64.b64decode(vectors["keys"]["root"]["public_key_b64"])
        self.assertTrue(ed25519.verify(root, certificate.signed_bytes(), certificate.signature))
        self.assertEqual(vectors["keys"]["publisher"]["key_id"], certificate.key_id)

    def test_the_revocation_list_bytes_are_the_rust_crates(self) -> None:
        vectors = support.load_vectors()
        revocations = revocation_from_vectors(vectors)

        self.assertEqual(vectors["revocation"]["signed_bytes_hex"], revocations.signed_bytes().hex())
        root = base64.b64decode(vectors["keys"]["root"]["public_key_b64"])
        self.assertTrue(ed25519.verify(root, revocations.signed_bytes(), revocations.signature))
        self.assertEqual(["519cae93e93ad011d785355894405ed8"], [entry.key_id for entry in revocations.keys])

    def test_a_verified_anchor_set_carries_the_crates_bytes_into_the_build(self) -> None:
        """An anchor set the store's own root signs, with the crate's certificate, verifies here."""
        with support.TempDirectory() as root:
            vectors = support.load_vectors()
            root_seed = bytes.fromhex(vectors["keys"]["root"]["seed_hex"])
            anchor_set = support.anchor_set_document(
                root_seed=root_seed, certificates=[certificate_file_form(vectors)], issued_at=support.AS_OF
            )
            root_key = trust.parse_root_key(support.root_key_document(root_seed).encode("utf-8"))
            verified = trust.parse_anchor_set(anchor_set.encode("utf-8"), root_key)

            self.assertEqual(1, len(verified.certificates))
            self.assertEqual(vectors["keys"]["publisher"]["key_id"], verified.certificates[0].key_id)

            revocations = trust.parse_revocations(
                support.revocation_document(
                    root_seed=root_seed,
                    keys=[support.revoked_key(bytes.fromhex(vectors["keys"]["revoked"]["seed_hex"]))],
                ).encode("utf-8"),
                verified,
                support.AS_OF,
            )
            self.assertEqual(1, len(revocations.keys))
            del root


class AnchorSetRefusalTests(unittest.TestCase):
    """What a build refuses about the material itself."""

    def assert_refused(self, code: str, call, *args, **kwargs) -> None:  # noqa: ANN001 - unittest style
        from component_shell.package import Refused

        with self.assertRaises(Refused) as caught:
            call(*args, **kwargs)
        self.assertEqual(code, caught.exception.code, str(caught.exception))

    def test_an_anchor_set_the_root_did_not_sign_is_refused(self) -> None:
        signer, other = support.generated_seed(), support.generated_seed()
        document = support.anchor_set_document(
            root_seed=signer,
            certificates=[support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=signer)],
        )
        root_key = trust.parse_root_key(support.root_key_document(other).encode("utf-8"))

        self.assert_refused(trust.ANCHOR_SET_SIGNATURE_INVALID, trust.parse_anchor_set, document.encode("utf-8"), root_key)

    def test_an_anchor_set_that_does_not_name_its_signing_root_is_refused(self) -> None:
        signer, other = support.generated_seed(), support.generated_seed()
        document = support.anchor_set_document(
            root_seed=signer,
            certificates=[support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=signer)],
            roots=[support.root_entry(other)],
        )
        root_key = trust.parse_root_key(support.root_key_document(signer).encode("utf-8"))

        self.assert_refused(trust.UNKNOWN_TRUST_ROOT, trust.parse_anchor_set, document.encode("utf-8"), root_key)

    def test_a_certificate_issued_by_a_root_outside_the_anchor_set_is_refused(self) -> None:
        signer, stranger = support.generated_seed(), support.generated_seed()
        document = support.anchor_set_document(
            root_seed=signer,
            certificates=[
                support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=stranger)
            ],
        )
        root_key = trust.parse_root_key(support.root_key_document(signer).encode("utf-8"))

        self.assert_refused(trust.UNKNOWN_ANCHOR, trust.parse_anchor_set, document.encode("utf-8"), root_key)

    def test_a_certificate_whose_body_was_edited_is_refused(self) -> None:
        signer = support.generated_seed()
        certificate = support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=signer)
        document = support.anchor_set_document(root_seed=signer, certificates=[certificate])
        root_key = trust.parse_root_key(support.root_key_document(signer).encode("utf-8"))

        # 1. The anchor set's own signature covers every certificate's identity.
        edited = json.loads(document)
        edited["certificates"][0]["display_name"] = "Acme, but someone edited the file"
        self.assert_refused(
            trust.ANCHOR_SET_SIGNATURE_INVALID, trust.parse_anchor_set, json.dumps(edited).encode("utf-8"), root_key
        )

        # 2. Even a re-signed anchor set does not make an unsigned certificate valid.
        re_signed = support.anchor_set_document(root_seed=signer, certificates=[edited["certificates"][0]])
        self.assert_refused(
            trust.CERTIFICATE_SIGNATURE_INVALID, trust.parse_anchor_set, re_signed.encode("utf-8"), root_key
        )

    def test_a_revocation_list_signed_by_another_root_is_refused(self) -> None:
        signer, stranger = support.generated_seed(), support.generated_seed()
        verified = trust.parse_anchor_set(
            support.anchor_set_document(
                root_seed=signer,
                certificates=[support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=signer)],
            ).encode("utf-8"),
            trust.parse_root_key(support.root_key_document(signer).encode("utf-8")),
        )
        document = support.revocation_document(root_seed=stranger)

        self.assert_refused(trust.UNKNOWN_TRUST_ROOT, trust.parse_revocations, document.encode("utf-8"), verified, support.AS_OF)

    def test_a_tampered_revocation_list_is_refused(self) -> None:
        signer = support.generated_seed()
        verified = trust.parse_anchor_set(
            support.anchor_set_document(
                root_seed=signer,
                certificates=[support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=signer)],
            ).encode("utf-8"),
            trust.parse_root_key(support.root_key_document(signer).encode("utf-8")),
        )
        document = json.loads(support.revocation_document(root_seed=signer))
        document["generation"] = document["generation"] + 1

        self.assert_refused(
            trust.REVOCATION_SIGNATURE_INVALID,
            trust.parse_revocations,
            json.dumps(document).encode("utf-8"),
            verified,
            support.AS_OF,
        )

    def test_a_revocation_list_signed_by_a_retired_root_is_refused(self) -> None:
        signer = support.generated_seed()
        verified = trust.parse_anchor_set(
            support.anchor_set_document(
                root_seed=signer,
                certificates=[support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=signer)],
                roots=[support.root_entry(signer, valid_from=0, valid_to=support.AS_OF)],
            ).encode("utf-8"),
            trust.parse_root_key(support.root_key_document(signer).encode("utf-8")),
        )

        self.assert_refused(
            trust.ROOT_OUTSIDE_WINDOW,
            trust.parse_revocations,
            support.revocation_document(root_seed=signer).encode("utf-8"),
            verified,
            support.AS_OF,
        )

    def test_a_document_with_a_member_nobody_defined_is_refused(self) -> None:
        signer = support.generated_seed()
        document = json.loads(
            support.anchor_set_document(
                root_seed=signer,
                certificates=[support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=signer)],
            )
        )
        document["extra"] = "a build host added this"
        root_key = trust.parse_root_key(support.root_key_document(signer).encode("utf-8"))

        self.assert_refused(trust.ANCHOR_SET_MALFORMED, trust.parse_anchor_set, json.dumps(document).encode("utf-8"), root_key)

    def test_a_root_key_file_that_names_another_key_is_refused(self) -> None:
        signer, other = support.generated_seed(), support.generated_seed()
        document = json.loads(support.root_key_document(signer))
        document["key_id"] = ed25519.key_id(ed25519.public_key(other))

        self.assert_refused(trust.ROOT_KEY_MALFORMED, trust.parse_root_key, json.dumps(document).encode("utf-8"))

    def test_a_reviewed_sets_line_that_is_not_a_digest_is_refused(self) -> None:
        self.assert_refused(trust.REVIEWED_LIST_MALFORMED, trust.parse_reviewed_list, "# note\nnot-a-digest\n")

    def test_an_as_of_instant_without_an_offset_is_refused(self) -> None:
        self.assert_refused("component.trust.instant_invalid", trust.parse_instant, "2026-10-26T00:00:00")
        self.assertEqual(1_760_000_000, trust.parse_instant("2025-10-09T08:53:20Z"))
        self.assertEqual(1_760_000_000, trust.parse_instant("1760000000"))
        # A non-ASCII digit is not UTC seconds: a refusal, never a traceback.
        self.assert_refused("component.trust.instant_invalid", trust.parse_instant, "\u00b2")


class SignedBuildTests(unittest.TestCase):
    """The default mode: a build whose trust comes only from the store's signed material."""

    def test_a_certificate_chain_that_verifies_builds_and_is_recorded(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            built = support.build_package(package)
            assert built is not None
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            output = root / "publish"
            files = support.write_trust(root / "trust")

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertIn("signed trust material", completed.stdout)
            record_path = next(output.glob("component-trust-*.json"))
            record = trust.parse_record(record_path.read_text(encoding="utf-8"))
            self.assertEqual("signed", record["mode"])
            self.assertEqual(support.AS_OF, record["as_of"])
            self.assertEqual(
                [{"component_id": "acme.rating", "component_version": "1.0.0", "manifest_sha256": built.manifest.digest()}],
                record["components"],
            )
            material = files.material()
            self.assertEqual(material.digest, record["trust_sha256"])
            self.assertEqual(material.anchor_set.sha256, record["trust_material"]["anchor_set_sha256"])
            self.assertEqual(material.revocations.sha256, record["trust_material"]["revocations_sha256"])
            self.assertEqual(record_path.name, "component-trust-" + support.hash_bytes(record_path.read_bytes())[:16] + ".json")

    def test_the_same_material_records_the_same_digest_and_a_new_list_does_not(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            files = support.write_trust(root / "trust")

            first = root / "first"
            second = root / "second"
            changed = root / "changed"
            self.assertEqual(0, run_tool(*files.arguments(), "--components", str(set_file), "--output", str(first)).returncode)
            self.assertEqual(0, run_tool(*files.arguments(), "--components", str(set_file), "--output", str(second)).returncode)

            newer = support.write_trust(
                root / "trust-newer",
                publisher_seed=support.PUBLISHER_SEED,
            )
            self.assertEqual(0, run_tool(*newer.arguments(), "--components", str(set_file), "--output", str(changed)).returncode)

            def digest_of(directory: Path) -> str:
                record = json.loads(next(directory.glob("component-trust-*.json")).read_text(encoding="utf-8"))
                return record["trust_sha256"]

            self.assertEqual(digest_of(first), digest_of(second))
            self.assertNotEqual(digest_of(first), digest_of(changed))

    def test_a_key_revoked_as_of_the_build_instant_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                revocations=support.revocation_document(
                    root_seed=root_seed, keys=[support.revoked_key(support.PUBLISHER_SEED, effective_from=support.AS_OF)]
                ),
            )
            output = root / "publish"

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.key.revoked", completed.stderr)
            self.assertFalse(output.exists())

    def test_a_key_revoked_after_the_build_instant_is_accepted(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                revocations=support.revocation_document(
                    root_seed=root_seed,
                    keys=[support.revoked_key(support.PUBLISHER_SEED, effective_from=support.AS_OF + 1)],
                ),
            )

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(0, completed.returncode, completed.stderr)

    def test_a_compromised_key_is_refused_whatever_the_date(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                revocations=support.revocation_document(
                    root_seed=root_seed,
                    keys=[
                        support.revoked_key(
                            support.PUBLISHER_SEED, effective_from=support.AS_OF + 10_000_000, compromised=True
                        )
                    ],
                ),
            )

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.key.compromised", completed.stderr)

    def test_a_package_whose_digest_is_revoked_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            built = support.build_package(package)
            assert built is not None
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                revocations=support.revocation_document(
                    root_seed=root_seed, packages=[support.revoked_package(built.manifest.digest())]
                ),
            )

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.package.revoked", completed.stderr)

    def test_a_certificate_issued_by_a_retired_root_is_refused(self) -> None:
        """The certificate chains to a root, but that root's window had closed by --as-of."""
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            retired, current = support.generated_seed(), support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=current,
                roots=[
                    support.root_entry(retired, valid_from=0, valid_to=support.AS_OF),
                    support.root_entry(current, valid_from=0, valid_to=None),
                ],
                certificates=[
                    support.certificate_document(publisher_seed=support.PUBLISHER_SEED, root_seed=retired)
                ],
                revocations=support.revocation_document(root_seed=current),
            )

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.trust.root_outside_window", completed.stderr)

    def test_a_certificate_that_expired_before_the_build_instant_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                certificates=[
                    support.certificate_document(
                        publisher_seed=support.PUBLISHER_SEED, root_seed=root_seed, valid_to=support.AS_OF
                    )
                ],
            )
            output = root / "publish"

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(output))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.certificate.expired", completed.stderr)
            self.assertFalse(output.exists())

    def test_a_certificate_that_is_not_yet_valid_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                certificates=[
                    support.certificate_document(
                        publisher_seed=support.PUBLISHER_SEED,
                        root_seed=root_seed,
                        valid_from=support.AS_OF + 1,
                        valid_to=support.AS_OF + 1000,
                    )
                ],
            )

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.certificate.not_yet_valid", completed.stderr)

    def test_a_certificate_for_another_publisher_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            root_seed = support.generated_seed()
            files = support.write_trust(
                root / "trust",
                root_seed=root_seed,
                certificates=[
                    support.certificate_document(
                        publisher_seed=support.PUBLISHER_SEED, root_seed=root_seed, publisher_id="someoneelse"
                    )
                ],
            )

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.publisher.mismatch", completed.stderr)

    def test_a_signature_that_does_not_verify_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package, signature=b"\x01" * 64)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            files = support.write_trust(root / "trust")

            completed = run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.signature.invalid", completed.stderr)

    def test_the_as_of_instant_may_be_an_iso_8601_instant(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            files = support.write_trust(root / "trust")
            arguments = files.arguments()
            arguments[arguments.index("--as-of") + 1] = "2025-10-09T08:53:20Z"  # the same instant as support.AS_OF

            completed = run_tool(*arguments, "--components", str(set_file), "--output", str(root / "publish"))

            self.assertEqual(0, completed.returncode, completed.stderr)
            record = json.loads(next((root / "publish").glob("component-trust-*.json")).read_text(encoding="utf-8"))
            self.assertEqual(support.AS_OF, record["as_of"])

    def test_the_build_needs_an_explicit_as_of_instant(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            files = support.write_trust(root / "trust")
            arguments = [value for value in files.arguments() if value not in ("--as-of", str(support.AS_OF))]

            completed = run_tool(*arguments, "--components", str(set_file))

            self.assertEqual(2, completed.returncode)
            self.assertIn("--as-of", completed.stderr)

    def test_the_two_modes_cannot_be_mixed(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package])
            files = support.write_trust(root / "trust")

            completed = run_tool(*files.arguments(), "--reviewed-set", "--components", str(set_file))

            self.assertEqual(2, completed.returncode)
            self.assertIn("--reviewed-set", completed.stderr)

    def test_a_trust_file_that_is_not_there_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
            files = support.write_trust(root / "trust")
            arguments = files.arguments()
            arguments[1] = str(root / "trust" / "no-such-root-key.json")

            completed = run_tool(*arguments, "--components", str(set_file))

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.trust.unreadable", completed.stderr)


class ReviewedSetTests(unittest.TestCase):
    """The fallback: a set file whose digest a reviewer recorded, trusted for what it names."""

    def test_a_recorded_reviewed_set_builds_with_a_loud_notice(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package])
            reviewed = support.write_reviewed_list(root / "reviewed-sets.txt", [support.set_digest(set_file)])
            output = root / "publish"

            completed = run_tool(
                "--components", str(set_file), "--output", str(output), "--reviewed-set", str(reviewed)
            )

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertIn("WARNING: reviewed-set mode", completed.stderr)
            self.assertIn("a reviewer read them", completed.stderr)
            self.assertIn("REVIEWED SET (unverified)", completed.stdout)
            record = json.loads(next(output.glob("component-trust-*.json")).read_text(encoding="utf-8"))
            self.assertEqual("reviewed-set", record["mode"])
            self.assertEqual(support.set_digest(set_file), record["trust_sha256"])
            self.assertEqual(reviewed.name, record["trust_material"]["reviewed_list"])

    def test_a_set_file_whose_digest_is_not_recorded_is_refused(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package])
            reviewed = support.write_reviewed_list(root / "reviewed-sets.txt", ["0" * 64])
            output = root / "publish"

            completed = run_tool(
                "--components", str(set_file), "--output", str(output), "--reviewed-set", str(reviewed)
            )

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.trust.reviewed_set_not_recorded", completed.stderr)
            self.assertFalse(output.exists())

    def test_a_reviewed_set_still_checks_the_keys_it_names(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package, seed=support.OUTSIDER_SEED)
            set_file = support.write_set_file(root / "set.toml", packages=[package])
            reviewed = support.write_reviewed_list(root / "reviewed-sets.txt", [support.set_digest(set_file)])

            completed = run_tool(
                "--components", str(set_file), "--output", str(root / "publish"), "--reviewed-set", str(reviewed)
            )

            self.assertEqual(1, completed.returncode)
            self.assertIn("component.signature.unknown_key", completed.stderr)

    def test_the_shipped_reviewed_sets_file_records_no_set(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package])

            completed = run_tool("--components", str(set_file), "--output", str(root / "publish"), "--reviewed-set")

            self.assertEqual(1, completed.returncode)
            self.assertIn("reviewed-sets.txt", completed.stderr)
            self.assertIn("component.trust.reviewed_set_not_recorded", completed.stderr)



class GenerationPinTests(unittest.TestCase):
    """a recorded security finding follow-up: a pinned minimum generation refuses a replay of an older signed document."""

    def _prepare(self, root: Path, **trust_arguments: int) -> tuple[Path, "support.TrustFiles"]:
        package = root / "acme.rating-1.0.0"
        support.build_package(package)
        set_file = support.write_set_file(root / "set.toml", packages=[package], trust_table=False)
        return set_file, support.write_trust(root / "trust", **trust_arguments)

    def _build(self, root: Path, set_file: Path, files: "support.TrustFiles", **pins: int) -> subprocess.CompletedProcess[str]:
        files = replace(files, **pins)
        return run_tool(*files.arguments(), "--components", str(set_file), "--output", str(root / "publish"))

    def test_a_revocation_list_at_the_pin_is_accepted_and_both_numbers_are_recorded(self) -> None:
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, revocation_generation=7, anchors_generation=4)

            completed = self._build(root, set_file, files, min_revocation_generation=7, min_anchors_generation=4)

            self.assertEqual(0, completed.returncode, completed.stderr)
            record = trust.parse_record(next((root / "publish").glob("component-trust-*.json")).read_text(encoding="utf-8"))
            material = record["trust_material"]
            self.assertEqual(7, material["revocation_generation"])
            self.assertEqual(7, material["min_revocation_generation"])
            self.assertEqual(4, material["anchor_set_generation"])
            self.assertEqual(4, material["min_anchors_generation"])

    def test_a_revocation_list_below_the_pin_is_refused_naming_both_numbers(self) -> None:
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, revocation_generation=6)

            completed = self._build(root, set_file, files, min_revocation_generation=7)

            self.assertEqual(1, completed.returncode)
            self.assertIn(trust.REVOCATION_STALE, completed.stderr)
            self.assertIn("the revocation list has generation 6, below the pinned minimum 7", completed.stderr)
            self.assertFalse((root / "publish").exists(), "a refused build writes nothing")

    def test_an_anchor_set_below_the_pin_is_refused_naming_both_numbers(self) -> None:
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, anchors_generation=2)

            completed = self._build(root, set_file, files, min_anchors_generation=5)

            self.assertEqual(1, completed.returncode)
            self.assertIn(trust.ANCHORS_STALE, completed.stderr)
            self.assertIn("the anchor set has generation 2, below the pinned minimum 5", completed.stderr)
            self.assertFalse((root / "publish").exists())

    def test_a_pin_of_zero_is_a_pin_and_accepts_any_generation(self) -> None:
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, revocation_generation=0, anchors_generation=0)

            completed = self._build(root, set_file, files, min_revocation_generation=0, min_anchors_generation=0)

            self.assertEqual(0, completed.returncode, completed.stderr)

    def test_the_build_line_names_the_generations_and_the_pins(self) -> None:
        """The build's own log line says which documents it used and what they had to clear."""
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, revocation_generation=7, anchors_generation=4)

            completed = self._build(root, set_file, files, min_revocation_generation=7, min_anchors_generation=4)

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertIn("anchors generation 4 (pinned >= 4)", completed.stdout)
            self.assertIn("revocation generation 7 (pinned >= 7)", completed.stdout)

    def test_a_document_newer_than_the_pin_is_accepted(self) -> None:
        """A pin is a floor, not an equality: the newest signed document has to build."""
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, revocation_generation=9, anchors_generation=9)

            completed = self._build(root, set_file, files, min_revocation_generation=2, min_anchors_generation=2)

            self.assertEqual(0, completed.returncode, completed.stderr)
            record = trust.parse_record(
                next((root / "publish").glob("component-trust-*.json")).read_text(encoding="utf-8")
            )
            self.assertEqual(9, record["trust_material"]["revocation_generation"])
            self.assertEqual(9, record["trust_material"]["anchor_set_generation"])
            self.assertEqual(2, record["trust_material"]["min_revocation_generation"])
            self.assertEqual(2, record["trust_material"]["min_anchors_generation"])

    def test_the_pin_is_bound_into_the_record_the_build_writes(self) -> None:
        """One material under two pins must not produce one audit record: the file name is its digest."""
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, revocation_generation=9, anchors_generation=9)

            low = self._build(root, set_file, files, min_revocation_generation=7, min_anchors_generation=4)
            self.assertEqual(0, low.returncode, low.stderr)
            low_record = next((root / "publish").glob("component-trust-*.json"))
            low_name, low_text = low_record.name, low_record.read_text(encoding="utf-8")
            (root / "publish").rename(root / "first")

            high = self._build(root, set_file, files, min_revocation_generation=9, min_anchors_generation=9)
            self.assertEqual(0, high.returncode, high.stderr)
            high_record = next((root / "publish").glob("component-trust-*.json"))
            high_text = high_record.read_text(encoding="utf-8")

            self.assertNotEqual(low_name, high_record.name, "a different pin is a different audit record")
            self.assertNotEqual(low_text, high_text)
            low_material = trust.parse_record(low_text)["trust_material"]
            high_material = trust.parse_record(high_text)["trust_material"]
            self.assertEqual((7, 4), (low_material["min_revocation_generation"], low_material["min_anchors_generation"]))
            self.assertEqual((9, 9), (high_material["min_revocation_generation"], high_material["min_anchors_generation"]))

    def test_a_pin_written_into_the_set_file_is_refused_not_read(self) -> None:
        """The pin comes from the command line; a number in a file the attacker writes is no pin."""
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set-bare.toml", packages=[package], trust_table=False)
            files = support.write_trust(root / "trust")

            for line in ("min_revocation_generation = 1", "min_anchors_generation = 1"):
                with self.subTest(line=line):
                    pinned = root / "set-pinned.toml"
                    pinned.write_text(
                        set_file.read_text(encoding="utf-8").replace("[shell]\n", f"[shell]\n{line}\n", 1),
                        encoding="utf-8",
                    )

                    completed = self._build(root, pinned, files, min_revocation_generation=5, min_anchors_generation=5)

                    self.assertEqual(1, completed.returncode)
                    self.assertIn(line.split(" ")[0], completed.stderr)
                    self.assertIn("--min-revocation-generation", completed.stderr)
                    self.assertFalse((root / "publish").exists())

    def test_a_pin_in_a_reviewed_set_trust_table_is_refused_too(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            bare = support.write_set_file(root / "set-bare.toml", packages=[package])
            pinned = root / "set-pinned.toml"
            pinned.write_text(
                bare.read_text(encoding="utf-8").replace("[trust]\n", "[trust]\nmin_revocation_generation = 1\n", 1),
                encoding="utf-8",
            )
            reviewed = support.write_reviewed_list(root / "reviewed-sets.txt", [support.set_digest(pinned)])

            completed = run_tool(
                "--components", str(pinned), "--output", str(root / "publish"), "--reviewed-set", str(reviewed)
            )

            self.assertEqual(1, completed.returncode)
            self.assertIn("min_revocation_generation", completed.stderr)
            self.assertFalse((root / "publish").exists())

    def test_a_missing_pin_is_a_usage_error_not_a_default(self) -> None:
        for pin in ("--min-revocation-generation", "--min-anchors-generation"):
            with self.subTest(pin=pin), support.TempDirectory() as root:
                set_file, files = self._prepare(root)
                arguments = files.arguments()
                index = arguments.index(pin)
                del arguments[index : index + 2]

                completed = run_tool(*arguments, "--components", str(set_file), "--output", str(root / "publish"))

                self.assertEqual(2, completed.returncode)
                self.assertIn(pin, completed.stderr)
                self.assertFalse((root / "publish").exists())

    def test_the_pin_is_not_read_from_the_environment(self) -> None:
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root)
            arguments = files.arguments()
            for pin in ("--min-revocation-generation", "--min-anchors-generation"):
                index = arguments.index(pin)
                del arguments[index : index + 2]
            environment = {
                **os.environ,
                "MIN_REVOCATION_GENERATION": "0",
                "MIN_ANCHORS_GENERATION": "0",
                "SPOT_MIN_REVOCATION_GENERATION": "0",
            }

            completed = subprocess.run(
                [sys.executable, str(TOOL), *arguments, "--components", str(set_file)],
                cwd=support.REPOSITORY_ROOT,
                capture_output=True,
                text=True,
                env=environment,
                timeout=300,
                check=False,
            )

            self.assertEqual(2, completed.returncode)

    def test_a_pin_that_is_not_a_non_negative_integer_is_a_usage_error(self) -> None:
        for value in ("-1", "abc", "1.5", "", "+3", " 2"):
            with self.subTest(value=value), support.TempDirectory() as root:
                set_file, files = self._prepare(root)
                arguments = files.arguments()
                arguments[arguments.index("--min-revocation-generation") + 1] = value

                completed = run_tool(*arguments, "--components", str(set_file), "--output", str(root / "publish"))

                self.assertEqual(2, completed.returncode)
                self.assertFalse((root / "publish").exists())

    def test_a_tampered_generation_fails_the_signature_before_the_pin_is_judged(self) -> None:
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, revocation_generation=1)
            document = json.loads(files.revocations.read_text(encoding="utf-8"))
            document["generation"] = 99  # raised to clear a high pin, without re-signing
            files.revocations.write_text(json.dumps(document), encoding="utf-8")

            completed = self._build(root, set_file, files, min_revocation_generation=50)

            self.assertEqual(1, completed.returncode)
            self.assertIn(trust.REVOCATION_SIGNATURE_INVALID, completed.stderr)
            self.assertNotIn(trust.REVOCATION_STALE, completed.stderr)

    def test_a_tampered_anchor_generation_fails_the_signature_before_the_pin_is_judged(self) -> None:
        with support.TempDirectory() as root:
            set_file, files = self._prepare(root, anchors_generation=1)
            document = json.loads(files.anchor_set.read_text(encoding="utf-8"))
            document["generation"] = 99
            files.anchor_set.write_text(json.dumps(document), encoding="utf-8")

            completed = self._build(root, set_file, files, min_anchors_generation=50)

            self.assertEqual(1, completed.returncode)
            self.assertIn(trust.ANCHOR_SET_SIGNATURE_INVALID, completed.stderr)
            self.assertNotIn(trust.ANCHORS_STALE, completed.stderr)

    def test_the_library_entry_point_has_no_default_pin(self) -> None:
        with support.TempDirectory() as root:
            _set_file, files = self._prepare(root)
            with self.assertRaises(TypeError):
                trust.load_trust_material(  # type: ignore[call-arg]
                    anchor_set=files.anchor_set.read_bytes(),
                    revocations=files.revocations.read_bytes(),
                    root_key=files.root_key.read_bytes(),
                    as_of=files.as_of,
                )

    def test_the_library_refuses_a_pin_that_is_not_a_non_negative_integer(self) -> None:
        with support.TempDirectory() as root:
            _set_file, files = self._prepare(root)
            for bad in (-1, True, "1", None):
                with self.subTest(bad=bad), self.assertRaises(ValueError):
                    trust.load_trust_material(
                        anchor_set=files.anchor_set.read_bytes(),
                        revocations=files.revocations.read_bytes(),
                        root_key=files.root_key.read_bytes(),
                        as_of=files.as_of,
                        min_anchors_generation=bad,  # type: ignore[arg-type]
                        min_revocation_generation=0,
                    )

    def test_reviewed_set_mode_neither_requires_nor_honours_the_pins(self) -> None:
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package])
            reviewed = support.write_reviewed_list(root / "reviewed-sets.txt", [support.set_digest(set_file)])

            without = run_tool(
                "--components", str(set_file), "--output", str(root / "publish"), "--reviewed-set", str(reviewed)
            )
            self.assertEqual(0, without.returncode, without.stderr)

            for pin in ("--min-revocation-generation", "--min-anchors-generation"):
                with self.subTest(pin=pin):
                    completed = run_tool(
                        "--components", str(set_file), "--output", str(root / "other"),
                        "--reviewed-set", str(reviewed), pin, "0",
                    )
                    self.assertEqual(2, completed.returncode)
                    self.assertIn("--reviewed-set", completed.stderr)
                    self.assertFalse((root / "other").exists())

    def test_a_reviewed_set_record_carries_no_generation_and_no_pin(self) -> None:
        """The fallback build is unchanged: a reviewed digest, no signed list, so nothing to pin."""
        with support.TempDirectory() as root:
            package = root / "acme.rating-1.0.0"
            support.build_package(package)
            set_file = support.write_set_file(root / "set.toml", packages=[package])
            reviewed = support.write_reviewed_list(root / "reviewed-sets.txt", [support.set_digest(set_file)])

            completed = run_tool(
                "--components", str(set_file), "--output", str(root / "publish"), "--reviewed-set", str(reviewed)
            )

            self.assertEqual(0, completed.returncode, completed.stderr)
            record = trust.parse_record(
                next((root / "publish").glob("component-trust-*.json")).read_text(encoding="utf-8")
            )
            self.assertEqual("reviewed-set", record["mode"])
            self.assertEqual(
                {"reviewed_list", "reviewed_list_sha256", "reviewed_set_sha256"},
                set(record["trust_material"]),
            )


if __name__ == "__main__":
    unittest.main()
