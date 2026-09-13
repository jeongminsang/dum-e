#!/usr/bin/env bash
# Build, archive, and smoke-test the current native platform. No provider calls.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
output="$repo_root/dist-bin"
platform=""
offline=false
usage() {
    echo "Usage: $0 [--platform <native-platform>] [--out <dir>] [--offline]"
}
while [[ $# -gt 0 ]]; do
    case "$1" in
        --platform|--out)
            [[ $# -ge 2 && -n "$2" ]] || { usage >&2; exit 1; }
            if [[ "$1" == --platform ]]; then platform="$2"; else output="$2"; fi
            shift 2 ;;
        --offline) offline=true; shift ;;
        --help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done
mkdir -p "$output"
output="$(cd "$output" && pwd)"
cd "$repo_root"
python="${PYTHON:-python3}"
host="$(rustc -vV | "$python" -c 'import sys; print(next(line.split(": ", 1)[1].strip() for line in sys.stdin.read().splitlines() if line.startswith("host: ")))')"
case "$host" in
    aarch64-apple-darwin) native=darwin-arm64 ;;
    x86_64-apple-darwin) native=darwin-x64 ;;
    aarch64-unknown-linux-gnu) native=linux-arm64 ;;
    x86_64-unknown-linux-gnu) native=linux-x64 ;;
    aarch64-pc-windows-msvc) native=windows-arm64 ;;
    x86_64-pc-windows-msvc) native=windows-x64 ;;
    *) echo "Unsupported native Rust host: $host" >&2; exit 1 ;;
esac
[[ -z "$platform" || "$platform" == "$native" ]] || {
    echo "Cannot smoke-test $platform on $native; use a native runner." >&2; exit 1;
}
platform="$native"
# Explicit target directory avoids ambient CARGO_TARGET_DIR changing the artifact path.
cargo_args=(--locked --target "$host" --target-dir "$repo_root/target")
if [[ "$offline" == true ]]; then cargo_args+=(--offline); fi
cargo test --workspace "${cargo_args[@]}"
cargo build --release -p dume-cli --bin dume "${cargo_args[@]}"
if [[ "$platform" == windows-* ]]; then
    output="$(cygpath -m "$output")"
fi
export DUME_ARCHIVE_PLATFORM="$platform" DUME_ARCHIVE_OUTPUT="$output" DUME_BUILD_HOST="$host"
"$python" - <<'PY'
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

root = Path.cwd()
platform = os.environ['DUME_ARCHIVE_PLATFORM']
output = Path(os.environ['DUME_ARCHIVE_OUTPUT'])
version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']
name = 'dume.exe' if platform.startswith('windows-') else 'dume'
assets = sorted({p for pattern in ('LICENSE*', 'NOTICE*', 'licenses') for p in root.glob(pattern)})
if not (root / 'LICENSE').is_file():
    raise SystemExit('Missing LICENSE')
with tempfile.TemporaryDirectory(prefix='dume-release-') as temporary:
    temporary = Path(temporary)
    stage = temporary / 'dume'
    stage.mkdir()
    shutil.copy2(root / 'target' / os.environ['DUME_BUILD_HOST'] / 'release' / name, stage / name)
    for asset in assets:
        if asset.is_dir():
            shutil.copytree(asset, stage / asset.name)
        else:
            shutil.copy2(asset, stage / asset.name)
    archive = output / ('dume-' + platform + ('.zip' if name.endswith('.exe') else '.tar.gz'))
    if archive.exists():
        raise SystemExit(f'Refusing to overwrite {archive}')
    if name.endswith('.exe'):
        with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as handle:
            for path in sorted(stage.rglob('*')):
                if path.is_file():
                    handle.write(path, path.relative_to(temporary))
    else:
        with tarfile.open(archive, 'w:gz') as handle:
            handle.add(stage, arcname='dume')
    extracted = temporary / 'extracted'
    extracted.mkdir()
    if name.endswith('.exe'):
        with zipfile.ZipFile(archive) as handle:
            handle.extractall(extracted)
    else:
        with tarfile.open(archive) as handle:
            handle.extractall(extracted, filter='data')
    binary = extracted / 'dume' / name
    # Run outside the checkout; only clap help/version paths, never model requests.
    subprocess.run([str(binary), '--help'], cwd=temporary, check=True)
    actual = subprocess.check_output([str(binary), '--version'], cwd=temporary, text=True).strip()
    if actual != f'dume {version}':
        raise SystemExit(f'Unexpected binary version: {actual!r}')
    with archive.open('rb') as content:
        digest = hashlib.file_digest(content, 'sha256').hexdigest()
    archive.with_name(archive.name + '.sha256').write_text(f'{digest}  {archive.name}\n')
    if not name.endswith('.exe'):
        subprocess.run([os.environ.get('PYTHON', 'python3'),
                        str(root / 'scripts/test-native-install.py'),
                        '--archive', str(archive), '--version', version], check=True)
    print(archive)
PY
