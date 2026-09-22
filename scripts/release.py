#!/usr/bin/env python3
"""Build and validate SystemOne (`s1`) binary release archives.

This script intentionally uses only the Python standard library. Python is a
build/verification dependency, not an `s1` runtime dependency.
"""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tarfile
import tempfile

from release_lib import (
    MEMBER_FILES,
    LLAMA_CPP_COMMIT,
    NATIVE_CRATE_VERSION,
    RUST_NOTICE_PATH,
    SOURCE_ARCHIVE_PATHS,
    SOURCE_SHA_RE,
    TARGETS,
    VERSION_RE,
    ReleaseError,
    add_bytes,
    add_directory,
    check_linkage,
    extract_safely,
    fail,
    inspect_members,
    json_bytes,
    run_json_command,
    sha256_path,
)

REPO_ROOT = Path(__file__).resolve().parents[1]


def workspace_version(repo_root):
    cargo_toml = (repo_root / "Cargo.toml").read_text(encoding="utf-8")
    in_workspace_package = False
    for raw_line in cargo_toml.splitlines():
        line = raw_line.strip()
        if line.startswith("["):
            in_workspace_package = line == "[workspace.package]"
            continue
        if in_workspace_package:
            match = re.fullmatch(r'version\s*=\s*"([^"]+)"', line)
            if match:
                version = match.group(1)
                if not VERSION_RE.fullmatch(version):
                    fail("workspace.package version is not a supported release version: %s" % version)
                return version
    fail("could not find workspace.package version in Cargo.toml")


def validate_ref(ref, version, require_tag=False):
    if not isinstance(ref, str) or not ref:
        fail("source ref must be a nonempty string")
    if ref.startswith("refs/tags/"):
        tag = ref[len("refs/tags/") :]
        if tag != "v" + version:
            fail("tag %s does not match workspace version %s" % (tag, version))
        if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", tag):
            fail("invalid release tag: %s" % tag)
        return tag
    if require_tag:
        fail("publication requires refs/tags/v<VERSION>, got %s" % ref)
    return None


