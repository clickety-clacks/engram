#!/usr/bin/env python3
"""Freeze the named source-defined §9.3 fixtures before candidate observation."""
import hashlib,json,re,subprocess,sys,shutil
from pathlib import Path
root=Path(sys.argv[1]);root.mkdir()
sha=lambda b:hashlib.sha256(b).hexdigest()
enc=lambda x:json.dumps(x,ensure_ascii=False,separators=(',',':'))
def fingerprint(text):
    tokens=[];word=''
    for c in text:
        if c.isalnum() or c=='_':word+=c.lower() if c.isascii() else c;continue
        if word:tokens.append(word);word=''
        if not c.isspace():tokens.append(c)
    if word:tokens.append(word)
    hashes=[sha('\x1f'.join(tokens[i:i+5]).encode())[:16] for i in range(max(0,len(tokens)-4))]
    features=list(dict.fromkeys(hashes if len(hashes)<=4 else [min(hashes[i:i+4]) for i in range(len(hashes)-3)]))
    return 'winnow:'+','.join(features) if features else ''
flags={'depth':10,'forensics':False,'include_deleted':False,'max_edges':500,'max_fanout':50,'min_confidence':0.5,'anchor':False}
def query(id,target,**kw):return {'id':id,'target':target,'flags':dict(flags,**kw)}
def read(file,text,t='2026-02-22T00:00:00Z',range=[1,4]):return {'t':t,'k':'code.read','file':file,'range':range,'text':text}
def edit(file,before,after,t='2026-02-22T00:00:00Z',before_range=[1,1],after_range=[1,1],similarity=0.95):
    e={'t':t,'k':'code.edit','file':file,'before_range':before_range,'after_range':after_range}
    if before is not None:e['before_text']=before
    if after is not None:e['after_text']=after
    e['similarity']=similarity
    return e
allgroups=[]
def group(name,tapes,queries,source,files=None,additional=False,reingest=None):
    p=root/name;p.mkdir();(p/'corpus').mkdir();(p/'raw').mkdir()
    ids=[];ledger=[];dispatch=[]
    for label,rows in tapes.items():
        if name=='ordering':rows=[dict(sorted(row.items())) for row in rows]
        b=''.join(enc(r)+'\n' for r in rows).encode();id=sha(b);ids.append(id)
        (p/'raw'/(label+'.jsonl')).write_bytes(b)
        z=subprocess.check_output(['/usr/bin/zstd','-q','-c','--no-check'],input=b)
        (p/'corpus'/(id+'.jsonl.zst')).write_bytes(z)
        ledger.append({'label':label,'tape_id':id,'raw':'raw/'+label+'.jsonl','sha256':id,'bytes':len(b),'compressed_sha256':sha(z)})
        first={};turn=0
        for e in rows:
            if e['k'] not in ['msg.in','msg.out']:continue
            surface=e.get('content') if isinstance(e.get('content'),str) else ''
            for uuid in re.findall(r'<engram-src id=\\?"([0-9a-f-]{36})\\?"/>',enc(e).replace('\\"','"')):
                direction='received' if uuid in surface else 'sent'
                first.setdefault(uuid,{'tape_id':id,'uuid':uuid,'first_turn_index':turn,'direction':direction})
            turn+=1
        dispatch+=sorted(first.values(),key=lambda l:(l['first_turn_index'],l['uuid']))
    (p/'ids.txt').write_text(''.join(i+'\n' for i in sorted(ids)))
    (p/'queries.json').write_text(enc({'queries':queries})+'\n')
    (p/'direct.json').write_text('{"queries":[]}\n')
    (p/'dispatch.jsonl').write_text(''.join(enc(l)+'\n' for l in dispatch))
    files=files or {}
    for name_,text in files.items():
        f=p/'files'/name_;f.parent.mkdir(parents=True,exist_ok=True);f.write_text(text)
    m={'group':name,'source':source,'tapes':ledger,'queries':queries,'files':{n:{'sha256':sha(t.encode()),'path':'files/'+n} for n,t in files.items()},'additional_store':additional,'reingest':reingest,'serialization':'UTF-8 compact JSON in declared field order plus LF; fixture bytes are frozen before observation; event values/paths/times/ranges from named source'}
    (p/'inputs.json').write_text(enc(m)+'\n');allgroups.append(m)
