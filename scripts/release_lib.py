"""Shared constants and helpers for SystemOne release packaging.

Standard library only. Python is a build/verification dependency, not an
`s1` runtime dependency.
"""

import hashlib
import json
from pathlib import PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile

TARGETS = {
    "aarch64-apple-darwin": {
        "features": ["metal"],
        "platform": "macos",
        "architecture": "arm64",
        "requirements": {
            "minimum_macos": "14.0",
            "runtime_tools_not_required": ["Homebrew", "Xcode", "CMake", "Python"],
        },
    },
    "x86_64-unknown-linux-gnu": {
        "features": ["native"],
        "platform": "linux",
        "architecture": "x86_64",
        "requirements": {
            "minimum_glibc": "2.35",
            "shared_libraries": ["glibc", "libstdc++", "libgcc"],
            "runtime_tools_not_required": ["CMake", "clang", "Python"],
        },
    },
    "x86_64-pc-windows-msvc": {
        "features": ["native"],
        "platform": "windows",
        "architecture": "x86_64",
        "requirements": {
            "minimum_windows": "10 (1809)",
            "shared_libraries": ["Windows system DLLs only; the C runtime is linked statically"],
            "runtime_tools_not_required": ["Visual C++ Redistributable", "CMake", "Python"],
        },
    },
}
NATIVE_CRATE_VERSION = "0.1.156"
LLAMA_CPP_COMMIT = "e79e4bf660e19f2ad851e06c6913f7a8c5852621"
SOURCE_ARCHIVE_PATHS = {
    "colored-3.1.1.crate": ("licenses", "sources", "colored-3.1.1.crate"),
    "option-ext-0.2.0.crate": ("licenses", "sources", "option-ext-0.2.0.crate"),
}
RUST_NOTICE_PATH = ("licenses", "RUST-COPYRIGHT-library.html")
# Package contents in archive order. The executable is `s1` on macOS and
# Linux and `s1.exe` on Windows; see member_files().
MEMBER_FILES = (
    "s1",
    "LICENSE",
    "THIRD_PARTY.md",
    "THIRD_PARTY_LICENSES.html",
    "RUST-COPYRIGHT-library.html",
    "colored-3.1.1.crate",
    "option-ext-0.2.0.crate",
    "README.md",
    "BUILD-INFO.json",
)
def binary_name(target):
    return "s1.exe" if TARGETS[target]["platform"] == "windows" else "s1"


def member_files(target):
    """Ordered archive members for one target."""
    return tuple(binary_name(target) if name == "s1" else name for name in MEMBER_FILES)


def target_of_root(root_name):
    """The target encoded in an archive root `s1-vVERSION-TARGET`, or None."""
    for target in TARGETS:
        if root_name.endswith("-" + target):
            return target
    return None


SOURCE_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
VERSION_RE = re.compile(r'^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$')



class ReleaseError(Exception):
    pass


def fail(message):
    raise ReleaseError(message)


