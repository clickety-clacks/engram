#!/usr/bin/env python3
"""Assemble the explicit §9.3 cases; no product execution or inferred expected answers."""
import hashlib,json,shutil,sys
from pathlib import Path
fixtures,p0,normalized,source,out,provenance=map(lambda x:Path(x).resolve(),sys.argv[1:])
out.mkdir();sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def dump(p,v):p.parent.mkdir(parents=True,exist_ok=True);p.write_text(json.dumps(v,ensure_ascii=False,sort_keys=True,separators=(',',':'))+'\n')
def copy(p,rel):d=out/rel;d.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(p,d);return rel

def cases(p,prefix):
 result=[]
 copy(p/'manifest.json',prefix+'/derivation-manifest.json')
 for c in json.loads((p/'manifest.json').read_bytes())['cases']:
  c=dict(c);old=p/c['expected'];assert sha(old)==c['expected_sha256'];c['expected']=copy(old,prefix+'/'+old.name);result.append(c)
 return result
p0cases=cases(p0,'P0/expected')
groups=[{'id':'P0','cases':p0cases,'target_files':[],'additional_store':False,'reingest':None,'tapes':[],'coverage':['canonical-event-identity','raw-windows-offsets','ordering','complete-P0-lineage','dispatch-absence-or-chain'],'input_ids_sha256':'29800d70e6812b446581c1f505ddd1b78c25effcd76a8c467c892553913f5757','query_manifest_sha256':'6004d054e1e962cec1e869bda97c590a1704dd268f1bc470d3f7423313f079f0'}]
for g in json.loads((fixtures/'groups-with-normalized.json').read_bytes()):
 name=g['group'];p=fixtures/name;group={'id':name,'cases':cases(p/'expected',name+'/expected'),'additional_store':g['additional_store'],'reingest':None,'target_files':[],'tapes':[],'source_derivation':g['source'],'normalization':g.get('normalization_custody')}
 for t in g['tapes']:
  tape=t['tape_id'];compressed=p/'corpus'/(tape+'.jsonl.zst');assert sha(compressed)==t['compressed_sha256']
  raw=p/t['raw'];assert sha(raw)==tape
  group['tapes'].append({'id':tape,'compressed':copy(compressed,name+'/corpus/'+compressed.name),'raw':copy(raw,name+'/'+t['raw']),'deferred':name=='reingest' and t['label']=='restart','store':('additional' if t['label']=='edit' else 'both') if g['additional_store'] else 'primary'})
  if g['reingest']==t['label'] or (name=='reingest' and t['label']=='restart'):
   b=raw.read_bytes();expected={'status':'ok','tape_id':tape,'path':'${CASE_ROOT}/.engram/tapes/'+compressed.name,'event_count':len(b.splitlines()),'uncompressed_bytes':len(b),'compressed_bytes':compressed.stat().st_size,'already_exists':t['label']!='restart','already_indexed':t['label']!='restart','tape_file_exists':t['label']!='restart','meta':None,'record':{'mode':'stdin'}}
   step='restart' if t['label']=='restart' else 'reingest'
   rel=name+'/'+step+'-expected.json';dump(out/rel,expected);group[step]={'raw':name+'/'+t['raw'],'tape_id':tape,'expected':rel}
 if name=='reingest':group['before_cases']=cases(p/'before/expected',name+'/before/expected')
 for path,entry in g['files'].items():group['target_files'].append({'target':path,'source':copy(p/entry['path'],name+'/'+entry['path'])})
 if name.startswith('normalized-') and not name.endswith('-session'):
  t=g['tapes'][0];lines=(p/t['raw']).read_text().splitlines();rows=[json.loads(l) for l in lines];tape=t['tape_id']
  expected={'query':{'command':'peek','session_id':tape,'start':1,'lines':len(lines),'before':None,'after':None,'grep_filter':None},'session':{'session_id':tape,'timestamp':max(r['t'] for r in rows),'window_start':1,'window_end':len(lines),'total_lines':len(lines),'content':[{'line':i+1,'text':line} for i,line in enumerate(lines)]}}
  rel=name+'/expected/complete-normalized-raw.json';dump(out/rel,expected);group['cases'].append({'id':name+'-complete-raw','argv':['peek',tape,'--start','1','--lines',str(len(lines))],'expected':rel,'expected_sha256':sha(out/rel),'exit':0,'coverage':['P1-normalized-raw-event-order','coverage-grades','structured-event-path-range-text'],'exclusions':[]})
 groups.append(group)
