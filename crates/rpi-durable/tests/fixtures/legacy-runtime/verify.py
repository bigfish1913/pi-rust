"""Read-only fixed legacy runtime fixture gate. No checkout/network required."""
import hashlib,json,os,pathlib,sys,tomllib
if not __debug__ or os.environ.get('PYTHONOPTIMIZE'):
    raise SystemExit('Run unoptimized Python with assertions enabled')
EXPECTED={
 'plugin.rs.txt':(109660,'0f4be632b1f5dab5dfa0ac9cf55bb9119b1a37ec3ac195f08161dca9480fdaac'),
 'runtime_contract.rs.txt':(17894,'8a9c04a88b7421e9544d1591069a7b227fc9e13e815a6ace86ecd02481978c3c'),
 'plugin.Cargo.toml.txt':(638,'35668eb351fd26dc65721b9e738145c21acc212ecf0f2fb5c608a102d91b4663'),
 'core.Cargo.toml.txt':(404,'61136e4c715ba69c7360df3aa7069a774f0676b8b4b3d8b9ac70a055689d984d'),
 'workspace.Cargo.toml.txt':(2027,'999f15d39cf6a14fa28deb93b6a2606472dcff38a18ba597b4a8936d4d8687ef'),
 'upstream.Cargo.lock.txt':(109835,'8b1ac3019094c4331e2b45722958bba5f3ac28669d8546bad2c95f3a669ea212'),
 'LICENSE':(1064,'d8d2baef7d67248adf749511d7ee59b4e960e8a56d0a921a9f7b0867b716e8b2'),
 'isolated.Cargo.lock.txt':(6184,'1ef433c1e5ae9a5590143553094ea2a4f4ca881d1ab3e6b427a8bfada759160b'),
 'callback_trace.rs.txt':(8432,'b8df586cff9bfca946b172a8fdae600ebbc98e81f6e5b2e88734ee59238833b2'),
}
def verify(fixture):
    metadata_bytes=(fixture/'provenance.json').read_bytes()
    assert hashlib.sha256(metadata_bytes).hexdigest()=='a8c312801e38b93b56ea13aeffa61a6b7b398f55a04155207433c18aadf6dc8f','provenance drift'
    metadata=json.loads(metadata_bytes)
    assert metadata['sourceRepository']=='https://github.com/pi-rust/rpi-package'
    assert metadata['sourceCommit']=='041c981b21ac42348feee371757f5ef26ecbdcab'
    assert metadata['sdk']=={'name':'rpi-plugin-sdk','version':'0.3.20','registryArchiveSha256':'88064b24428c5c5a5d3ee93616a754d171951fb8b31edbc55530433359047d85'}
    assert metadata['coreSource']=='../legacy-oracle/reference.rs.txt'
    declared={**metadata['files'],**metadata['derivedArtifacts']}
    assert set(declared)==set(EXPECTED)
    for name,(size,digest) in EXPECTED.items():
        data=(fixture/name).read_bytes()
        assert len(data)==size and hashlib.sha256(data).hexdigest()==digest,('artifact drift',name)
        assert declared[name]['bytes']==size and declared[name]['sha256']==digest,('metadata drift',name)
    assert hashlib.sha256((fixture.parent/'legacy-oracle/reference.rs.txt').read_bytes()).hexdigest()=='3de38799116a5a7be960d9b630d6676107b43375841fadda6b27d2bacadff352'
    assert (fixture/'runtime_contract.rs.txt').read_bytes().count(b'#[test]')==1
    old=tomllib.loads((fixture/'upstream.Cargo.lock.txt').read_text(encoding='utf-8'))['package']
    new=tomllib.loads((fixture/'isolated.Cargo.lock.txt').read_text(encoding='utf-8'))['package']
    assert len(new)==27
    for package in new:
        original=next(p for p in old if p['name']==package['name'] and p['version']==package['version'])
        assert package.get('source')==original.get('source') and package.get('checksum')==original.get('checksum')
        normalized={dep.split()[0] for dep in original.get('dependencies',[])}
        if package['name']=='rpi-plugin-sdk':normalized-={'libc','windows-sys'}
        if package['name']=='digest':normalized-={'subtle'}
        assert set(package.get('dependencies',[]))==normalized,package['name']
    assert [p['version'] for p in new if p['name']=='rpi-plugin-sdk']==['0.3.20']
    print('PASS fixed plugin/core/contract/MIT/SDK version and derived lock/test-body hashes; not remote source authentication')
if __name__=='__main__':
    verify(pathlib.Path(sys.argv[1]).resolve() if len(sys.argv)==2 else pathlib.Path(__file__).resolve().parent)