text=''.join(f'fn line_{i}() {{ value_{i}(); }}\n' for i in range(1,73))
q=query('fixed-unaligned-edit','src/lib.rs:25-32');q['target_text_variants']=['\n'.join(text.splitlines()[24:32]),''.join(text.splitlines(keepends=True)[24:32])]
group('unaligned',{'edit':[edit('src/lib.rs','fn old() { legacy(); }\n',text,before_range=[1,24],after_range=[1,24])]},[q],'tests/cli_e2e.rs::explain_matches_windowed_edit_anchor_for_arbitrary_subspan',{'src/lib.rs':text})
before='fn before_item() { return alpha_value + beta_value; }';after='fn after_item() { return gamma_value + delta_value; }'
link={'t':'2026-02-22T00:00:01Z','k':'span.link','from_file':'src/a.rs','from_range':[1,2],'to_file':'src/b.rs','to_range':[10,20],'note':'extract'}
group('forensics-span',{'edit-link':[edit('src/lib.rs',before,after,similarity=0.1),link]},[query('fixed-ordinary',fingerprint(after),anchor=True),query('fixed-forensics',fingerprint(after),anchor=True,forensics=True),query('fixed-span-link','span:src/b.rs:10-20',anchor=True,min_confidence=0.99)],'tests/cli_e2e.rs::explain_forensics_and_agent_links_behave_as_specified')
deleted='fn deleted_item() { return removed_value + legacy_value; }';feature=fingerprint(deleted).split(',')[0]
group('tombstone',{'deleted':[{'t':'2026-02-22T00:00:00Z','k':'code.edit','file':'src/lib.rs','before_range':[10,12],'before_text':deleted}]},[query('fixed-deleted-hidden',feature,anchor=True),query('fixed-deleted-visible',feature,anchor=True,include_deleted=True)],'tests/cli_e2e.rs::explain_include_deleted_controls_tombstones')
text='pub fn ordered_touch() {\nlet alpha = normalize(source);\nlet beta = transform(alpha);\npublish(alpha + beta);\n}\n';part='pub fn ordered_touch() {\nlet alpha = normalize(source);\nlegacy_publish(alpha);\n}\n'
tapes={}
for name,one,two in [('recent','04','05'),('tied-a','01','02'),('tied-b','01','02')]:
    tapes[name]=[read('src/'+name+'.rs',part,'2026-02-22T00:00:'+s+'Z') for s in [one,two]]
