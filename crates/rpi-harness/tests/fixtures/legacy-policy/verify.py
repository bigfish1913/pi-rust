"""Read-only drift gate for the private legacy decoder prefix and MIT notice.

Normal invocation needs no other checkout/network. Optional root is for isolated
synthetic-copy negative probes, not an automatic source fetch or repair.
"""
import hashlib
import json
import os
from pathlib import Path
import sys

if not __debug__ or os.environ.get('PYTHONOPTIMIZE'):
    raise SystemExit('Run unoptimized Python with assertions enabled')

EXPECTED = {
    'sourceCommit': '041c981b21ac42348feee371757f5ef26ecbdcab',
    'sourcePath': 'packages/rpi-durable-core/src/lib.rs',
    'sourceSha256': '3de38799116a5a7be960d9b630d6676107b43375841fadda6b27d2bacadff352',
    'operation': 'Exact byte prefix before first #[cfg(test)], removing only upstream unit-test module',
    'decoderBytes': 19568,
    'decoderSha256': '09e48831346e7e10aa9659cbc7dd4e35947d744928f580a0955a0b95f4ddb560',
    'licenseSha256': 'd8d2baef7d67248adf749511d7ee59b4e960e8a56d0a921a9f7b0867b716e8b2',
    'scope': 'Private read-only legacy decoder; never native state authority, execution proof or import permit',
}

def verify(root):
    reference_dir = root / 'crates/rpi-durable/tests/fixtures/legacy-oracle'
    derived_dir = root / 'crates/rpi-harness/src/durable/legacy'
    metadata = json.loads((derived_dir / 'provenance.json').read_text(encoding='utf-8'))
    assert metadata == EXPECTED, 'provenance metadata changed'
    source = (reference_dir / 'reference.rs.txt').read_bytes()
    assert hashlib.sha256(source).hexdigest() == EXPECTED['sourceSha256'], 'reference changed'
    assert source.count(b'#[cfg(test)]') == 1, 'ambiguous source extraction boundary'
    decoder = (derived_dir / 'decoder.rs').read_bytes()
    assert decoder == source.split(b'#[cfg(test)]', 1)[0], 'decoder not exact source prefix'
    assert len(decoder) == EXPECTED['decoderBytes'], 'decoder length changed'
    assert hashlib.sha256(decoder).hexdigest() == EXPECTED['decoderSha256'], 'decoder hash changed'
    license_bytes = (derived_dir / 'LICENSE').read_bytes()
    assert license_bytes == (reference_dir / 'LICENSE').read_bytes(), 'license bytes changed'
    assert hashlib.sha256(license_bytes).hexdigest() == EXPECTED['licenseSha256'], 'license hash changed'
    print('PASS legacy decoder exact prefix/provenance/MIT license; no remote authentication claim')

if __name__ == '__main__':
    verify(Path(sys.argv[1]).resolve() if len(sys.argv) == 2 else Path(__file__).resolve().parents[5])
