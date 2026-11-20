import json, time, urllib.request, urllib.parse
BASE="http://127.0.0.1:8797"; G="knowledge"
H={"Authorization":"Bearer dev-token","Omnigraph-Http-Api":"0.14","Content-Type":"application/json"}
STATS={"req":0,"bytes":0}
def req(method, path, params=None, body=None):
    url=BASE+path
    if params: url+="?"+urllib.parse.urlencode(params, doseq=True)
    data=json.dumps(body).encode() if body is not None else None
    r=urllib.request.Request(url, data=data, method=method, headers=H)
    try:
        with urllib.request.urlopen(r, timeout=300) as resp:
            raw=resp.read()
    except urllib.error.HTTPError as e:
        raw=e.read(); STATS["req"]+=1; STATS["bytes"]+=len(raw)
        return e.code, json.loads(raw or b"{}")
    STATS["req"]+=1; STATS["bytes"]+=len(raw)
    return 200, json.loads(raw)
def mutate(q, params=None, branch="main"):
    b={"query":q} if q.startswith("branch") else {"query":q,"params":params or {},"branch":branch}
    c,o=req("POST",f"/graphs/{G}/mutate",body=b)
    assert c==200,(c,o); return o
def query(q, params=None, branch=None, snapshot=None):
    b={"query":q,"params":params or {}}
    if snapshot: b["snapshot"]=snapshot
    elif branch: b["branch"]=branch
    return req("POST",f"/graphs/{G}/query",body=b)
