"""Offline isolated execution only. No normal host/extension loading.

Uses exact old plugin/core/manifests/SDK, with a two-member derived workspace and
transitive closure lock. Synthetic host HOME/config is a TempDir. Cached Cargo /
Rustup configuration, installed toolchain and OS discovery remain trusted inputs;
offline is not a sandbox or a guarantee those tools cannot access credentials.
"""
import hashlib,json,os,pathlib,re,shutil,subprocess,sys,tempfile,time
assert __debug__ and not os.environ.get('PYTHONOPTIMIZE')
import argparse,tomllib
from verify import verify
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--log-dir',type=pathlib.Path,required=True)
options=parser.parse_args()
fixture=pathlib.Path(__file__).resolve().parent
logs=options.log_dir.resolve();logs.mkdir(parents=True,exist_ok=True)
prefix='pi-rust-durable-core-legacy-runtime-'
verify(fixture)
metadata=json.loads((fixture/'provenance.json').read_text(encoding='utf-8'))
with tempfile.TemporaryDirectory(prefix='synthetic-legacy-runtime-') as directory:
 temp=pathlib.Path(directory);work=temp/'workspace';work.mkdir()
 old=(fixture/'workspace.Cargo.toml.txt').read_text(encoding='utf-8')
 start=old.index('members = [');end=old.index('\n]',start)+2
 (work/'Cargo.toml').write_text(old[:start]+'members = ["packages/rpi-durable", "packages/rpi-durable-core"]'+old[end:],encoding='utf-8')
 shutil.copyfile(fixture/'isolated.Cargo.lock.txt',work/'Cargo.lock')
 copies={
  'plugin.Cargo.toml.txt':'packages/rpi-durable/Cargo.toml',
  'plugin.rs.txt':'packages/rpi-durable/src/lib.rs',
  'runtime_contract.rs.txt':'packages/rpi-durable/tests/runtime_contract.rs',
  'core.Cargo.toml.txt':'packages/rpi-durable-core/Cargo.toml',
 }
 for source,destination in copies.items():
  p=work/destination;p.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(fixture/source,p)
 p=work/'packages/rpi-durable-core/src/lib.rs';p.parent.mkdir(parents=True);shutil.copyfile(fixture.parent/'legacy-oracle/reference.rs.txt',p)
 upstream=(fixture/'runtime_contract.rs.txt').read_bytes();assert upstream.count(b'#[test]')==1
 callback=upstream.split(b'#[test]',1)[0]+(fixture/'callback_trace.rs.txt').read_bytes()
 (work/'packages/rpi-durable/tests/callback_trace.rs').write_bytes(callback)
 for sub in ['rpi-durable','rpi-durable-core']:shutil.copyfile(fixture/'LICENSE',work/'packages'/sub/'LICENSE')
 keep=('ProgramFiles','ProgramFiles(x86)','ProgramW6432','ProgramData','ALLUSERSPROFILE','PATH','SystemRoot','WINDIR','COMSPEC','PATHEXT','TEMP','TMP','LIB','LIBPATH','INCLUDE','VSINSTALLDIR','VCINSTALLDIR','VCToolsInstallDir','WindowsSdkDir','WindowsSDKVersion','NUMBER_OF_PROCESSORS','PROCESSOR_ARCHITECTURE')
 env={k:os.environ[k] for k in keep if k in os.environ}
 profile=pathlib.Path(os.environ.get('USERPROFILE','C:/Users/bigfish'))
 env['CARGO_HOME']=os.environ.get('CARGO_HOME',str(profile/'.cargo'));env['RUSTUP_HOME']=os.environ.get('RUSTUP_HOME',str(profile/'.rustup'))
 for key,sub in [('HOME','home'),('USERPROFILE','home'),('APPDATA','config'),('LOCALAPPDATA','data'),('XDG_CONFIG_HOME','config'),('XDG_DATA_HOME','data'),('RPI_CODING_AGENT_DIR','agent'),('RPI_CODING_AGENT_SESSION_DIR','sessions')]:
  (temp/sub).mkdir(exist_ok=True);env[key]=str(temp/sub)
 env['CARGO_TARGET_DIR']='D:/BuildCache/cargo/00/f71dba7f035f44'
 # Cargo can separately relocate intermediates with build.build-dir. Pin both
 # directories so a random TempDir workspace cannot choose another cache shard.
 env['CARGO_BUILD_BUILD_DIR']=env['CARGO_TARGET_DIR']
 # Verify cached archives against upstream-lock checksums without reading any
 # provider/session/config content. Cargo also verifies its registry source.
 locked=tomllib.loads((fixture/'isolated.Cargo.lock.txt').read_text(encoding='utf-8'))['package']
 cache=pathlib.Path(env['CARGO_HOME'])/'registry/cache'
 for package in locked:
  if 'checksum' not in package:continue
  archives=list(cache.glob('*/'+package['name']+'-'+package['version']+'.crate'))
  assert archives,('offline archive missing',package['name'],package['version'])
  assert all(hashlib.sha256(p.read_bytes()).hexdigest()==package['checksum'] for p in archives),('archive checksum mismatch',package['name'])
 before=(work/'Cargo.lock').read_bytes()
 info=subprocess.run(['cargo','metadata','--manifest-path',str(work/'Cargo.toml'),'--locked','--offline','--no-deps','--format-version','1'],cwd=fixture.parents[4],env=env,capture_output=True)
 assert info.returncode==0,info.stderr.decode('utf-8',errors='replace')
 actual_target=json.loads(info.stdout)['target_directory']
 print('OBSERVED_CARGO_TARGET',actual_target,flush=True)
 assert pathlib.Path(actual_target).resolve()==pathlib.Path(env['CARGO_TARGET_DIR']).resolve(),'Cargo target mapping differs from authorized cache; stop before tests'
 stamp=time.strftime('%Y%m%d-%H%M%S');path=logs/(prefix+'upstream-'+stamp+'.log')
 print('START',path,flush=True)
 with path.open('wb') as output:
  process=subprocess.Popen(['cargo','test','--manifest-path',str(work/'Cargo.toml'),'--locked','--offline','--target-dir',env['CARGO_TARGET_DIR'],'--workspace','--','--test-threads=1'],cwd=fixture.parents[4],env=env,stdout=output,stderr=subprocess.STDOUT)
  (logs/(prefix+'upstream-'+stamp+'-pids.json')).write_text(json.dumps({'runnerPid':os.getpid(),'cargoPid':process.pid,'started':stamp}),encoding='utf-8')
  code=process.wait()
 assert (work/'Cargo.lock').read_bytes()==before,'locked cargo changed fixture lock'
 for source,destination in copies.items():assert(work/destination).read_bytes()==(fixture/source).read_bytes()
 assert (work/'packages/rpi-durable/tests/callback_trace.rs').read_bytes()==callback
 assert (work/'packages/rpi-durable-core/src/lib.rs').read_bytes()==(fixture.parent/'legacy-oracle/reference.rs.txt').read_bytes()
 executable_paths=re.findall(r'Running [^\r\n]+ \(([^\r\n]+\.exe)\)',path.read_bytes().decode('utf-8',errors='replace'))
 assert len(executable_paths)==4 if code==0 else True
 assert all(p.replace('\\','/').startswith(env['CARGO_TARGET_DIR']+'/') for p in executable_paths),'test executable escaped pinned intermediate build directory'
 result={'exit':code,'log':str(path),'logSha256':hashlib.sha256(path.read_bytes()).hexdigest(),
  'observedCargoTarget':actual_target,'observedTestExecutables':executable_paths,'cargoIntermediateBuildDir':env['CARGO_BUILD_BUILD_DIR'],
  'lockedArtifactSha256':hashlib.sha256(before).hexdigest(),'assembledCallbackSha256':hashlib.sha256(callback).hexdigest(),
  'copiedSourceBytesUnchanged':True,'isolatedHomeConfig':True,'providerCredentialEnvironmentNotInherited':True,
  'usesTrustedCargoRustupCacheAndToolchain':True,
  'cargoTargetRequested':env['CARGO_TARGET_DIR'],'cachedArchiveChecks':len([p for p in locked if 'checksum' in p])}
 (logs/(prefix+'upstream-'+stamp+'-result.json')).write_text(json.dumps(result,indent=2),encoding='utf-8')
 print('END',json.dumps(result),flush=True)
 sys.exit(code)
