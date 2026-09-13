#!/usr/bin/env bash
# Archive native sources only. Default: worktree; --ref: exact committed tree.
set -euo pipefail
version=""
source_ref=""
output=""
usage() {
    echo "Usage: $0 --version <version> [--ref <git-ref>] --out <archive.tar.gz>"
}
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version|--ref|--out)
            [[ $# -ge 2 && -n "$2" ]] || { usage >&2; exit 1; }
            case "$1" in
                --version) version="$2" ;;
                --ref) source_ref="$2" ;;
                --out) output="$2" ;;
            esac
            shift 2 ;;
        --help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done
[[ -n "$version" && -n "$output" ]] || { usage >&2; exit 1; }
export DUME_SOURCE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        DUME_SOURCE_ROOT="$(cygpath -m "$DUME_SOURCE_ROOT")"
        output="$(cygpath -m "$output")"
        ;;
esac
export DUME_SOURCE_VERSION="$version" DUME_SOURCE_REF="$source_ref" DUME_SOURCE_OUTPUT="$output"
"${PYTHON:-python3}" - <<'PY'
import gzip
import io
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tempfile
import tomllib

root = Path(os.environ['DUME_SOURCE_ROOT'])
version = os.environ['DUME_SOURCE_VERSION']
ref = os.environ['DUME_SOURCE_REF']
output = Path(os.environ['DUME_SOURCE_OUTPUT']).resolve()
if not re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?', version):
    raise SystemExit('Invalid release version')
if output.exists():
    raise SystemExit(f'Refusing to overwrite {output}')

def allowed(path):
    parts = PurePosixPath(path).parts
    if not parts or any(part in ('.git', 'target', 'node_modules', '__pycache__') for part in parts):
        return False
    return (path in ('Cargo.toml', 'Cargo.lock', 'scripts/build-binaries.sh', 'scripts/create-source-archive.sh',
                    'scripts/install.sh', 'scripts/test-native-install.py')
            or parts[0] in ('crates', 'licenses')
            or (len(parts) == 1 and parts[0].startswith(('LICENSE', 'NOTICE'))))

files = {}
if ref:
    commit = subprocess.check_output(['git', 'rev-parse', '--verify', '--end-of-options', ref + '^{commit}'], cwd=root, text=True).strip()
    timestamp = int(subprocess.check_output(['git', 'show', '-s', '--format=%ct', commit], cwd=root, text=True))
    archive = subprocess.check_output(['git', 'archive', '--format=tar', commit], cwd=root)
    with tarfile.open(fileobj=io.BytesIO(archive)) as source:
        for member in source:
            if allowed(member.name) and not member.isdir():
                if not member.isfile():
                    raise SystemExit(f'Non-regular source file: {member.name}')
                files[member.name] = (source.extractfile(member).read(), member.mode)
else:
    timestamp = 0
    candidates = [root / path for path in (
        'Cargo.toml', 'Cargo.lock', 'scripts/build-binaries.sh', 'scripts/create-source-archive.sh',
        'scripts/install.sh', 'scripts/test-native-install.py')]
    for pattern in ('crates/**/*', 'licenses/**/*', 'LICENSE*', 'NOTICE*'):
        candidates.extend(root.glob(pattern))
    for path in sorted(set(candidates)):
        relative = path.relative_to(root).as_posix()
        if allowed(relative) and not path.is_dir():
            if path.is_symlink() or not path.is_file():
                raise SystemExit(f'Missing or non-regular source file: {relative}')
            files[relative] = (path.read_bytes(), path.stat().st_mode & 0o777)

required = ('Cargo.toml', 'Cargo.lock', 'LICENSE', 'scripts/build-binaries.sh',
            'scripts/create-source-archive.sh', 'scripts/install.sh',
            'scripts/test-native-install.py', 'crates/dume-cli/src/main.rs',
            'crates/dume-provider/tests/mock_http_fixtures.rs')
for path in required:
    if path not in files:
        raise SystemExit(f'Missing required source: {path}')
manifest = tomllib.loads(files['Cargo.toml'][0].decode())
if manifest['workspace']['package']['version'] != version:
    raise SystemExit('Release version does not match Cargo.toml')
for member in manifest['workspace']['members']:
    if f'{member}/Cargo.toml' not in files:
        raise SystemExit(f'Missing workspace member: {member}')
output.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='dume-source-') as temporary:
    staged = Path(temporary) / 'source.tar.gz'
    with staged.open('wb') as raw, gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=timestamp) as compressed:
        with tarfile.open(fileobj=compressed, mode='w') as target:
            for path, (data, mode) in sorted(files.items()):
                info = tarfile.TarInfo(f'dume-{version}/{path}')
                info.size, info.mode, info.mtime = len(data), mode, timestamp
                target.addfile(info, io.BytesIO(data))
    # Validate the complete file inventory before installing the archive.
    with tarfile.open(staged) as check:
        if check.getnames() != [f'dume-{version}/{path}' for path in sorted(files)]:
            raise SystemExit('Source archive inventory mismatch')
    with output.open('xb') as destination, staged.open('rb') as source:
        import shutil
        shutil.copyfileobj(source, destination)
print(output)
PY
