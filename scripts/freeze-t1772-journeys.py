#!/usr/bin/env python3
"""Reproduce only frozen §9.3 expectations; never run product queries or P0.
Arguments: CORPUS_CUSTODY ORACLES HISTORICAL_COPIES HISTORICAL_LOGS
           REFERENCE_EXECUTABLE NORMALIZER_EXECUTABLE NEW_OUTPUT
Build the two named examples from this clean source before invoking this script.
"""
import csv, hashlib, json, subprocess, sys
from pathlib import Path
source=Path(__file__).resolve().parent.parent
corpus,oracles,historical,logs,reference,normalizer,out=map(lambda s:Path(s).resolve(),sys.argv[1:])
out.mkdir();commands=[]
def sha(p):
 h=hashlib.sha256()
 with p.open('rb') as f:
  for b in iter(lambda:f.read(1024*1024),b''):h.update(b)
 return h.hexdigest()
def dump(p,v):p.write_text(json.dumps(v,sort_keys=True,separators=(',',':'))+'\n')
def run(argv,label):
 argv=list(map(str,argv));record={'argv':argv,'cwd':str(source),'stdout_stderr':label+'.log'};commands.append(record)
 with (out/(label+'.log')).open('wb') as f:r=subprocess.run(argv,cwd=source,stdout=f,stderr=subprocess.STDOUT)
 record['exit']=r.returncode;record['log_sha256']=sha(out/(label+'.log'));r.check_returncode()
def command(*args):return ['python3',source/'scripts'/args[0],*args[1:]]
assert sha(corpus/'frozen-p0-tape-ids.txt')=='29800d70e6812b446581c1f505ddd1b78c25effcd76a8c467c892553913f5757'
assert sha(corpus/'ordered-corpus-manifest.tsv')=='932dbdd5968a5582e6f3c04519946df90735cc0858b4d7247c7adf7bd2b233bb'
ids=(corpus/'frozen-p0-tape-ids.txt').read_text().splitlines();assert len(ids)==len(set(ids))==45758
rows=list(csv.DictReader((corpus/'ordered-corpus-manifest.tsv').open(),delimiter='\t'))
assert [r['tape_id'] for r in rows]==ids
for r in rows:
 p=corpus/'corpus'/(r['tape_id']+'.jsonl.zst');assert p.stat().st_size==int(r['compressed_size']) and sha(p)==r['sha256']
assert sorted(p.name for p in (corpus/'corpus').iterdir())==sorted(i+'.jsonl.zst' for i in ids)
for name,wanted in [('p0-performance-query-manifest.json','6004d054e1e962cec1e869bda97c590a1704dd268f1bc470d3f7423313f079f0'),('p0-performance-expected-direct-touches.json','417d3aeabaf80a2196b51c3f0b7dad5381903a06d4017b7960410203100d3b31'),('dispatch-oracle.jsonl','de556156054c6299407b0e61efaff8e59a0c2bc67c1b5d172d22f85817eac661')]:assert sha(oracles/name)==wanted
fixtures=out/'fixtures';normalized=out/'normalized'
run(command('t1772-fixed-fixtures.py',fixtures),'fixed-inputs')
run([normalizer,source,historical,normalized],'normalize-twice')
run(command('t1772-normalized-groups.py',fixtures,normalized,logs),'normalized-inputs')
def derive(p,mode,corpus_,ids_,queries,direct,dispatch):
 run([reference,corpus_,ids_,queries,p/'records.jsonl'],p.name+'-reference')
 run(command('derive-t1772-journeys.py','--mode',mode,'--records',p/'records.jsonl','--corpus',corpus_,'--queries',queries,'--oracle',direct,'--dispatch',dispatch,'--output',p/'expected'),p.name+'-expectations')
for g in json.loads((fixtures/'groups-with-normalized.json').read_bytes()):
 p=fixtures/g['group'];derive(p,'P1',p/'corpus',p/'ids.txt',p/'queries.json',p/'direct.json',p/'dispatch.jsonl')
p=fixtures/'reingest/before';derive(p,'P1',p/'corpus',p/'ids.txt',p/'queries.json',p/'direct.json',p/'dispatch.jsonl')
p=out/'P0';p.mkdir();derive(p,'P0',corpus/'corpus',corpus/'frozen-p0-tape-ids.txt',oracles/'p0-performance-query-manifest.json',oracles/'p0-performance-expected-direct-touches.json',oracles/'dispatch-oracle.jsonl')
receipt={'schema':'t1772-section93-derivation-v1','source_revision':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'source_tree':subprocess.check_output(['git','rev-parse','HEAD^{tree}'],cwd=source,text=True).strip(),'source_clean':not subprocess.check_output(['git','status','--porcelain'],cwd=source),'product_query_execution':False,'corpus_blobs_verified':45758,'compressed_bytes':sum(int(r['compressed_size']) for r in rows),'corpus_root':str(corpus),'corpus_manifest_sha256':sha(corpus/'ordered-corpus-manifest.tsv'),'ids_sha256':sha(corpus/'frozen-p0-tape-ids.txt'),'executables':[{'path':str(p),'sha256':sha(p)} for p in [reference,normalizer,Path('/usr/bin/zstd')]],'python_version':sys.version,'commands':commands,'p0_reference_sha256':sha(out/'P0/records.jsonl'),'p0_component_manifest_sha256':sha(out/'P0/expected/manifest.json')}
assert receipt['source_clean'],'freeze requires clean committed source'
dump(out/'derivation-receipt.json',receipt)
run(command('package-t1772-journeys.py',fixtures,out/'P0/expected',normalized,source,out/'journey-inputs',out/'derivation-receipt.json'),'package')
print(json.dumps({'package':str(out/'journey-inputs'),'manifest_sha256':sha(out/'journey-inputs/manifest.json'),'derivation_receipt_sha256':sha(out/'derivation-receipt.json')},sort_keys=True))
