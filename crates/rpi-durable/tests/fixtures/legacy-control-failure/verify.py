"""Read-only source gate for synthetic old control failures; no import authority."""
import hashlib,os,pathlib,runpy,sys
if not __debug__ or os.environ.get('PYTHONOPTIMIZE'):
    raise SystemExit('Run unoptimized Python with assertions enabled')
EXPECTED={
 '../../../../rpi-harness/tests/durable_control_failure_differential.rs':'f7ef5730e665fe0a66b8d20d31dbf95040097aaedf0d1505c68d620ff0f6096f',
 'captured-probe.json':'6e6b5c131db19f9ebdfc09d203af62c7d7843d98e38b65207531ff87785f2ef5',
 'plugin_failure.rs.txt':'383f3bbb29416dfde8bb48b6b77276fa978e7d4ef13ec8eb462d9d449f4ac08c',
 'cases.json':'4eace46c0dd0353b8c91e4e10b60beb5b5afa880829592e82b031c98ab25b6f5',
 'run.py':'f2a33939ee637faceda096288d7226d2be0c8c5e1c0cf335c36a884e6aaca79d',
}
def verify(fixture):
    old=fixture.parent/'legacy-runtime'
    runpy.run_path(str(old/'verify.py'))['verify'](old)
    for name,digest in EXPECTED.items():
        assert hashlib.sha256((fixture/name).read_bytes()).hexdigest()==digest,('failure bridge drift',name)
    print('PASS fixed old source, synthetic cases, retained wire probe, exporter, runner and native bridge; old-only probe is not paired native coverage')
if __name__=='__main__':
    verify(pathlib.Path(sys.argv[1]).resolve() if len(sys.argv)==2 else pathlib.Path(__file__).resolve().parent)