copy(provenance,'provenance/derivation-receipt.json')
copy(normalized/'normalization.json','provenance/normalization.json')
for record in json.loads((normalized/'normalization.json').read_bytes()):
 path=Path(record['raw_source']);assert sha(path)==record['raw_sha256'];copy(path,'raw-sources/'+record['name']+path.suffix)
for rel in ['examples/t1772_journey_reference.rs','examples/t1772_freeze_normalized.rs','scripts/derive-t1772-journeys.py','scripts/t1772-fixed-fixtures.py','scripts/t1772-normalized-groups.py','scripts/package-t1772-journeys.py','scripts/freeze-t1772-journeys.py','src/anchor/mod.rs','src/anchor/winnow.rs','src/ingest/mod.rs','src/tape/compress.rs','tests/cli_e2e.rs','tests/ingest_e2e.rs','tests/dispatch_marker_e2e.rs','tests/t1772_adapter_contract.rs','tests/fixtures/t1772/sha256-manifest.json']:
 copy(source/rel,'derivation-source/'+rel)
metadata={'schema':'t1772-section93-package-v1','contract_sha256':'29fd50d40d56860b6c3619b53ba342c7afc6b06d0b0b3b005420d4efbe62db83','complete':True,'config_template':'db: ${DB}\ntapes_dir: ${TAPES}\nexplain:\n  default_limit: 10\npeek:\n  default_lines: 30\n  default_before: 30\n  default_after: 10\n  grep_context: 5\n','additional_config_template':'additional_stores:\n  - ${ADDITIONAL_DB}\n','runtime_status':'NOT_EXECUTED; no full section 9.3 pass claim','groups':groups,'normalization_policy':'P1/repaired candidate-normalized frozen inputs; repeated conversion equality and raw pre/post hash receipt; query expectations independently derived from those inputs, never candidate query output','p0_policy':'unchanged full 45758-tape P0; standalone record derivation; all 12 complete explain JSONs plus exact raw peek outputs; existing exhaustive tombstone/canonical oracle gates remain mandatory','p0_span_links':0,'synthetic_span_link_positive_group':'forensics-span','comparison_policy':'complete JSON semantic equality, array order/multiplicity and all fields retained; object-key serialization order is nonsemantic','stderr_policy':'raw stderr retained; successful query stdout is the complete product JSON; on expected failure stdout must be empty and exactly one stderr JSON object must match; config:/db: diagnostic lines bind staging paths and are not product result JSON','historical_boundary':'two expressly routed Codex raw copies only; no claim of complete historical raw availability for other adapters','derivation_rules':'reference never opens SQLite or invokes product queries; only source-hash-bound canonical fingerprint primitive shared; frozen direct projection checked independently before full P0 expectations','files':[],'derivation_receipt':'provenance/derivation-receipt.json','derivation_receipt_sha256':sha(provenance),'case_serialization':'UTF-8 compact JSON sorted object keys with one final LF; arrays remain ordered; no semantic exclusions'}
metadata['files']=[{'path':str(p.relative_to(out)),'bytes':p.stat().st_size,'sha256':sha(p)} for p in sorted(out.rglob('*')) if p.is_file()]
metadata['io_plan']={'P0_queries_per_rebuild':len(p0cases),'P0_full_database_hashes_both_rebuilds':4*len(p0cases),'P0_hash_bytes_formula':f'{4*len(p0cases)} * actual_candidate_database_bytes','P0_extra_database_copies':0,'fixture_primary_databases_per_rebuild':len(groups)-1,'fixture_additional_databases_per_rebuild':sum(g['additional_store'] for g in groups),'policy':'§9.3 per-query pre/post full database hash and sidecar/listing custody; no P0 corpus copy; no changes to accepted §9.4 custody placement'}
dump(out/'manifest.json',metadata)
print(json.dumps({'manifest_sha256':sha(out/'manifest.json'),'groups':len(groups),'cases':sum(len(g['cases'])+len(g.get('before_cases',[])) for g in groups),'files':len(metadata['files']),'input_bytes':sum(f['bytes'] for f in metadata['files']),'io_plan':metadata['io_plan']},sort_keys=True))
