"""Read-only fixed synthetic old-runtime capture gate; no fetch or repair."""
import hashlib,json,os,pathlib,sys
if not __debug__ or os.environ.get('PYTHONOPTIMIZE'):
    raise SystemExit('Run unoptimized Python with assertions enabled')
CAPTURE_SHA='43433f593bbf44140b4d02aa25373d510dc798b29662334aa0933867e5528760'
PROVENANCE_SHA='dca63ea119b984ec7b4af622b6f20973bc84a7d24452a07b3f18d25b40e51715'
def verify(fixture):
    data=(fixture/'captured-control.json').read_bytes()
    assert len(data)==13278 and hashlib.sha256(data).hexdigest()==CAPTURE_SHA,'capture bytes changed'
    metadata=(fixture/'provenance.json').read_bytes()
    assert hashlib.sha256(metadata).hexdigest()==PROVENANCE_SHA,'capture provenance changed'
    capture=json.loads(data);provenance=json.loads(metadata)
    assert capture['schema']=='rpi-synthetic-legacy-control-differential-v1'
    assert capture['legacyCommit']==provenance['sourceCommit']=='041c981b21ac42348feee371757f5ef26ecbdcab'
    assert capture['pluginSha256']==provenance['pluginSha256']=='0f4be632b1f5dab5dfa0ac9cf55bb9119b1a37ec3ac195f08161dca9480fdaac'
    assert capture['traceSha256']==provenance['traceSha256']=='3aa91d01e3e92a10b34b4e89a923802537437f6076b359edb2d2b4bfad050343'
    assert len(capture['frames'])==17 and len(capture['entries'])==16
    assert capture['entries'][0]['data']['kind']=='snapshot' and capture['entries'][0]['data']['revision']==1
    assert all(frame['state']['nextRunId']==0 and frame['state'].get('run') is None for frame in capture['frames'])
    assert provenance['capture']['sha256']==CAPTURE_SHA and provenance['capture']['bytes']==13278
    print('PASS immutable #32 actual synthetic old-plugin capture and provenance; not fresh execution, remote authentication or import authorization')
if __name__=='__main__':
    verify(pathlib.Path(sys.argv[1]).resolve() if len(sys.argv)==2 else pathlib.Path(__file__).resolve().parent)
