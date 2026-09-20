#!/usr/bin/env python3
"""§9.3 frozen P0 product expectations from raw/reference records, never candidate output.
Only canonical fingerprint primitives are shared by the separately built extractor.
No SQL, Engram query, index, or product subprocess is used here.
"""
import argparse, collections, hashlib, json, struct, subprocess
from pathlib import Path

def canonical(v):return json.dumps(v,ensure_ascii=False,sort_keys=True,separators=(',',':'))+'\n'
def digest(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def unique(xs):return list(dict.fromkeys(xs))
def f32(x):return struct.unpack('f',struct.pack('f',x))[0]

def derive(records,corpus,queries,oracle,dispatch,output,mode="P0"):
    output.mkdir()
    query_anchors={}; by_anchor=collections.defaultdict(list)
    incoming=collections.defaultdict(list);outgoing=collections.defaultdict(list)
    edges=[];tombstones=[];census=None
    for line in records.open():
        r=json.loads(line);k=r['kind']
        if k=='query':query_anchors[r['id']]=r['anchors']
        elif k=='edge':
            edge=(r['from'],r['to'],r['confidence'],'moved' if r.get('agent_link') else 'same','1:1',r.get('agent_link',False),r.get('note'))
            edges.append(edge);incoming[edge[1]].append(edge);outgoing[edge[0]].append(edge)
        elif k=='window':by_anchor[r['anchor']].append((r['timestamp'],r['tape'],r['offset'],r['touch_kind'],r['file'],r['ordinal']))
        elif k=='tombstone':tombstones.append(r)
        elif k=='census':census=r
        else:raise ValueError(k)
    if mode=='P0':assert census['events']==3250068 and census['edit_edges']==150357 and census['span_links']==0 and census['tapes']==45758,census
    for values in by_anchor.values():values.sort()
    for d in [incoming,outgoing]:
        for a,es in d.items():d[a]=sorted(unique(es),key=lambda e:-e[2])
    wanted={a for q in query_anchors.values() for a in q}
    wanted.update(a for e in edges for a in e[:2] if ',' not in a)
    matches=collections.defaultdict(list)
    for a in by_anchor:
        for feature in a.removeprefix('winnow:').split(','):
            f='winnow:'+feature
            if f in wanted:matches[f].append(a)
    for values in matches.values():values.sort()
    links=[json.loads(l) for l in dispatch.read_text().splitlines()]
    by_tape=collections.defaultdict(list);sent=collections.defaultdict(list)
    for l in links:
        by_tape[l['tape_id']].append(l)
        if l['direction']=='sent':sent[l['uuid']].append(l)
    for values in sent.values():values.sort(key=lambda l:(-l['first_turn_index'],l['tape_id']))
    raw_cache={}
    def raw(t):
        if t not in raw_cache:
            data=subprocess.check_output(['/usr/bin/zstd','-dc',str(corpus/(t+'.jsonl.zst'))])
            assert hashlib.sha256(data).hexdigest()==t
            text=data.decode();lines=text.split('\n')
            if lines[-1]=='':lines.pop()
            lines=[l.removesuffix('\r') for l in lines]
            raw_cache[t]=(lines,[json.loads(l) for l in lines if l.strip()])
        return raw_cache[t]
    def project(t):return {'timestamp':t[0],'tape_id':t[1],'event_offset':t[2],'kind':t[3],'file_path':t[4]}
    frozen={q['id']:q['touches'] for q in json.loads(oracle.read_bytes())['queries']}
    def anchors_for(a):return [a] if a.startswith('span:') or ',' in a else matches[a]
    cases=[]
    for q in json.loads(queries.read_bytes())['queries']:
        anchors=query_anchors[q['id']]
        direct=sorted(set(w[:5] for a in anchors for composite in anchors_for(a) for w in by_anchor[composite]))
        if mode=='P0' or q['id'] in frozen:assert [project(t) for t in direct]==frozen[q['id']],('independent direct projection mismatch',q['id'])
        roots=unique(c for a in anchors for c in anchors_for(a));queue=collections.deque((r,0) for r in roots)
        visited=set();seen_edges=set();lineage=[]
        while queue:
            a,depth=queue.popleft()
            if a in visited or depth>=q['flags']['depth']:continue
            visited.add(a)
            if len(lineage)>=q['flags']['max_edges']:break
            candidates=unique(incoming[a]+outgoing[a])
            candidates=[e for e in candidates if e not in seen_edges and (q['flags']['forensics'] or e[5] or e[2]>=max(f32(0.30),f32(q['flags']['min_confidence'])))]
            candidates.sort(key=lambda e:-e[2])
            for e in candidates[:q['flags']['max_fanout']]:
                if len(lineage)>=q['flags']['max_edges']:break
                seen_edges.add(e);lineage.append(e);nxt=e[1] if e[0]==a else e[0]
                if nxt not in visited:queue.append((nxt,depth+1))
        touched=unique(anchors+roots+[a for e in lineage for a in e[:2]])
        touches=set(direct)
        for a in touched:
            for composite in anchors_for(a):
                touches.update(w[:5] for w in by_anchor[composite])
        grouped=collections.defaultdict(list)
        for t in sorted(touches):grouped[t[1]].append(t)
        raw_sessions=[];scores={}
        for tape,ts in grouped.items():
            ts.sort(key=lambda t:t[2]);scores[tape]=f32(sum(any(w[1]==tape for c in anchors_for(a) for w in by_anchor[c]) for a in anchors)/len(anchors))
            raw_sessions.append((tape,ts,max(t[0] for t in ts),ts[0][2]))
        raw_sessions.sort(key=lambda s:(-len(s[1]),tuple(-ord(c) for c in s[2]),s[0]))
        # The first two ordering keys are the existing public traversal rule;
        # tape ID is used only when it cannot change hop order. Detect otherwise.
        hops=[];seen_hops=set();seen_tapes=set(grouped);extras=[];rank_hop_origins=collections.defaultdict(set)
        for tape,ts,latest,anchor_offset in raw_sessions:
            for touch in ts:
                if touch[3]!='edit':continue
                rows=raw(tape)[1];turn=sum(r.get('k') in ('msg.in','msg.out') for r in rows[:touch[2]])
                current=tape;seen=set()
                while True:
                    received=sorted((l for l in by_tape[current] if l['direction']=='received' and l['first_turn_index']<turn),key=lambda l:(-l['first_turn_index'],l['uuid']))
                    if not received:break
                    r=received[0]
                    if not sent[r['uuid']]:break
                    p=sent[r['uuid']][0];key=(current,turn,r['uuid'],r['first_turn_index'],p['tape_id'],p['first_turn_index'])
                    if key in seen:break
                    seen.add(key)
                    rank_hop_origins[(len(ts),latest)].add(tape)
                    if key not in seen_hops:
                        seen_hops.add(key);hops.append(dict(zip(['session','edit_turn_index','received_uuid','received_turn_index','parent_session','parent_sent_turn_index'],key)))
                    if p['tape_id'] not in seen_tapes:
                        seen_tapes.add(p['tape_id']);pr=raw(p['tape_id'])[1]
                        mo=[i for i,r in enumerate(pr) if r.get('k') in ('msg.in','msg.out')]
                        po=mo[p['first_turn_index']] if p['first_turn_index']<len(mo) else max(len(pr)-1,0)
                        extras.append((p['tape_id'],[],max((r.get('t','') for r in pr),default=''),po))
                    current=p['tape_id'];turn=p['first_turn_index']
        ambiguous={str(k):sorted(v) for k,v in rank_hop_origins.items() if len(v)>1}
        if ambiguous:
            (output/'ordering-blocker.json').write_text(canonical({'query':q['id'],'ambiguous_rank_dispatch_origins':ambiguous}))
            raise ValueError('Unsettled dispatch hop ordering in fixed case '+q['id'])
        sessions=[]
        for tape,ts,latest,anchor_offset in raw_sessions+extras:
            lines,rows=raw(tape);start=max(1,anchor_offset+1-22);end=min(len(lines),start+29)
            files=sorted({t[4] for t in ts if t[4]})
            if not files:files=sorted({r[k] for r in rows for k in ['file','from_file','to_file'] if isinstance(r.get(k),str)})
            sessions.append({'session_id':tape,'timestamp':latest,'window_start':start,'window_end':end,'total_lines':len(lines),'confidence':scores.get(tape,0.0),'refs_up':sum(l['direction']=='received' for l in by_tape[tape]),'refs_down':sum(l['direction']=='sent' for l in by_tape[tape]),'files_touched':files,'touches':[{k:v for k,v in project(t).items() if k!='tape_id'} for t in ts]})
        parents={h['session']:h['parent_session'] for h in hops};children=collections.defaultdict(list)
        for h in hops:children[h['parent_session']].append(h['session'])
        roots_of={};depths={}
        for s in sessions:
            t=s['session_id'];cur=t;depth=0;seen=set()
            while cur in parents:
                if cur in seen:raise ValueError('cyclic fixed dispatch ancestry')
                seen.add(cur);cur=parents[cur];depth+=1
            roots_of[t]=cur;depths[t]=depth
        lengths=collections.Counter(roots_of.values())
        for s in sessions:
            t=s['session_id'];s.update(depth=depths[t],parent=parents.get(t),children=sorted(children[t]),chain_length=lengths[roots_of[t]])
        sessions.sort(key=lambda s:(-len(s['touches']),tuple(-ord(c) for c in s['timestamp']),s['depth'],-s['confidence'],s['session_id']))
        total=len(sessions);times=sorted(s['timestamp'] for s in sessions if s['timestamp']);sessions=sessions[:10]
        chains=[]
        for s in sessions:
            cur=s['session_id'];subset={x['session_id']:x['parent'] for x in sessions if x['parent'] is not None}
            while cur in subset:cur=subset[cur]
            chain=next((c for c in chains if c['root_session_id']==cur),None)
            if chain is None:chain={'root_session_id':cur,'descendants':[]};chains.append(chain)
            chain['descendants'].append({k:s[k] for k in ['session_id','depth','parent','children']})
        for c in chains:c['descendants'].sort(key=lambda d:d['depth'])
        deleted=[];seen_deleted=set()
        if q['flags']['include_deleted']:
            for anchor in touched:
                hits=[t for t in tombstones if (t['anchor']==anchor if ',' in anchor else anchor.removeprefix('winnow:') in t['anchor'].removeprefix('winnow:').split(','))]
                hits.sort(key=lambda t:(t['timestamp'],t['tape'],t['offset'],t['ordinal']))
                for t in hits:
                    key=(t['tape'],t['offset'],t['file'],*t['range'],t['timestamp'])
                    if key in seen_deleted:continue
                    seen_deleted.add(key)
                    deleted.append({'anchor':t['anchor'],'tape_id':t['tape'],'event_offset':t['offset'],'file_path':t['file'],'range':{'start':t['range'][0],'end':t['range'][1]},'timestamp':t['timestamp']})
        payload={'query':{'command':'explain','target':q['target'],'anchors':anchors,'grep_filter':None,'limit':None,'offset':0,'min_confidence':f32(q['flags']['min_confidence']),'since':None,'until':None,'count':False,'max_fanout':q['flags']['max_fanout'],'max_edges':q['flags']['max_edges'],'depth':q['flags']['depth'],'forensics':q['flags']['forensics'],'include_deleted':q['flags']['include_deleted']},'sessions':sessions,'chains':chains,'lineage':[dict(zip(['from_anchor','to_anchor','confidence','location_delta','cardinality','agent_link','note'],e),stored_class='lineage' if e[5] or e[2]>=f32(0.30) else 'location_only') for e in lineage],'dispatch_lineage':hops,'tombstones':deleted,'stores_queried':q['flags'].get('stores_queried',1),'returned':len(sessions),'total':total,'time_range':{'start':times[0] if times else None,'end':times[-1] if times else None},'truncated':total>len(sessions)}
        success=bool(sessions or lineage or deleted)
        if not success:payload={'error':'no_results','query':q['target']}
        path=output/(q['id']+'-explain.json');path.write_text(canonical(payload))
        case={'id':q['id']+'-full-explain','corpus':mode,'argv':['explain',q['target'],'--depth',str(q['flags']['depth']),'--max-fanout',str(q['flags']['max_fanout']),'--max-edges',str(q['flags']['max_edges']),'--min-confidence',str(q['flags']['min_confidence'])],'expected':path.name,'expected_sha256':digest(path),'exit':0 if success else 1,'channel':'stdout' if success else 'stderr_json','coverage':['read-edit-sessions','canonical-touches','session-order','lineage','dispatch','stores-queried'],'exclusions':[]}
        if q['flags'].get('anchor'):case['argv'].append('--anchor')
        if q['flags']['forensics']:case['argv'].append('--forensics')
        if q['flags']['include_deleted']:case['argv'].append('--include-deleted')
        cases.append(case)
        for i,s in enumerate(sessions):
            tape=s['session_id'];lines,rows=raw(tape);start=s['window_start'];end=s['window_end']
            peek={'query':{'command':'peek','session_id':tape,'start':start,'lines':30,'before':None,'after':None,'grep_filter':None},'session':{'session_id':tape,'timestamp':max(r.get('t','') for r in rows),'window_start':start,'window_end':end,'total_lines':len(lines),'content':[{'line':j+1,'text':lines[j]} for j in range(start-1,end)]}}
            p=output/(q['id']+f'-window-{i:02}.json');p.write_text(canonical(peek))
            cases.append({'id':q['id']+f'-window-{i:02}','corpus':mode,'argv':['peek',tape,'--start',str(start),'--lines','30'],'expected':p.name,'expected_sha256':digest(p),'exit':0,'coverage':['raw-windows','raw-offsets'],'exclusions':[]})
        print(q['id'],total,len(lineage),len(hops),flush=True)
    manifest={'schema':'t1772-section93-cases-v1','status':'P0 component; full gate still blocked pending unchanged fixed synthetic/P1/reingest cases','cases':cases,'parents':{'records_sha256':digest(records),'queries_sha256':digest(queries),'direct_projection_sha256':digest(oracle),'dispatch_sha256':digest(dispatch)},'nonsemantic_exclusions':[],'p0_span_link_count':census['span_links'] if mode=='P0' else None,'default_config':{'explain_default_limit':10,'peek_default_lines':30,'peek_default_before':30,'peek_default_after':10,'peek_grep_context':5}}
    (output/'manifest.json').write_text(canonical(manifest))

if __name__=='__main__':
    p=argparse.ArgumentParser()
    p.add_argument('--mode',choices=['P0','P1'],default='P0')
    for name in ['records','corpus','queries','oracle','dispatch','output']:p.add_argument('--'+name,type=Path,required=True)
    derive(**vars(p.parse_args()))