def package_archive(args):
    repo_root = Path(args.repo_root).resolve()
    version = workspace_version(repo_root)
    tag = validate_ref(args.source_ref, version)
    if not SOURCE_SHA_RE.fullmatch(args.source_sha):
        fail("source SHA must be 40 lowercase hexadecimal characters")
    if args.target not in TARGETS:
        fail("unsupported release target: %s" % args.target)
    if args.source_date_epoch < 0:
        fail("source date epoch must be nonnegative")

    binary = Path(args.binary).resolve()
    if not binary.is_file():
        fail("release binary is not a regular file: %s" % binary)
    if not os.access(str(binary), os.X_OK):
        fail("release binary is not executable: %s" % binary)
    run_json_command(binary, "--version", "systemone-version-v1", version, repo_root)
    run_json_command(binary, "--help", "systemone-help-v1", cwd=repo_root)

    package_sources = {
        "LICENSE": repo_root / "LICENSE",
        "THIRD_PARTY.md": repo_root / "THIRD_PARTY.md",
        "THIRD_PARTY_LICENSES.html": repo_root / "THIRD_PARTY_LICENSES.html",
        "RUST-COPYRIGHT-library.html": repo_root.joinpath(*RUST_NOTICE_PATH),
        "README.md": repo_root / "docs" / "RELEASE.md",
    }
    package_sources.update(
        {
            destination: repo_root.joinpath(*relative_path)
            for destination, relative_path in SOURCE_ARCHIVE_PATHS.items()
        }
    )
    for destination, source in package_sources.items():
        if not source.is_file():
            fail("missing package input for %s: %s" % (destination, source))

    root_name = "s1-v%s-%s" % (version, args.target)
    archive_name = root_name + ".tar.gz"
    output_dir = Path(args.output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    archive_path = output_dir / archive_name
    if archive_path.exists():
        fail("refusing to overwrite archive: %s" % archive_path)

    file_data = {"s1": binary.read_bytes()}
    for destination, source in package_sources.items():
        file_data[destination] = source.read_bytes()
    file_hashes = {name: hashlib.sha256(data).hexdigest() for name, data in sorted(file_data.items())}
    target_metadata = TARGETS[args.target]
    build_info = {
        "schema": "systemone-build-info-v1",
        "name": "s1",
        "version": version,
        "tag": tag,
        "source_ref": args.source_ref,
        "source_sha": args.source_sha,
        "workflow_run_url": args.workflow_run_url or None,
        "target": args.target,
        "platform": target_metadata["platform"],
        "architecture": target_metadata["architecture"],
        "features": target_metadata["features"],
        "rust_version": args.rust_version,
        "backend": {
            "llama_cpp_rs_crate": NATIVE_CRATE_VERSION,
            "llama_cpp_sys_crate": NATIVE_CRATE_VERSION,
            "llama_cpp_commit": LLAMA_CPP_COMMIT,
            "native_libraries": "static",
            "metal_library_embedded": args.target == "aarch64-apple-darwin",
        },
        "system_requirements": target_metadata["requirements"],
        "distribution": {
            "contains_model_weights": False,
            "developer_id_signed": False,
            "apple_notarized": False,
        },
        "files_sha256": file_hashes,
    }
    file_data["BUILD-INFO.json"] = json_bytes(build_info)

    try:
        with archive_path.open("xb") as raw_output:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw_output, mtime=args.source_date_epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                    add_directory(archive, root_name, args.source_date_epoch)
                    for name in MEMBER_FILES:
                        mode = 0o755 if name == "s1" else 0o644
                        add_bytes(archive, root_name + "/" + name, file_data[name], mode, args.source_date_epoch)
    except Exception:
        archive_path.unlink(missing_ok=True)
        raise

    print(str(archive_path))


def backend_identity(target):
    target_metadata = TARGETS[target]
    return {
        "name": "s1",
        "platform": target_metadata["platform"],
        "architecture": target_metadata["architecture"],
        "features": target_metadata["features"],
        "system_requirements": target_metadata["requirements"],
        "backend": {
            "llama_cpp_rs_crate": NATIVE_CRATE_VERSION,
            "llama_cpp_sys_crate": NATIVE_CRATE_VERSION,
            "llama_cpp_commit": LLAMA_CPP_COMMIT,
            "native_libraries": "static",
            "metal_library_embedded": target == "aarch64-apple-darwin",
        },
        "distribution": {
            "contains_model_weights": False,
            "developer_id_signed": False,
            "apple_notarized": False,
        },
    }


def archive_layout(members):
    file_members = [member.name for member in members if member.isfile()]
    directory_members = [member.name.rstrip("/") for member in members if member.isdir()]
    roots = {PurePosixPath(name).parts[0] for name in file_members}
    if len(roots) != 1:
        fail("archive must contain exactly one root directory")
    root_name = roots.pop()
    expected_files = [root_name + "/" + name for name in MEMBER_FILES]
    if file_members != expected_files:
        fail("archive members do not match the required ordered package contents")
    if directory_members != [root_name]:
        fail("archive must contain only its one versioned root directory")
    return root_name


def load_build_info(root):
    try:
        build_info = json.loads((root / "BUILD-INFO.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail("invalid BUILD-INFO.json: %s" % error)
    if not isinstance(build_info, dict) or build_info.get("schema") != "systemone-build-info-v1":
        fail("invalid build-info schema")
    version = build_info.get("version")
    target = build_info.get("target")
    if not isinstance(version, str) or not VERSION_RE.fullmatch(version):
        fail("invalid build-info version")
    if target not in TARGETS:
        fail("invalid build-info target")
    for key, expected_value in backend_identity(target).items():
        if build_info.get(key) != expected_value:
            fail("invalid build-info %s" % key)
    if not isinstance(build_info.get("rust_version"), str) or not build_info["rust_version"]:
        fail("invalid build-info Rust version")
    validate_ref(build_info.get("source_ref"), version)
    expected_tag = "v" + version if build_info.get("source_ref", "").startswith("refs/tags/") else None
    if build_info.get("tag") != expected_tag:
        fail("build-info tag is inconsistent with source_ref")
    if not SOURCE_SHA_RE.fullmatch(str(build_info.get("source_sha", ""))):
        fail("invalid build-info source SHA")
    return build_info


def check_expectations(args, build_info, archive_path, root_name):
    version = build_info["version"]
    target = build_info["target"]
    if args.expected_version and version != args.expected_version:
        fail("archive version %r does not match expected %r" % (version, args.expected_version))
    if args.expected_target and target != args.expected_target:
        fail("archive target %r does not match expected %r" % (target, args.expected_target))
    if args.expected_source_sha and build_info.get("source_sha") != args.expected_source_sha:
        fail("archive source SHA does not match expected source SHA")
    expected_root = "s1-v%s-%s" % (version, target)
    if root_name != expected_root or archive_path.name != expected_root + ".tar.gz":
        fail("archive/root name does not match build-info version and target")


def check_file_hashes(root, build_info):
    hashes = build_info.get("files_sha256")
    if not isinstance(hashes, dict) or set(hashes) != set(MEMBER_FILES) - {"BUILD-INFO.json"}:
        fail("build-info file hash manifest is incomplete")
    for name, expected_hash in hashes.items():
        if not re.fullmatch(r"[0-9a-f]{64}", str(expected_hash)):
            fail("invalid SHA-256 for %s" % name)
        if sha256_path(root / name) != expected_hash:
            fail("SHA-256 mismatch for %s" % name)


def verify_archive(args):
    archive_path = Path(args.archive).resolve()
    if not archive_path.is_file():
        fail("archive is not a regular file: %s" % archive_path)
    members = inspect_members(archive_path)
    root_name = archive_layout(members)
    with tempfile.TemporaryDirectory(prefix="s1-release-smoke-") as temporary:
        temporary_path = Path(temporary)
        extracted = temporary_path / "extracted"
        extract_safely(archive_path, extracted, members)
        root = extracted / root_name
        build_info = load_build_info(root)
        check_expectations(args, build_info, archive_path, root_name)
        check_file_hashes(root, build_info)
        binary = root / "s1"
        if not args.skip_execute:
            # Run from a directory outside both the source checkout and archive root.
            run_json_command(binary, "--version", "systemone-version-v1", build_info["version"], temporary_path)
            run_json_command(binary, "--help", "systemone-help-v1", cwd=temporary_path)
        if args.check_linkage:
            if args.skip_execute:
                fail("--check-linkage cannot be combined with --skip-execute")
            check_linkage(binary, build_info["target"])
    print("verified %s sha256=%s" % (archive_path.name, sha256_path(archive_path)))


def write_checksums(args):
    output = Path(args.output).resolve()
    if output.exists():
        fail("refusing to overwrite checksum file: %s" % output)
    archives = sorted((Path(value).resolve() for value in args.archives), key=lambda path: path.name)
    if len(archives) != 2 or len({path.name for path in archives}) != 2:
        fail("exactly two distinct release archives are required")
    versions = set()
    targets = set()
    for archive in archives:
        if not archive.is_file() or archive.parent != output.parent:
            fail("checksum inputs must be regular files beside the output: %s" % archive)
        match = re.fullmatch(
            r"s1-v(.+)-(aarch64-apple-darwin|x86_64-unknown-linux-gnu)\.tar\.gz",
            archive.name,
        )
        if match is None or not VERSION_RE.fullmatch(match.group(1)):
            fail("unexpected release archive name: %s" % archive.name)
        versions.add(match.group(1))
        targets.add(match.group(2))
    if len(versions) != 1:
        fail("checksum inputs must have the same release version")
    if targets != set(TARGETS):
        fail("checksum inputs must contain one archive for each supported target")
    lines = ["%s  %s\n" % (sha256_path(path), path.name) for path in archives]
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("x", encoding="utf-8", newline="\n") as handle:
        handle.writelines(lines)
    print(str(output))


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", default=str(REPO_ROOT), help="repository root (default: inferred)")
    subparsers = parser.add_subparsers(dest="command", required=True)

    validate = subparsers.add_parser("validate-ref", help="validate a source ref against Cargo version")
    validate.add_argument("--ref", required=True)
    validate.add_argument("--require-tag", action="store_true")

    package = subparsers.add_parser("package", help="create one deterministic binary archive")
    package.add_argument("--binary", required=True)
    package.add_argument("--target", required=True, choices=sorted(TARGETS))
    package.add_argument("--source-sha", required=True)
    package.add_argument("--source-ref", required=True)
    package.add_argument("--source-date-epoch", required=True, type=int)
    package.add_argument("--rust-version", required=True)
    package.add_argument("--workflow-run-url", default="")
    package.add_argument("--output-dir", required=True)

    verify = subparsers.add_parser("verify", help="validate and optionally execute an archive")
    verify.add_argument("--archive", required=True)
    verify.add_argument("--expected-version")
    verify.add_argument("--expected-target", choices=sorted(TARGETS))
    verify.add_argument("--expected-source-sha")
    verify.add_argument("--skip-execute", action="store_true")
    verify.add_argument("--check-linkage", action="store_true")

    notes = subparsers.add_parser("notes", help="print the CHANGELOG.md section for a release tag")
    notes.add_argument("--tag", required=True)

    checksums = subparsers.add_parser("checksums", help="write SHA256SUMS for both release archives")
    checksums.add_argument("--output", required=True)
    checksums.add_argument("archives", nargs="+")
    return parser


def changelog_section(changelog_text, version):
    """Return the body of the `## [version]` section of a Keep a Changelog file."""
    heading = re.compile(r"^## \[(?P<version>[^\]]+)\]")
    lines = changelog_text.splitlines()
    start = None
    for index, line in enumerate(lines):
        match = heading.match(line)
        if match and match.group("version") == version:
            start = index + 1
            break
    if start is None:
        fail("CHANGELOG.md has no '## [%s]' section" % version)
    body = []
    for line in lines[start:]:
        if heading.match(line):
            break
        body.append(line)
    text = "\n".join(body).strip()
    if not text:
        fail("CHANGELOG.md section for %s is empty" % version)
    return text + "\n"


def notes_command(args):
    repo_root = Path(args.repo_root).resolve()
    version = workspace_version(repo_root)
    validate_ref("refs/tags/" + args.tag, version, require_tag=True)
    changelog = (repo_root / "CHANGELOG.md").read_text(encoding="utf-8")
    sys.stdout.write(changelog_section(changelog, version))


def validate_ref_command(args):
    version = workspace_version(Path(args.repo_root).resolve())
    tag = validate_ref(args.ref, version, args.require_tag)
    print(tag or version)


COMMANDS = {
    "validate-ref": validate_ref_command,
    "package": package_archive,
    "verify": verify_archive,
    "checksums": write_checksums,
    "notes": notes_command,
}


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        COMMANDS[args.command](args)
    except (ReleaseError, OSError, subprocess.SubprocessError) as error:
        print("release.py: error: %s" % error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
