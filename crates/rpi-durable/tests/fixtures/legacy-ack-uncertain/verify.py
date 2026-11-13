"""Read-only exact old-source and synthetic ACK probe gate, not provenance authentication."""
import hashlib,os,pathlib,runpy,sys
if not __debug__ or os.environ.get('PYTHONOPTIMIZE'):
    raise SystemExit('Run unoptimized Python with assertions enabled')
EXPECTED={'../../../../rpi-harness/tests/durable_ack_uncertain_differential.rs': '112d4f82e0ca1538c484ae467e9c64d5a7c6a6b6ff85a6b06f742e154d4752a4', 'captured-probe.json': 'd016c1d32b34eb1cff5d359bf6d7232cd270829b6c264074235fdcd293da5608', 'cases.json': '2db451f05628e51f75b08795c91cc9d715f866064623846f1da9a2f25ef2c791', 'plugin_ack.rs.txt': 'e190760d59d5d274188bc4fef6aadfa71691f5382bdd92bf71afedebc728de8f', 'run.py': 'eb10594f5d461a00a548fcc054e30dad471b2e1bf1d7ce48554bdf0d169c2bb9'}

def verify(fixture):
    old=fixture.parent/'legacy-runtime'
    runpy.run_path(str(old/'verify.py'))['verify'](old)
    for name,digest in EXPECTED.items():
        assert hashlib.sha256((fixture/name).read_bytes()).hexdigest()==digest,('ACK bridge drift',name)
    print('PASS exact old source, ACK cases, retained probe, exporter, runner and native bridge; old-only is not paired native coverage')
if __name__=='__main__':
    verify(pathlib.Path(sys.argv[1]).resolve() if len(sys.argv)==2 else pathlib.Path(__file__).resolve().parent)