tapes['newest']=[read('src/newest.rs',text,'2026-02-22T00:00:10Z',[1,5])]
group('ordering',tapes,[query('fixed-session-order',text)],'tests/cli_e2e.rs::explain_orders_sessions_by_touch_count_then_recency')
text='fn shared_touch() {\n    let value = alpha + beta;\n    consume(value);\n}\n';before='fn shared_touch() {\n    let value = alpha;\n    consume(value);\n}\n'
group('additional',{'read':[read('src/a.rs',text)],'edit':[edit('src/b.rs',before,text,'2026-02-22T00:00:01Z',[1,4],[1,4],0.91)]},[query('fixed-additional-dedupe',fingerprint(text),anchor=True,stores_queried=2)],'tests/ingest_e2e.rs::explain_fans_out_to_additional_stores_and_dedupes',additional=True)
u1='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';u2='bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb'
def msg(t,received,uuid,suffix):return {'t':t,'k':'msg.in' if received else 'msg.out','role':'user' if received else 'assistant','content':f'<engram-src id="{uuid}"/> '+suffix if received else [{'type':'toolCall','arguments':{'payload':f'<engram-src id="{uuid}"/> '+suffix}}]}
text='pub fn continuation_probe() -> &\'static str { "T126" }'
q=query('fixed-dispatch-chain','src/engine.rs:2-2');q['target_text_variants']=[text,text+'\n']
group('dispatch',{'a':[msg('2026-02-27T12:00:00Z',False,u1,'do work')],'b':[msg('2026-02-27T12:05:01Z',True,u1,'please implement'),msg('2026-02-27T12:05:02Z',False,u2,'continue')],'c':[msg('2026-02-27T12:10:01Z',True,u2,'execute'),edit('src/engine.rs',text,text,'2026-02-27T12:10:02Z',[2,2],[2,2])],'sibling':[msg('2026-02-27T12:06:01Z',True,u1,'sibling')]},[q],'tests/dispatch_marker_e2e.rs::explain_dispatch_chain_includes_a_to_b_to_c_and_excludes_sibling',{'src/engine.rs':'fn helper() {}\n'+text+'\n'})
u='cccccccc-cccc-4ccc-8ccc-cccccccccccc';text='pub fn continuation_probe() -> &\'static str { "T126R" }'
q=query('fixed-restart-reingest','src/engine.rs:2-2');q['target_text_variants']=[text,text+'\n']
group('reingest',{'base':[{'t':'2026-02-27T13:00:01Z','k':'msg.out','role':'assistant','content':[{'type':'toolCall','arguments':{'payload':f'<engram-src id="{u}"/>'}}]}],'worker':[msg('2026-02-27T13:05:01Z',True,u,'run'),edit('src/engine.rs',text,text,'2026-02-27T13:05:02Z',[2,2],[2,2])],'restart':[msg('2026-02-27T13:20:01Z',True,u,'resumed after compact'),edit('src/engine.rs',text,text,'2026-02-27T13:20:02Z',[2,2],[2,2])]},[q],'tests/dispatch_marker_e2e.rs::compact_restart_reingest_adds_new_tape_without_duplication',{'src/engine.rs':'fn helper() {}\n'+text+'\n'},reingest='worker')
text=''.join(f'fn line_{i}() {{ value_{i}(); }}\n' for i in range(1,73))
q=query('fixed-unaligned-read','src/lib.rs:25-32');q['target_text_variants']=['\n'.join(text.splitlines()[24:32]),''.join(text.splitlines(keepends=True)[24:32])]
group('unaligned-read',{'read':[read('src/lib.rs',text,range=[1,72])]},[q],'§9.3 canonical read counterpart of tests/cli_e2e.rs::explain_matches_windowed_edit_anchor_for_arbitrary_subspan; existing edit case unchanged',{'src/lib.rs':text})
(root/'groups.json').write_text(enc(allgroups)+'\n')

# The retained restart journey queries base+worker before repeating worker and
# adding the distinct restart. Derive this pre-state independently as well.
p=root/'reingest';before=p/'before';(before/'corpus').mkdir(parents=True)
g=next(g for g in allgroups if g['group']=='reingest')
ids=[t['tape_id'] for t in g['tapes'] if t['label']!='restart']
for tape in ids:shutil.copyfile(p/'corpus'/(tape+'.jsonl.zst'),before/'corpus'/(tape+'.jsonl.zst'))
(before/'ids.txt').write_text(''.join(i+'\n' for i in sorted(ids)))
q=dict(g['queries'][0],id='fixed-restart-before')
(before/'queries.json').write_text(enc({'queries':[q]})+'\n')
(before/'direct.json').write_text('{"queries":[]}\n')
(before/'dispatch.jsonl').write_text(''.join(l+'\n' for l in (p/'dispatch.jsonl').read_text().splitlines() if json.loads(l)['tape_id'] in ids))
