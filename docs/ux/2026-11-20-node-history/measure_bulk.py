from og import *
import time
T="person-000007"; TYPES=["Person","Knows","WorksAt","Contributes"]
def reset(): STATS["req"]=0; STATS["bytes"]=0
def touches(ch):
    if ch["id"]==T and ch["type"]["name"]=="Person": return True
    for im in (ch.get("before"),ch.get("after")):
        if im and im.get("endpoints") and T in (im["endpoints"]["from"],im["endpoints"]["to"]): return True
    return False
def rep(name,t0,found): print(f"{name}: {STATS['req']} req, {STATS['bytes']/1048576:.1f} MiB, {time.time()-t0:.2f} s, {found} hits")
c,o=req("GET",f"/graphs/{G}/commits",{"branch":"main","limit":1000}); commits=o["commits"]; print(len(commits),"commits")
def feed(start,types,stop_ids=None):
    found=0; blocks=0; p={"branch":"main","start":start,"limit":8192,"type":types}
    while True:
        c,o=req("GET",f"/graphs/{G}/changes",p)
        if c!=200: print(c,o); return found,blocks
        for b in o["blocks"]:
            blocks+=1; found+=sum(1 for ch in b["changes"] if touches(ch))
        if o.get("next_page_token"): p={"branch":"main","page_token":o["next_page_token"],"limit":8192,"type":types}
        elif o.get("caught_up") is False: p={"branch":"main","cursor":o["cursor"],"limit":8192,"type":types}
        else: return found,blocks
for n in (60,200):
    oldest=commits[n-1]; start="after:"+oldest["parent_commit_id"]
    reset(); t0=time.time(); f,b=feed(start,TYPES); rep(f"feed window newest {n} commits ({b} blocks)",t0,f)
    reset(); t0=time.time(); found=0
    for cm in commits[:n]:
        tok=None
        while True:
            p={"limit":8192,"type":TYPES}
            if tok: p["page_token"]=tok
            c,o=req("GET",f"/graphs/{G}/commits/{cm['graph_commit_id']}/changes",p)
            if c!=200: break
            found+=sum(1 for ch in o["changes"] if touches(ch)); tok=o.get("next_page_token")
            if not tok: break
    rep(f"walk newest {n} commits",t0,found)
reset(); t0=time.time(); f,b=feed("beginning",TYPES); rep(f"feed from beginning ({b} blocks)",t0,f)
# edge types only (a node's relationships), excluding the node's own type
reset(); t0=time.time(); f,b=feed("beginning",["Knows","WorksAt","Contributes"]); rep(f"feed from beginning, edge types only",t0,f)
# op=update only on Person
reset(); t0=time.time(); p={"branch":"main","start":"beginning","limit":8192,"type":["Person"],"op":["update"]}; c,o=req("GET",f"/graphs/{G}/changes",p); rep("one feed page Person op=update",t0,"-")
# diff from a commit before the bulk to head
mid=commits[100]["graph_commit_id"]
reset(); t0=time.time(); f=0; p={"from":mid,"to":"main","type":["Person"],"limit":8192}
while True:
    c,o=req("GET",f"/graphs/{G}/diff",p); f+=sum(1 for ch in o.get("changes",[]) if touches(ch))
    if not o.get("next_page_token"): break
    p={"from":mid,"to":"main","type":["Person"],"limit":8192,"page_token":o["next_page_token"]}
rep("diff from before the bulk load to head, Person",t0,f)
reset(); t0=time.time()
for cm in commits[:20]:
    query("query q($s: String) { match { $p: Person { slug: $s } } return { $p.title, $p.email, $p.tags } }",{"s":T},snapshot=cm["graph_commit_id"])
rep("20 point reads of the node at 20 commits",t0,"-")
def feedop(start,types,ops):
    found=0; p={"branch":"main","start":start,"limit":8192,"type":types,"op":ops}
    while True:
        c,o=req("GET",f"/graphs/{G}/changes",p)
        for b in o["blocks"]: found+=sum(1 for ch in b["changes"] if touches(ch))
        if o.get("next_page_token"): p={"branch":"main","page_token":o["next_page_token"],"limit":8192,"type":types,"op":ops}
        elif o.get("caught_up") is False: p={"branch":"main","cursor":o["cursor"],"limit":8192,"type":types,"op":ops}
        else: return found
reset(); t0=time.time(); f=feedop("beginning",TYPES,["update","delete"]); rep("feed from beginning, op update+delete",t0,f)
