from og import *
import time, sys
T=sys.argv[1] if len(sys.argv)>1 else "person-000007"
TYPES=["Person","Knows","WorksAt","Contributes"]
def reset(): STATS["req"]=0; STATS["bytes"]=0
def touches(ch):
    if ch["id"]==T and ch["type"]["name"]=="Person": return True
    for im in (ch.get("before"),ch.get("after")):
        if im and im.get("endpoints") and T in (im["endpoints"]["from"],im["endpoints"]["to"]): return True
    return False
def report(name,t0,found):
    print(f"{name}: {STATS['req']} requests, {STATS['bytes']/1024:.0f} KiB, {time.time()-t0:.2f} s, {found} changes to {T}")
# list all first-parent commits
reset(); t0=time.time(); commits=[]; tok=None
while True:
    p={"branch":"main","limit":1000}
    if tok: p["page_token"]=tok
    c,o=req("GET",f"/graphs/{G}/commits",p); commits+=o["commits"]; tok=o.get("next_page_token")
    if not tok: break
report(f"A0 list {len(commits)} commits",t0,"-")
# A: walk newest 200 commits' changes, type-filtered
for n,flt in ((200,True),(200,False),(len(commits),True)):
    reset(); t0=time.time(); found=0
    for cm in commits[:n]:
        p={"limit":8192}
        if flt: p["type"]=TYPES
        c,o=req("GET",f"/graphs/{G}/commits/{cm['graph_commit_id']}/changes",p)
        if c==200: found+=sum(1 for ch in o["changes"] if touches(ch))
    report(f"A walk {n} commits ({'type-filtered' if flt else 'unfiltered'})",t0,found)
# B: change feed from the beginning, filtered by type
for types in (["Person"],TYPES):
    reset(); t0=time.time(); found=0; blocks=0; ents=[]
    p={"branch":"main","start":"beginning","limit":8192,"type":types}
    while True:
        c,o=req("GET",f"/graphs/{G}/changes",p)
        if c!=200: print("feed",c,o); break
        for b in o["blocks"]:
            blocks+=1
            for ch in b["changes"]:
                if touches(ch): found+=1; ents.append((b["cause"],ch))
        if o.get("next_page_token"):
            p={"branch":"main","page_token":o["next_page_token"],"limit":8192,"type":types}
        elif o.get("caught_up") is False:
            p={"branch":"main","cursor":o["cursor"],"limit":8192,"type":types}
        else: break
    report(f"B feed from beginning, types {types} ({blocks} blocks)",t0,found)
json.dump(ents,open("entries.json","w"))
# D: diff between two commits, filtered by type
mid=commits[len(commits)//2]["graph_commit_id"]; head=commits[0]["graph_commit_id"]
reset(); t0=time.time(); c,o=req("GET",f"/graphs/{G}/diff",{"from":mid,"to":head,"type":["Person"],"limit":8192})
report(f"D diff mid..head type Person ({c}, {len(o.get('changes',[]))} changes)",t0,sum(1 for ch in o.get('changes',[]) if touches(ch)))
# E: point read at a commit
reset(); t0=time.time()
c,o=query("query q($s: String) { match { $p: Person { slug: $s } } return { $p.slug, $p.title, $p.email, $p.tags } }",{"s":T},snapshot=mid)
report(f"E point read at a commit ({c}) {o.get('rows')}",t0,"-")
# G: pending branches
reset(); t0=time.time(); c,o=req("GET",f"/graphs/{G}/branches",{"detailed":"true"}); names=[b if isinstance(b,str) else b.get("name") for b in o.get("branches",[])]
pend={}
for nm in names:
    if nm=="main": continue
    c,d=req("GET",f"/graphs/{G}/branches/diff",{"source":nm,"target":"main","type":TYPES,"limit":8192})
    pend[nm]=sum(1 for ch in d.get("changes",[]) if touches(ch)) if c==200 else c
report(f"G pending on {len(names)-1} branches {pend}",t0,"-")
