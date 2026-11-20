"""The fixture of docs/ux/2026-11-20-node-history.md, on the development graph.

Run against a fresh `script/dev-server --reset` (STUDIO_DEV_BIND=127.0.0.1:8797), then
`python3 measure.py` for the first phase, `python3 fixture.py --bulk`, then `python3 measure_bulk.py`.
"""
import sys, urllib.request
from og import *
import time

def history():
    T="person-000007"; R="person-000099"
    UT="query u($s: String, $t: String) { update Person set { title: $t } where slug = $s }"
    UE="query u($s: String, $e: String) { update Person set { email: $e } where slug = $s }"
    UG="query u($s: String, $g: [String]) { update Person set { tags: $g } where slug = $s }"
    KI="query k($a: String, $b: String) { insert Knows { from: $a, to: $b } }"
    KD="query k($a: String, $b: String) { delete Knows where from = $a and to = $b }"
    t0=time.time(); touched=[]
    for i in range(400):
        if i%40==20:
            k=(i//40)%3
            o=mutate([UT,UE,UG][k], {"s":T, **[{"t":f"T title {i}"},{"e":f"t{i}@x.org"},{"g":[f"tag{i}","x"]}][k]})
            touched.append(o["commit"]["graph_commit_id"])
        elif i==110: touched.append(mutate(KI,{"a":T,"b":"person-000051"})["commit"]["graph_commit_id"])
        elif i==150: touched.append(mutate(KI,{"a":"person-000052","b":T})["commit"]["graph_commit_id"])
        elif i==210: touched.append(mutate(KD,{"a":T,"b":"person-000051"})["commit"]["graph_commit_id"])
        elif i==255: mutate("query d($s: String) { delete Person where slug = $s }",{"s":R})
        elif i==265: mutate("query n($s: String) { insert Person { slug: $s, name: \"Reborn\" } }",{"s":R})
        else:
            s=f"person-{(i%200):06d}"
            if s in (T,R): s="person-000001"
            mutate(UT,{"s":s,"t":f"Title {i}"})
    print("400 commits in",round(time.time()-t0,1),"s; touched T in",len(touched))
    
    mutate('branch create featmerged from main')
    mutate(UT,{"s":T,"t":"Set on featmerged"},branch="featmerged")
    mutate(UT,{"s":"person-000003","t":"main moved"})
    print(mutate('branch merge featmerged into main')["outcome"])
    mutate('branch create agentpending from main')
    mutate(UT,{"s":T,"t":"Proposed by agent"},branch="agentpending")
    mutate(UG,{"s":T,"g":["proposed"]},branch="agentpending")
    

def bulk():
    for k in range(10):
      lines="\n".join(json.dumps({"type":"Person","data":{"slug":f"bulk-{i:06d}","name":f"Bulk {i}","title":"Imported","email":f"b{i}@example.org","tags":["import","2026"]}}) for i in range(k*5000,(k+1)*5000))+"\n"
      r=urllib.request.Request(BASE+f"/graphs/{G}/load/ndjson?branch=main&mode=merge",data=lines.encode(),method="POST",headers={**H,"Content-Type":"application/x-ndjson"})
      t=time.time(); print(urllib.request.urlopen(r,timeout=600).read()[:300], time.time()-t)
    UT="query u($s: String, $t: String) { update Person set { title: $t } where slug = $s }"
    for i in range(50): mutate(UT,{"s":"person-000007" if i%10==5 else f"person-{i:06d}","t":f"after bulk {i}"})
    

bulk() if '--bulk' in sys.argv else history()
