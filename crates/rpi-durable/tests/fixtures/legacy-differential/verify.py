"""Read-only gate for the explicit old-plugin/native-service test bridge."""
import hashlib,os,pathlib,runpy,sys
if not __debug__ or os.environ.get('PYTHONOPTIMIZE'):
    raise SystemExit('Run unoptimized Python with assertions enabled')
EXPECTED={
 'plugin_trace.rs.txt':'3764009f053b8f58c8e72733e642d545f41b9f31893211b2b95609a5a842036b',
 'run.py':'44445800f7b92461409096ecad5aee3ca5038c08948ecc322208575397a70075',
}
NATIVE_SHA='86fc57ab661667a2ad5d76ed7e90bbb91430c43b2eb3222fad510fb9062d71b1'
def verify(fixture):
    old=fixture.parent/'legacy-runtime'
    # Existing fixed gates are executed read-only, not patched for this slice.
    runpy.run_path(str(old/'verify.py'))['verify'](old)
    oracle=fixture.parent/'legacy-oracle'
    assert hashlib.sha256((oracle/'control-trace.json').read_bytes()).hexdigest()=='3aa91d01e3e92a10b34b4e89a923802537437f6076b359edb2d2b4bfad050343'
    for name,digest in EXPECTED.items():
        assert hashlib.sha256((fixture/name).read_bytes()).hexdigest()==digest,('bridge drift',name)
    native=fixture.parents[4]/'crates/rpi-harness/tests/durable_legacy_differential.rs'
    assert hashlib.sha256(native.read_bytes()).hexdigest()==NATIVE_SHA,'native bridge drift'
    print('PASS fixed old runtime/literal trace and bridge source hashes; artifacts remain non-authorizing synthetic data')
if __name__=='__main__':
    verify(pathlib.Path(sys.argv[1]).resolve() if len(sys.argv)==2 else pathlib.Path(__file__).resolve().parent)
