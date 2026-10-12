#!/usr/bin/env python3
"""Build and verify RAZE Windows packages; Python 3.13 + OpenSSL 3, no pip."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_KEY = ROOT / "release/public-key.pem"
REPO = "https://github.com/RERCON0/RAZE"
TARGET = "x86_64-pc-windows-msvc"
PAYLOAD = {"raze.exe", "raze-app.exe", "raze_shell.dll", "RAZE.msix", "RAZE.cer",
           "README.md", "LICENSE", "FONT-LICENSE.txt"}
META = {"manifest.json", "manifest.sig", "public-key.pem"}
SUBSYSTEMS = {"raze.exe": 3, "raze-app.exe": 2, "raze_shell.dll": 2}
MAX_FILE = 64 * 1024 * 1024
MAX_PACKAGE = 160 * 1024 * 1024
MAX_META = 2 * 1024 * 1024
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")


def run(args: list[str], *, cwd: Path = ROOT, capture: bool = True, timeout: int = 120,
        input: bytes | None = None) -> bytes:
    result = subprocess.run(args, cwd=cwd, check=True, timeout=timeout, input=input,
                            stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None)
    return result.stdout if capture else b""


def openssl() -> str:
    found = shutil.which("openssl")
    if not found and os.name == "nt":
        for rel in ("Git/mingw64/bin/openssl.exe", "Git/usr/bin/openssl.exe"):
            candidate = Path(os.environ.get("ProgramFiles", "C:/Program Files")) / rel
            if candidate.is_file():
                found = str(candidate)
                break
    if not found:
        raise ValueError("OpenSSL 3 is required (Windows: included with Git for Windows)")
    if not run([found, "version"]).startswith(b"OpenSSL 3."):
        raise ValueError("OpenSSL 3 is required")
    return found


def canonical(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":")) + "\n").encode()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def public_der(path: Path) -> bytes:
    value = run([openssl(), "pkey", "-pubin", "-in", str(path), "-outform", "DER"])
    if len(value) != 44 or not value.startswith(ED25519_SPKI_PREFIX):
        raise ValueError("Only Ed25519 release keys are accepted")
    return value


def sign(manifest: bytes, key: Path) -> bytes:
    with tempfile.TemporaryDirectory(prefix="raze-sign-") as folder:
        data, signature = Path(folder) / "manifest", Path(folder) / "signature"
        data.write_bytes(manifest)
        run([openssl(), "pkeyutl", "-sign", "-rawin", "-inkey", str(key),
             "-in", str(data), "-out", str(signature)])
        return signature.read_bytes()


def verify_signature(manifest: bytes, signature: bytes, key: Path) -> None:
    if len(signature) != 64:
        raise ValueError("Invalid Ed25519 signature length")
    with tempfile.TemporaryDirectory(prefix="raze-verify-") as folder:
        data, sig = Path(folder) / "manifest", Path(folder) / "signature"
        data.write_bytes(manifest)
        sig.write_bytes(signature)
        run([openssl(), "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", str(key),
             "-in", str(data), "-sigfile", str(sig)])


def pe_info(data: bytes, subsystem: int, *, dll: bool = False) -> dict:
    """Reject wrong architecture/image type, missing mitigations and dynamic CRT."""
    def unpack(fmt: str, offset: int) -> tuple:
        size = struct.calcsize(fmt)
        if offset < 0 or offset + size > len(data):
            raise ValueError("Truncated PE image")
        return struct.unpack_from(fmt, data, offset)

    if data[:2] != b"MZ":
        raise ValueError("Missing DOS header")
    pe, = unpack("<I", 0x3C)
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Missing PE header")
    machine, sections = unpack("<HH", pe + 4)
    opt_size, = unpack("<H", pe + 20)
    characteristics, = unpack("<H", pe + 22)
    if bool(characteristics & 0x2000) != dll:
        raise ValueError("Incorrect PE executable/DLL image type")
    opt = pe + 24
    magic, = unpack("<H", opt)
    actual_subsystem, flags = unpack("<HH", opt + 68)
    if machine != 0x8664 or magic != 0x20B or actual_subsystem != subsystem:
        raise ValueError("Expected Windows x64 PE with the correct CLI/GUI subsystem")
    if flags & 0x160 != 0x160:
        raise ValueError("PE requires ASLR, high-entropy ASLR and DEP")
    if opt_size < 128 or sections == 0 or sections > 96:
        raise ValueError("Invalid PE sections/optional header")
    mappings = []
    for number in range(sections):
        offset = opt + opt_size + 40 * number
        _, virtual_size, rva, raw_size, raw = unpack("<8sIIII", offset)
        mappings.append((rva, max(virtual_size, raw_size), raw, raw_size))

    def position(rva: int, size: int = 1) -> int:
        for start, length, raw, raw_size in mappings:
            delta = rva - start
            if 0 <= delta < length and delta + size <= raw_size:
                at = raw + delta
                if at + size <= len(data):
                    return at
        raise ValueError("PE RVA outside file-backed sections")

    imports_rva, imports_size = unpack("<II", opt + 120)
    imports = []
    if not imports_rva or imports_size < 20 or imports_size > MAX_META:
        raise ValueError("Missing/invalid PE imports")
    for index in range(min(imports_size // 20, 4096)):
        descriptor = unpack("<IIIII", position(imports_rva + index * 20, 20))
        if not any(descriptor):
            break
        at = position(descriptor[3])
        name = data[at:at + 256].split(b"\0", 1)[0].decode("ascii").lower()
        if not re.fullmatch(r"[a-z0-9_.-]+\.dll", name):
            raise ValueError("Invalid PE import name")
        if name.startswith(("vcruntime", "msvcp", "api-ms-win-crt")) or name == "ucrtbase.dll":
            raise ValueError(f"Dynamic Visual C++ runtime dependency: {name}")
        imports.append(name)
    else:
        raise ValueError("Unterminated PE import table")
    return {"machine": "amd64", "subsystem": subsystem, "dll_characteristics": flags,
            "imports": sorted(set(imports))}


def reject_duplicates(pairs: list[tuple[str, object]]) -> dict:
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"Duplicate JSON field: {key}")
        value[key] = item
    return value


def verify_package(path: Path, trusted_key: Path = PUBLIC_KEY) -> dict:
    """Do not extract or execute any member; the key must be trusted externally."""
    trusted_der = public_der(trusted_key)
    if path.stat().st_size > MAX_PACKAGE:
        raise ValueError("Package exceeds size limit")
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        names = [entry.filename for entry in entries]
        if len(names) != len(set(names)) or set(names) != PAYLOAD | META:
            raise ValueError("Unexpected, missing or duplicate ZIP members")
        if sum(entry.file_size for entry in entries) > MAX_PACKAGE:
            raise ValueError("Expanded ZIP exceeds size limit")
        for entry in entries:
            limit = MAX_META if entry.filename in META else MAX_FILE
            mode = entry.external_attr >> 16
            if entry.file_size <= 0 or entry.file_size > limit or entry.flag_bits & 1:
                raise ValueError("Invalid ZIP member size/encryption")
            if entry.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
                raise ValueError("Unsupported ZIP compression")
            if stat.S_IFMT(mode) not in (0, stat.S_IFREG):
                raise ValueError("Non-regular ZIP member")
        # read() verifies each member's CRC; size limits were enforced above.
        content = {name: archive.read(name) for name in names}
    with tempfile.TemporaryDirectory(prefix="raze-key-") as folder:
        bundled = Path(folder) / "public.pem"
        bundled.write_bytes(content["public-key.pem"])
        if public_der(bundled) != trusted_der:
            raise ValueError("Bundled key differs from the trusted RAZE key")
    raw = content["manifest.json"]
    verify_signature(raw, content["manifest.sig"], trusted_key)
    manifest = json.loads(raw, object_pairs_hook=reject_duplicates)
    expected_fields = {"schema", "project", "version", "target", "source", "toolchain", "files", "signer_sha256"}
    if not isinstance(manifest, dict) or set(manifest) != expected_fields or canonical(manifest) != raw:
        raise ValueError("Invalid/non-canonical manifest schema")
    if type(manifest["schema"]) is not int or manifest["schema"] != 1 or manifest["project"] != "RAZE" or manifest["target"] != TARGET:
        raise ValueError("Invalid manifest project/target")
    if not isinstance(manifest["version"], str) or not re.fullmatch(r"\d+\.\d+\.\d+", manifest["version"]):
        raise ValueError("Invalid version")
    source = manifest["source"]
    if not isinstance(source, dict) or set(source) != {"commit", "tree", "inputs_sha256", "url"}:
        raise ValueError("Invalid source provenance")
    for field in ("commit", "tree"):
        if not isinstance(source[field], str) or not re.fullmatch(r"[0-9a-f]{40}", source[field]):
            raise ValueError("Invalid source revision")
    if not isinstance(source["inputs_sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", source["inputs_sha256"]):
        raise ValueError("Invalid source input hash")
    if source["url"] != REPO + "/tree/" + source["commit"]:
        raise ValueError("Invalid source URL")
    toolchain = manifest["toolchain"]
    if not isinstance(toolchain, dict) or set(toolchain) != {"rustc", "cargo", "cargo_lock_sha256"}:
        raise ValueError("Invalid toolchain provenance")
    if not all(isinstance(v, str) and 0 < len(v) < 4096 for v in toolchain.values()):
        raise ValueError("Invalid toolchain fields")
    if not re.fullmatch(r"[0-9a-f]{64}", toolchain["cargo_lock_sha256"]):
        raise ValueError("Invalid lockfile hash")
    if manifest["signer_sha256"] != sha256(trusted_der):
        raise ValueError("Invalid signer fingerprint")
    files = manifest["files"]
    if not isinstance(files, dict) or set(files) != PAYLOAD:
        raise ValueError("Invalid payload inventory")
    for name in sorted(PAYLOAD):
        record = files[name]
        fields = {"sha256", "size", "pe"} if name.endswith(".exe") else {"sha256", "size"}
        if not isinstance(record, dict) or set(record) != fields:
            raise ValueError("Invalid payload record")
        if type(record["size"]) is not int or record["size"] != len(content[name]) or record["sha256"] != sha256(content[name]):
            raise ValueError(f"Payload hash/size mismatch: {name}")
        if name in SUBSYSTEMS:
            info = pe_info(content[name], SUBSYSTEMS[name], dll=name.endswith(".dll"))
            if name.endswith(".exe") and record["pe"] != info:
                raise ValueError("PE metadata mismatch")
    return manifest


def source_state() -> dict:
    if run(["git", "status", "--porcelain", "--untracked-files=all"]).strip():
        raise ValueError("Release builds require a clean committed checkout")
    commit = run(["git", "rev-parse", "HEAD"]).decode().strip()
    tree = run(["git", "rev-parse", "HEAD^{tree}"]).decode().strip()
    tracked = run(["git", "ls-files", "-z"]).decode().split("\0")
    inputs = {}
    for name in sorted(filter(None, tracked)):
        path = ROOT / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"Source input is not a regular file: {name}")
        inputs[name] = sha256(path.read_bytes())
    return {"commit": commit, "tree": tree, "inputs_sha256": sha256(canonical(inputs)),
            "url": REPO + "/tree/" + commit}


def package(payload: dict[str, bytes], source: dict, toolchain: dict, version: str,
            private_key: Path, public_key: Path, output: Path) -> dict:
    der = public_der(public_key)
    derived = run([openssl(), "pkey", "-in", str(private_key), "-pubout", "-outform", "DER"])
    if derived != der:
        raise ValueError("Signing key does not match the trusted public key")
    if set(payload) != PAYLOAD:
        raise ValueError("Invalid build payload")
    files = {}
    for name, data in payload.items():
        if not data or len(data) > MAX_FILE:
            raise ValueError(f"Invalid payload size: {name}")
        files[name] = {"sha256": sha256(data), "size": len(data)}
        if name in SUBSYSTEMS:
            info = pe_info(data, SUBSYSTEMS[name], dll=name.endswith(".dll"))
            if name.endswith(".exe"):
                files[name]["pe"] = info
    manifest = {"schema": 1, "project": "RAZE", "version": version, "target": TARGET,
                "source": source, "toolchain": toolchain, "files": files, "signer_sha256": sha256(der)}
    raw = canonical(manifest)
    entries = {**payload, "manifest.json": raw, "manifest.sig": sign(raw, private_key),
               "public-key.pem": public_key.read_bytes()}
    output.parent.mkdir(parents=True, exist_ok=True)
    # Unique sibling + os.replace: never leave a partial archive at the release path.
    descriptor, temporary = tempfile.mkstemp(prefix=output.name + ".", suffix=".tmp", dir=output.parent)
    os.close(descriptor)
    candidate = Path(temporary)
    try:
        with zipfile.ZipFile(candidate, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name, data in sorted(entries.items()):
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | 0o644) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, data)
        verify_package(candidate, public_key)
        os.replace(candidate, output)
    finally:
        candidate.unlink(missing_ok=True)
    return manifest


def build(output: Path, private_key: Path) -> dict:
    if sys.platform != "win32":
        raise ValueError("Release builds require native Windows MSVC")
    if private_key.resolve().is_relative_to(ROOT):
        raise ValueError("Private signing keys must be stored outside the repository")
    disallowed = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER",
                  "RUSTC_WORKSPACE_WRAPPER", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET"}
    for name in os.environ:
        if name in disallowed or name.startswith("CARGO_PROFILE_RELEASE_"):
            raise ValueError(f"Release build environment override is forbidden: {name}")
    # The sparse package and RAZE.cer come first: raze.exe bakes in the pinned
    # certificate thumbprint that build-package.ps1 may have just rewritten.
    run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass",
         "-File", str(ROOT / "packaging" / "build-package.ps1")], capture=False, timeout=900)
    source = source_state()
    toolchain = {"rustc": run(["rustc", "--version", "--verbose"]).decode().strip(),
                 "cargo": run(["cargo", "--version"]).decode().strip(),
                 "cargo_lock_sha256": sha256((ROOT / "Cargo.lock").read_bytes())}
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    # Fresh target tree prevents stale/foreign executables from being packaged.
    with tempfile.TemporaryDirectory(prefix="raze-build-") as folder:
        target = Path(folder) / "target"
        run(["cargo", "build", "--locked", "--release", "--workspace", "--target", TARGET,
             "--target-dir", str(target)], capture=False, timeout=2400)
        built = target / TARGET / "release"
        payload = {name: (built / name).read_bytes()
                   for name in ("raze.exe", "raze-app.exe", "raze_shell.dll")}
        package_dir = ROOT / "target" / "release"
        payload.update({name: (package_dir / name).read_bytes() for name in ("RAZE.msix", "RAZE.cer")})
        payload.update({"README.md": release_readme(version, source["commit"]),
                        "LICENSE": (ROOT / "LICENSE").read_bytes(),
                        "FONT-LICENSE.txt": (ROOT / "fonts" / "OFL-notice.txt").read_bytes()})
        if source_state() != source:
            raise ValueError("Source checkout changed during compilation")
        return package(payload, source, toolchain, version, private_key, PUBLIC_KEY, output)


def release_readme(version: str, commit: str) -> bytes:
    base = REPO + "/tree/" + commit
    return (f"# RAZE {version} — Windows x64\n\n"
            "Point removal of stuck files and folders on Windows.\n"
            "Run raze-app.exe for the GUI, or raze.exe in a terminal.\n"
            "Windows 11 context menu: raze --install asks for administrator rights once, copies these\n"
            "files into %ProgramFiles%\\RAZE, trusts the pinned package certificate and registers\n"
            'the "Delete with RAZE" menu entry (objects that cannot go to the Recycle Bin ask\n'
            "for confirmation first); raze --uninstall reverts every change.\n"
            "Dry run: raze -n <path>. Permanent deletion: raze --permanent <path>.\n"
            "Delete at the next Windows restart: raze --reboot <path>.\n\n"
            "The EXEs have no Authenticode signature. SmartScreen may show 'Windows protected your PC'.\n"
            "For a trusted download from official Releases, choose More info -> Run anyway.\n"
            "RAZE.msix is signed with a self-signed certificate (CN=RERCON RAZE, pinned in raze.exe);\n"
            "the Ed25519 package signature does not remove this warning.\n\n"
            "Verify from a trusted checkout (requires Python 3.13+ and OpenSSL 3):\n"
            "python scripts/release.py verify <path-to-this-ZIP>\n"
            "Do not obtain the trusted key only from the archive being checked.\n\n"
            f"Source and documentation: {base}\n"
            f"Русская документация: {base}/README.ru.md\n"
            f"Release verification: {base}/docs/RELEASING.md\n\n"
            "---\n\n# Русский\n\n"
            "Точечное удаление застрявших файлов и папок в Windows.\n"
            "Запустите raze-app.exe для окна приложения или raze.exe в терминале.\n"
            "Меню Windows 11: raze --install один раз попросит права администратора, скопирует эти\n"
            "файлы в %ProgramFiles%\\RAZE, добавит доверие закреплённому сертификату пакета\n"
            'и зарегистрирует пункт "Delete with RAZE" (безвозвратные объекты сначала спрашивают\n'
            "подтверждение); raze --uninstall отменяет всё это.\n"
            "Сухой прогон: raze -n <путь>. Удаление мимо корзины: raze --permanent <путь>.\n"
            "Удаление при следующей перезагрузке: raze --reboot <путь>.\n\n"
            "EXE пока без Authenticode-подписи. SmartScreen может показать «Windows защитила ваш\n"
            "компьютер»; для доверенного архива из официальных Releases нажмите\n"
            "Подробнее -> Выполнить в любом случае.\n"
            "RAZE.msix подписан самозаверяющим сертификатом (CN=RERCON RAZE, отпечаток вшит\n"
            "в raze.exe); подпись Ed25519 не убирает это предупреждение.\n\n"
            "Проверьте подпись из доверенного checkout (Python 3.13+ и OpenSSL 3):\n"
            "python scripts/release.py verify <путь-к-этому-ZIP>\n"
            "Не берите доверенный ключ только из самого проверяемого архива.\n\n"
            f"Исходники и документация: {base}/README.ru.md\n"
            f"Проверка релиза: {base}/docs/RELEASING.md\n\n"
            "LICENSE: GNU GPL v3.0 or later; FONT-LICENSE.txt: Cascadia Mono / SIL OFL.\n"
            "Ed25519 covers the manifest and hashes of all ZIP files.\n"
            "Подпись Ed25519 охватывает манифест и хеши всех файлов ZIP.\n").encode("utf-8")


def keygen(private: Path, public: Path) -> None:
    if private.resolve().is_relative_to(ROOT):
        raise ValueError("Private keys must live outside the repository")
    if private.resolve() == public.resolve():
        raise ValueError("Private and public key paths must differ")
    private.parent.mkdir(parents=True, exist_ok=True)
    public.parent.mkdir(parents=True, exist_ok=True)
    # Reserve both destinations exclusively, including dangling symlinks. Do not
    # change permissions on a caller's existing directory or ask OpenSSL to
    # reopen output paths, which could overwrite another key.
    created: list[Path] = []
    try:
        with os.fdopen(os.open(private, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as private_file:
            created.append(private)
            if os.name == "nt":
                # Reset explicit grants on this new, still empty file, then remove
                # inherited grants. OWNER RIGHTS refers to the actual file owner,
                # not USERNAME/USERDOMAIN inherited from the caller's environment.
                run(["icacls", str(private), "/reset"])
                run(["icacls", str(private), "/inheritance:r", "/grant:r", "*S-1-3-4:F"])
            with public.open("xb") as public_file:
                created.append(public)
                secret = run([openssl(), "genpkey", "-algorithm", "ED25519"])
                published = run([openssl(), "pkey", "-pubout"], input=secret)
                private_file.write(secret)
                public_file.write(published)
        fingerprint = sha256(public_der(public))
    except BaseException:
        for path in reversed(created):
            path.unlink(missing_ok=True)
        raise
    print("Public key SHA-256:", fingerprint)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    verify = commands.add_parser("verify", help="Verify without extracting or running binaries")
    verify.add_argument("archive", type=Path)
    verify.add_argument("--public-key", type=Path, default=PUBLIC_KEY)
    release = commands.add_parser("build", help="Clean source -> fresh MSVC build -> signed, verified ZIP")
    release.add_argument("--private-key", type=Path, required=True)
    release.add_argument("--output", type=Path, default=ROOT / "dist/raze-windows-x64.zip")
    keys = commands.add_parser("keygen", help="Create an independent publisher key; never overwrites")
    keys.add_argument("--private-key", type=Path, required=True)
    keys.add_argument("--public-key", type=Path, default=PUBLIC_KEY)
    binaries = commands.add_parser("check-binaries", help="Inspect unsigned candidates, without running them")
    binaries.add_argument("directory", type=Path)
    args = parser.parse_args()
    if args.command == "check-binaries":
        for name, subsystem in sorted(SUBSYSTEMS.items()):
            path = args.directory / name
            if not 0 < path.stat().st_size <= MAX_FILE:
                raise ValueError("Invalid executable size")
            data = path.read_bytes()
            print(name, sha256(data), json.dumps(pe_info(data, subsystem, dll=name.endswith(".dll")), sort_keys=True))
        return
    if args.command == "keygen":
        keygen(args.private_key, args.public_key)
        return
    if args.command == "verify":
        manifest = verify_package(args.archive, args.public_key)
    else:
        manifest = build(args.output, args.private_key)
    print(f"Verified RAZE {manifest['version']} / {manifest['source']['commit']} / {manifest['signer_sha256']}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError, zipfile.BadZipFile, KeyError, TypeError) as error:
        print(f"Release rejected: {error}", file=sys.stderr)
        sys.exit(1)
