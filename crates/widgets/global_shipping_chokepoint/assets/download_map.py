"""Download the four OSM extracts `build_map.py` packs into `hormuz_map.bin`.

The sources are large (about 1.5 GiB in total) and are **not** committed: the
repository only keeps this script, `build_map.py`, `README.md` and the generated
asset. Run this first, then `build_map.py`.

    python crates/widgets/global_shipping_chokepoint/assets/download_map.py

The download itself is left to the system `curl`: it resumes a partial file with
`-C -`, retries a dropped connection with `--retry 3`, and understands the proxy
strings `--proxy` accepts (`http://`, `socks5h://`, ...). Every file is written
to `<name>.part` and only renamed once curl exits successfully, so a killed run
never leaves a truncated file under the real name.

The recorded snapshot sizes are what a re-run compares against: a file at least
that large cannot be a truncated download, so it is kept even when it is larger,
with a warning that the upstream extract moved on and that the counts in
`README.md` no longer apply. A smaller file is downloaded again, as is every
file with `--force`, and `--sha256` also downloads a file again when its hash
differs from the recorded snapshot.

Standard library only.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

#: Where the sources land: next to this script, i.e. in the asset directory.
ASSET_DIR = Path(__file__).resolve().parent

#: Refuse to start a download with less free space than this.
MIN_FREE_BYTES = 2 * 1024**3

#: curl exit codes that mean "the resume is impossible", not "the network broke":
#: 33 is "the server does not support byte ranges", 36 "bad download resume".
RESUME_REFUSED_CODES = frozenset({33, 36})

#: The command `build_map.py` tells the user to run when a source is missing.
DOWNLOAD_COMMAND = "python crates/widgets/global_shipping_chokepoint/assets/download_map.py"

#: The build command printed once every source is in place.
BUILD_COMMAND = "python crates/widgets/global_shipping_chokepoint/assets/build_map.py"


@dataclass(frozen=True)
class Source:
    """One file `build_map.py` reads."""

    #: File name inside the asset directory.
    name: str
    #: The public download URL.
    url: str
    #: Size of the snapshot this script was written against, in bytes.
    snapshot_bytes: int
    #: SHA256 of that snapshot, the same value `README.md` records.
    sha256: str


#: The four sources, in the order the README lists them.
SOURCES: tuple[Source, ...] = (
    Source(
        name="land-polygons-split-4326.zip",
        url="https://osmdata.openstreetmap.de/download/land-polygons-split-4326.zip",
        snapshot_bytes=928_619_967,
        sha256="84d44f2df27e501c3e8eb83777bca314897660ecfce8d09f6b00c588072da163",
    ),
    Source(
        name="gcc-states-latest.osm.pbf",
        url="https://download.geofabrik.de/asia/gcc-states-latest.osm.pbf",
        snapshot_bytes=256_396_961,
        sha256="363606bd76d12ce03f0e2854fc9b3c7c2c5ea5b170cbec454b85c44ad65bba2a",
    ),
    Source(
        name="iran-latest.osm.pbf",
        url="https://download.geofabrik.de/asia/iran-latest.osm.pbf",
        snapshot_bytes=229_955_721,
        sha256="06d98c2b3dd4615b6cc29b7cc6a44705152a63be9e57fac67db3bbc456ce5a49",
    ),
    Source(
        name="pakistan-latest.osm.pbf",
        url="https://download.geofabrik.de/asia/pakistan-latest.osm.pbf",
        snapshot_bytes=157_522_517,
        sha256="79138552345d21cf5aa362f40b7a765e5f9bd7a1ba8cafc3f34735f3722c16d9",
    ),
)


def human_bytes(size: int) -> str:
    """`size` as bytes and a one-decimal MiB value, for the log lines."""
    return f"{size:,} B ({size / (1024 * 1024):.1f} MiB)"


def proxy_from_environment() -> str | None:
    """The proxy `--proxy` defaults to, or `None` when no variable is set."""
    for name in ("HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"):
        value = os.environ.get(name)
        if value:
            return value
    return None


def require_curl() -> str:
    """The `curl` executable, or a clear failure."""
    curl = shutil.which("curl")
    if curl is None:
        print(
            "error: curl was not found on PATH; install curl or use the one that "
            "ships with Git for Windows",
            file=sys.stderr,
        )
        raise SystemExit(2)
    return curl


def curl_once(curl: str, source: Source, part: Path, proxy: str | None) -> int:
    """Run one curl download into `part`; returns its exit code."""
    command = [curl, "-L", "--fail", "--retry", "3", "-C", "-", "--silent", "--show-error"]
    if proxy:
        command += ["--proxy", proxy]
    command += ["-o", str(part), source.url]
    return subprocess.run(command, check=False).returncode


def download(
    curl: str, source: Source, target: Path, proxy: str | None, fresh: bool = False
) -> None:
    """Download one source to `target`, resuming `target.part` when it exists."""
    part = target.with_name(target.name + ".part")
    if fresh:
        # `--force` means from zero: a leftover `.part` may be a prefix of the
        # previous snapshot, and appending the new one would corrupt the file.
        part.unlink(missing_ok=True)
    resumed = part.exists() and part.stat().st_size > 0
    print(f"  downloading {source.url}", flush=True)
    if resumed:
        print(f"  resuming at {human_bytes(part.stat().st_size)}", flush=True)

    code = curl_once(curl, source, part, proxy)
    if code in RESUME_REFUSED_CODES and resumed:
        # The server does not honour `Range` (33) or the partial file does not
        # match it (36); a fresh download is the only way forward. Every other
        # failure keeps the `.part` so the next run resumes it.
        print(f"  curl exited {code} while resuming; restarting from zero", flush=True)
        part.unlink(missing_ok=True)
        code = curl_once(curl, source, part, proxy)
    if code != 0:
        print(f"error: curl exited {code} for {source.url}", file=sys.stderr)
        raise SystemExit(1)
    if not part.exists() or part.stat().st_size == 0:
        print(f"error: curl produced no data for {source.url}", file=sys.stderr)
        raise SystemExit(1)

    os.replace(part, target)


def sha256_of(path: Path) -> str:
    """The SHA256 of `path`, read in chunks."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def keep_existing(
    source: Source, target: Path, *, force: bool, verify_sha256: bool
) -> tuple[bool, str]:
    """Whether the file already on disk can be kept, and the line to print about it.

    A file at least as large as the recorded snapshot cannot be a truncated
    download, so it is kept even when it is larger: Geofabrik rebuilds its
    `-latest` extracts daily, and re-downloading 1.5 GB on every run because the
    upstream data moved on is worse than building from the newer snapshot.
    """
    if force:
        return False, "downloading again (--force)"
    if not target.exists():
        return False, "not present"
    size = target.stat().st_size
    if size < source.snapshot_bytes:
        return False, (
            f"present at {human_bytes(size)}, smaller than the "
            f"{human_bytes(source.snapshot_bytes)} snapshot; downloading again"
        )
    if verify_sha256 and sha256_of(target) != source.sha256:
        return False, (
            f"sha256 is not the recorded {source.sha256[:16]}...; downloading again"
        )
    if size == source.snapshot_bytes:
        return True, f"already present, {human_bytes(size)}; skipped"
    return True, (
        f"warning: {source.name} is {human_bytes(size)}, larger than the recorded "
        f"{human_bytes(source.snapshot_bytes)} snapshot; the upstream extract has "
        "been refreshed, so the counts in README.md no longer apply. Keeping the "
        "file as it is (pass --force to download it again)"
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Download the OSM sources of the Hormuz map asset.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="download every source again from zero, even when it is already the "
        "snapshot size",
    )
    parser.add_argument(
        "--proxy",
        default=proxy_from_environment(),
        help="proxy passed to curl (default: $HTTPS_PROXY, then $HTTP_PROXY)",
    )
    parser.add_argument(
        "--sha256",
        action="store_true",
        help="print every source's SHA256 and download a file again when its hash "
        "differs from the recorded snapshot",
    )
    arguments = parser.parse_args()

    curl = require_curl()

    decisions = [
        keep_existing(
            source,
            ASSET_DIR / source.name,
            force=arguments.force,
            verify_sha256=arguments.sha256,
        )
        for source in SOURCES
    ]
    pending = [
        source for source, (keep, _note) in zip(SOURCES, decisions) if not keep
    ]
    needed = sum(source.snapshot_bytes for source in pending)
    free = shutil.disk_usage(ASSET_DIR).free
    if pending:
        print(
            f"free space: {human_bytes(free)}; this run downloads at most "
            f"{human_bytes(needed)}",
            flush=True,
        )
        if free < MIN_FREE_BYTES or free < needed:
            print(
                f"error: need at least {human_bytes(max(MIN_FREE_BYTES, needed))} free "
                f"in {ASSET_DIR}, have {human_bytes(free)}",
                file=sys.stderr,
            )
            raise SystemExit(2)

    if arguments.proxy:
        print(f"proxy: {arguments.proxy}", flush=True)

    for index, (source, (keep, note)) in enumerate(
        zip(SOURCES, decisions), start=1
    ):
        target = ASSET_DIR / source.name
        print(f"[{index}/{len(SOURCES)}] {source.name}", flush=True)
        print(f"  {note}", flush=True)
        if keep:
            continue
        download(curl, source, target, arguments.proxy, fresh=arguments.force)
        size = target.stat().st_size
        print(f"  done, {human_bytes(size)}", flush=True)
        if size != source.snapshot_bytes:
            print(
                f"  warning: {source.name} is not the recorded snapshot size; the "
                "upstream data may be newer, so the counts in README.md no longer apply",
                flush=True,
            )

    print("\nsources in place:", flush=True)
    for source in SOURCES:
        target = ASSET_DIR / source.name
        size = target.stat().st_size
        note = "" if size == source.snapshot_bytes else "  (differs from snapshot)"
        print(f"  {source.name:34s} {human_bytes(size)}{note}", flush=True)
        if arguments.sha256:
            digest = sha256_of(target)
            verdict = "ok" if digest == source.sha256 else "DIFFERS from the snapshot"
            print(f"  {'':34s} sha256 {digest} ({verdict})", flush=True)

    print(f"\nnext: {BUILD_COMMAND}", flush=True)


if __name__ == "__main__":
    main()
