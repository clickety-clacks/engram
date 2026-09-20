from pathlib import Path
import hashlib,json,shutil,re,sys
r=Path(sys.argv[1]).resolve();n=Path(sys.argv[2]).resolve();historical_logs=Path(sys.argv[3]).resolve();groups=json.loads((r/'groups.json').read_bytes())
flags={'depth':10,'forensics':False,'include_deleted':False,'max_edges':500,'max_fanout':50,'min_confidence':0.5,'anchor':False}
def dispatch(rows,tape):
 def markers(v):
  if isinstance(v,str):return set(re.findall(r'<engram-src id="([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})"/>',v.replace('\\"','"')))
  if isinstance(v,list):return set().union(*(markers(x) for x in v)) if v else set()
  if isinstance(v,dict):return set().union(*(markers(x) for x in v.values())) if v else set()
  return set()
 def surface(v):
  if isinstance(v,str):return markers(v)
  if isinstance(v,list):return set().union(*(markers(x) if isinstance(x,str) else set().union(*(markers(x.get(k)) for k in ['text','input_text','output_text'])) if isinstance(x,dict) else set() for x in v)) if v else set()
  return set()
 first={};turn=0;last=None
 def put(uuid,t,d):
  if uuid not in first or t<first[uuid][0] or (t==first[uuid][0] and d=='received'):first[uuid]=(t,d)
 for row in rows:
  if row.get('k')=='tool.call':
   for u in markers(row.get('args')):put(u,max(0,turn-1) if last==row.get('t','') else turn,'sent')
  if row.get('k') in ['msg.in','msg.out'] or ('role' in row and 'content' in row):
   received=surface(row.get('content'))|markers(row.get('text'))
   for u in markers(row):put(u,turn,'received' if u in received else 'sent')
   turn+=1
  if row.get('k') in ['msg.in','msg.out']:last=row.get('t','')
 return [{'tape_id':tape,'uuid':u,'first_turn_index':t,'direction':d} for u,(t,d) in sorted(first.items(),key=lambda x:(x[1][0],x[0]))]
for receipt in json.loads((n/'normalization.json').read_bytes()):
 name=receipt['name'];p=r/('normalized-'+name);p.mkdir(exist_ok=True);(p/'raw').mkdir(exist_ok=True);shutil.copytree(n/name/'corpus',p/'corpus',dirs_exist_ok=True);shutil.copyfile(n/name/'normalized.jsonl',p/'raw/normalized.jsonl')
 data=(p/'raw/normalized.jsonl').read_bytes();rows=[json.loads(l) for l in data.splitlines()];id=receipt['normalized_sha256'];assert hashlib.sha256(data).hexdigest()==id
 queries=[]
 if name in ['root-session','implementer-session']:
  hist=historical_logs
  for label in ['explain-indexed-anchor','explain-edit']:
   old=json.loads((hist/(label+'.command.json')).read_bytes());queries.append({'id':'repaired-'+name+'-'+label,'target':old['argv'][2],'flags':dict(flags,anchor=True,depth=2,max_edges=20,max_fanout=10),'target_custody_sha256':hashlib.sha256((hist/(label+'.command.json')).read_bytes()).hexdigest()})
 else:
  for i,row in enumerate(rows):
   text=row.get('text') if row.get('k')=='code.read' else row.get('after_text') if row.get('k')=='code.edit' else None
   if text is not None:queries.append({'id':f'p1-{name}-event-{i:03}','target':text,'flags':dict(flags),'required_normalized_event_offset':i})
  if not queries:queries=[{'id':'p1-'+name+'-raw-only','target':'this raw transcript is not structured file evidence','flags':dict(flags)}]
 (p/'queries.json').write_text(json.dumps({'queries':queries},sort_keys=True)+'\n');(p/'ids.txt').write_text(id+'\n');(p/'direct.json').write_text('{"queries":[]}\n');(p/'dispatch.jsonl').write_text(''.join(json.dumps(l,sort_keys=True)+'\n' for l in dispatch(rows,id)))
 meta={'group':p.name,'source':'existing frozen P1 raw fixture or retained repaired transcript','normalization_custody':receipt,'tapes':[{'label':'normalized','tape_id':id,'raw':'raw/normalized.jsonl','sha256':id,'bytes':len(data),'compressed_sha256':receipt['compressed_sha256']}],'queries':queries,'files':{},'additional_store':False,'reingest':None}
 (p/'inputs.json').write_text(json.dumps(meta,sort_keys=True)+'\n');groups.append(meta)
(r/'groups-with-normalized.json').write_text(json.dumps(groups,sort_keys=True)+'\n')
