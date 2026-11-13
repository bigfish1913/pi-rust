"""Read-only, standard-library provenance gate; run with python -B verify.py.

No Git/network/external checkout or user session/config is accessed. This checks
recorded bytes, not authenticity against an independent remote trust root.
"""
import hashlib
import json
from pathlib import Path


def main():
    root = Path(__file__).resolve().parent
    metadata = json.loads((root / 'provenance.json').read_text(encoding='utf-8'))
    prior = json.loads((root.parent / 'provenance.json').read_text(encoding='utf-8'))
    assert metadata['schema'] == 'rpi-durable-offline-legacy-oracle-v1'
    assert metadata['legacyCommit'] == prior['legacyCommit']
    assert metadata['legacyRepository'] == prior['legacyRepository']
    reference = metadata['reference']
    assert reference['sourcePath'] == 'packages/rpi-durable-core/src/lib.rs'
    assert reference['sha256'] == prior['sources'][reference['sourcePath']]['sha256']
    rows = []
    for key in ('reference', 'license', 'fixture'):
        entry = metadata[key]
        name = entry.get('vendoredPath', entry.get('path'))
        assert name in ('reference.rs.txt', 'LICENSE', 'control-trace.json')
        data = (root / name).read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        assert digest == entry['sha256'], f'{key}: SHA-256 mismatch'
        if 'bytes' in entry:
            assert len(data) == entry['bytes'], f'{key}: byte count mismatch'
        rows.append({'file': name, 'bytes': len(data), 'sha256': digest})
    fixture = json.loads((root / 'control-trace.json').read_text(encoding='utf-8'))
    assert 'synthetic' in fixture['name'] and 'no real sessions' in fixture['name']
    assert len(fixture['frames']) == metadata['fixture']['frames'] == 17
    assert sum(frame.get('action') is not None for frame in fixture['frames']) == 16
    assert (root / 'LICENSE').read_text(encoding='utf-8').startswith('MIT License')
    print(json.dumps({'schema': metadata['schema'], 'legacyCommit': metadata['legacyCommit'],
                      'fixtureFrames': 17, 'artifacts': rows}, ensure_ascii=False))


if __name__ == '__main__':
    main()