def sha256_path(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run_json_command(binary, argument, expected_schema, expected_version=None, cwd=None):
    completed = subprocess.run(
        [str(binary), argument],
        cwd=str(cwd) if cwd else None,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        fail("%s %s exited %d" % (binary, argument, completed.returncode))
    if completed.stderr:
        fail("%s %s wrote to stderr" % (binary, argument))
    try:
        text = completed.stdout.decode("utf-8")
        value = json.loads(text)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail("%s %s did not emit one UTF-8 JSON value: %s" % (binary, argument, error))
    if not isinstance(value, dict) or value.get("schema") != expected_schema:
        fail("%s %s emitted the wrong schema" % (binary, argument))
    if expected_version is not None and value.get("version") != expected_version:
        fail("binary version %r does not match workspace version %r" % (value.get("version"), expected_version))
    return value


def json_bytes(value):
    return (json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n").encode("utf-8")


def add_directory(archive, name, epoch):
    info = tarfile.TarInfo(name.rstrip("/") + "/")
    info.type = tarfile.DIRTYPE
    info.mode = 0o755
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = epoch
    archive.addfile(info)


def add_bytes(archive, name, data, mode, epoch):
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = epoch
    with tempfile.SpooledTemporaryFile() as handle:
        handle.write(data)
        handle.seek(0)
        archive.addfile(info, handle)


def safe_member_name(name):
    if not name or "\\" in name:
        return False
    path = PurePosixPath(name)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        return False
    return True


def inspect_members(archive_path):
    try:
        with tarfile.open(str(archive_path), mode="r:gz") as archive:
            members = archive.getmembers()
    except (tarfile.TarError, OSError) as error:
        fail("invalid release archive %s: %s" % (archive_path, error))
    if not members:
        fail("release archive is empty")
    seen = set()
    for member in members:
        if not safe_member_name(member.name):
            fail("unsafe archive member: %r" % member.name)
        if member.name in seen:
            fail("duplicate archive member: %s" % member.name)
        seen.add(member.name)
        if not (member.isdir() or member.isfile()):
            fail("archive links/devices are forbidden: %s" % member.name)
    return members


def extract_safely(archive_path, destination, members):
    destination.mkdir(parents=True, exist_ok=False)
    with tarfile.open(str(archive_path), mode="r:gz") as archive:
        for member in members:
            output = destination.joinpath(*PurePosixPath(member.name).parts)
            if member.isdir():
                output.mkdir(mode=0o755, parents=True, exist_ok=True)
                continue
            output.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
            source = archive.extractfile(member)
            if source is None:
                fail("could not read archive member: %s" % member.name)
            with source, output.open("xb") as target:
                shutil.copyfileobj(source, target)
            output.chmod(member.mode & 0o777)


# Import libraries that belong to the Visual C++ runtime. Release builds link
# the C runtime statically, so none of these may appear.
WINDOWS_FORBIDDEN_IMPORT_PREFIXES = ("vcruntime", "msvcp", "concrt", "vccorlib")


def pe_imports(data):
    """Machine type and imported DLL names of a PE32+ image (standard library only)."""
    import struct

    if data[:2] != b"MZ":
        fail("packaged binary is not a PE image")
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_offset:pe_offset + 4] != b"PE\0\0":
        fail("packaged binary has no PE signature")
    machine, sections, _, _, _, optional_size, _ = struct.unpack_from("<HHIIIHH", data, pe_offset + 4)
    optional = pe_offset + 24
    if struct.unpack_from("<H", data, optional)[0] != 0x20B:
        fail("packaged binary is not PE32+")
    import_rva = struct.unpack_from("<I", data, optional + 112 + 8)[0]
    section_table = optional + optional_size
    ranges = []
    for index in range(sections):
        base = section_table + index * 40
        virtual_size, virtual_address, raw_size, raw_pointer = struct.unpack_from("<IIII", data, base + 8)
        ranges.append((virtual_address, max(virtual_size, raw_size), raw_pointer))

    def offset(rva):
        for virtual_address, size, raw_pointer in ranges:
            if virtual_address <= rva < virtual_address + size:
                return rva - virtual_address + raw_pointer
        fail("PE import table points outside every section")

    names = []
    if import_rva:
        descriptor = offset(import_rva)
        while True:
            fields = struct.unpack_from("<IIIII", data, descriptor)
            if not any(fields):
                break
            name_at = offset(fields[3])
            end = data.index(b"\0", name_at)
            names.append(data[name_at:end].decode("ascii"))
            descriptor += 20
    return machine, names


def check_windows_imports(names):
    for name in names:
        lowered = name.lower()
        if lowered.startswith(WINDOWS_FORBIDDEN_IMPORT_PREFIXES):
            fail("Windows binary depends on the Visual C++ runtime (%s); link it statically" % name)
        if not lowered.endswith(".dll"):
            fail("unexpected Windows import: %s" % name)


def check_linkage(binary, target):
    if target == "x86_64-pc-windows-msvc":
        machine, names = pe_imports(binary.read_bytes())
        if machine != 0x8664:
            fail("packaged binary is not x86-64 (machine 0x%04x)" % machine)
        check_windows_imports(names)
        import os
        system = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32")
        if os.path.isdir(system):
            for name in names:
                if not name.lower().startswith("api-ms-win-") and not os.path.isfile(os.path.join(system, name)):
                    fail("non-system Windows dependency: %s" % name)
        print("PE32+ x86-64 executable; imports: %s" % ", ".join(sorted(names, key=str.lower)))
        return
    file_result = subprocess.run(["file", str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False, text=True)
    if file_result.returncode != 0:
        fail("file failed for packaged binary: %s" % file_result.stderr.strip())
    description = file_result.stdout
    if target == "aarch64-apple-darwin":
        if "Mach-O" not in description or "arm64" not in description:
            fail("packaged binary is not Mach-O arm64: %s" % description.strip())
        result = subprocess.run(["otool", "-L", str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False, text=True)
        if result.returncode != 0:
            fail("otool -L failed: %s" % result.stderr.strip())
        for line in result.stdout.splitlines()[1:]:
            dependency = line.strip().split(" ", 1)[0]
            if dependency and not dependency.startswith(("/usr/lib/", "/System/Library/")):
                fail("non-system macOS dependency: %s" % dependency)
    else:
        if "ELF 64-bit" not in description or "x86-64" not in description:
            fail("packaged binary is not ELF x86-64: %s" % description.strip())
        result = subprocess.run(["ldd", str(binary)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False, text=True)
        if result.returncode != 0:
            fail("ldd failed: %s" % result.stdout.strip())
        for line in result.stdout.splitlines():
            stripped = line.strip()
            if "not found" in stripped:
                fail("unresolved Linux dependency: %s" % stripped)
            if "=>" in stripped:
                resolved = stripped.split("=>", 1)[1].strip().split(" ", 1)[0]
                if resolved and not resolved.startswith(("/lib/", "/lib64/", "/usr/lib/", "/usr/lib64/")):
                    fail("non-system Linux dependency: %s" % resolved)
            elif stripped.startswith("/"):
                resolved = stripped.split(" ", 1)[0]
                if not resolved.startswith(("/lib/", "/lib64/", "/usr/lib/", "/usr/lib64/")):
                    fail("non-system Linux loader: %s" % resolved)
    print(description.strip())
