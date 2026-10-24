#!/usr/bin/env python3
"""Collect contributors' own X posts about the Corpus Hub into data/x_posts.json.

Needs an X API v2 bearer token in the environment (never commit it):
    X_BEARER_TOKEN=... python3 fetch_x_posts.py
Re-run matching on a saved API dump without network:
    python3 fetch_x_posts.py --from-cache dump.json

Steps: full-archive search for the queries below (paged with next_token), then
GET /2/tweets?ids=... for entities (expanded links) and long-post text, then match.
A post is attached to a contributor page only with one of:
  1. the post links hub.xerj.org/contributors/<login>;
  2. the author's X profile (url, bio, bio links) links github.com/<login>;
  3. the X username equals the GitHub login and the post is about XERJ / Corpus Hub.
Only the contributor's own posts are kept: retweets, third parties and project or
company accounts are excluded (EXCLUDE). Posts by @xerj_org are kept only as
"featured" when they link the contributor page. Text is stored exactly as returned.
"""
import json, os, re, sys, time, urllib.parse, urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
LEDGER = HERE.parent / "xerj-code/hub/candidates/authors/CONTRIBUTORS.jsonl"
OUT = HERE / "data/x_posts.json"
OFFICIAL = "xerj_org"
QUERIES = ["url:hub.xerj.org", '"hub.xerj.org" -is:retweet', '"Corpus Hub" xerj -is:retweet',
           '"selected for the XERJ" -is:retweet', "@xerj_org -is:retweet", "from:xerj_org",
           'xerj corpus -is:retweet -"Corpus Hub"', 'xerj (contributor OR "reference memory") -is:retweet']
# Matched by a link, but posted by a project/company account rather than the contributor.
EXCLUDE = {"getscriptlog": "project account", "somora_ai": "project account",
           "telemetrytrkr": "project account"}
GH = re.compile(r"github\.com/([A-Za-z0-9-]+)", re.I)
CP = re.compile(r"hub\.xerj\.org/contributors/([A-Za-z0-9_.-]+?)(?:\.html|/|\?|#|$)", re.I)
API = "https://api.x.com/2"
USER_FIELDS = "username,name,url,description,entities"


def get(path, params):
    req = urllib.request.Request(f"{API}/{path}?{urllib.parse.urlencode(params)}",
                                 headers={"Authorization": "Bearer " + os.environ["X_BEARER_TOKEN"]})
    time.sleep(2.1)  # stay under 30 requests/minute
    with urllib.request.urlopen(req) as r:
        return json.load(r)


def fetch():
    ids, dump = set(), {"posts": {}, "users": {}}
    for q in QUERIES:
        tok = None
        while True:
            p = {"query": q, "max_results": 25, "tweet.fields": "created_at,author_id",
                 "expansions": "author_id", "user.fields": USER_FIELDS}
            if tok:
                p["next_token"] = tok
            j = get("tweets/search/all", p)
            ids |= {t["id"] for t in j.get("data", [])}
            tok = j.get("meta", {}).get("next_token")
            if not tok or not j.get("data"):
                break
    ids = sorted(ids)
    for i in range(0, len(ids), 100):
        j = get("tweets", {"ids": ",".join(ids[i:i + 100]), "expansions": "author_id",
                           "tweet.fields": "entities,created_at,author_id,lang,note_tweet,referenced_tweets",
                           "user.fields": USER_FIELDS})
        dump["posts"].update({t["id"]: t for t in j.get("data", [])})
        dump["users"].update({u["id"]: u for u in j.get("includes", {}).get("users", [])})
    return dump


def profile_links(u):
    s = [u.get("url") or "", u.get("description") or ""]
    for k in ("url", "description"):
        s += [x.get("expanded_url") or "" for x in (u.get("entities", {}).get(k, {}).get("urls") or [])]
    return " ".join(s)


def match(dump):
    logins = {}
    for part in sorted(LEDGER.parent.glob("CONTRIBUTORS*.jsonl"), key=lambda p: int(p.stem.partition("-")[2] or 1)):   # split under the 5 MB file limit
        for line in part.open(encoding="utf-8"):
            if not line.strip():
                continue
            lg = json.loads(line)["contributor"]["login"]
            logins[lg.lower()] = lg
    out, skipped = {}, []
    for p in sorted(dump["posts"].values(), key=lambda p: p["created_at"]):
        u = dump["users"].get(p["author_id"], {})
        un = u.get("username", "")
        text = (p.get("note_tweet") or {}).get("text") or p["text"]
        urls = [x.get("unwound_url") or x.get("expanded_url") or "" for x in (p.get("entities", {}).get("urls") or [])]
        cp = [logins[m.group(1).lower()] for x in urls for m in [CP.search(x)] if m and m.group(1).lower() in logins]
        gh = [logins[g.lower()] for g in GH.findall(profile_links(u)) if g.lower() in logins]
        about = bool(re.search(r"xerj|corpus hub", text, re.I))
        login = ev = None
        if any(r["type"] == "retweeted" for r in p.get("referenced_tweets") or []):
            why = "retweet"
        elif un.lower() in EXCLUDE:
            why = EXCLUDE[un.lower()]
        elif un.lower() == OFFICIAL:
            why = "official post without a contributor link"
            if cp:
                login, ev = cp[0], f"featured: @xerj_org post links /contributors/{cp[0]}"
        elif cp and gh and cp[0] != gh[0]:
            why = f"conflict: links /contributors/{cp[0]}, profile links github.com/{gh[0]}"
        elif cp:
            login, ev = cp[0], f"post links /contributors/{cp[0]}" + (f"; X profile links github.com/{gh[0]}" if gh else "")
        elif gh:
            login, ev = gh[0], f"X profile links github.com/{gh[0]}"
        elif un.lower() in logins and about:
            login, ev = logins[un.lower()], f"X username equals GitHub login {logins[un.lower()]}; post is about XERJ"
        else:
            why = "no evidence"
        if login:
            out.setdefault(login, []).append({
                "id": p["id"], "url": f"https://x.com/{un}/status/{p['id']}", "author_username": un,
                "author_name": u.get("name", ""), "created_at": p["created_at"], "text": text, "match": ev})
        else:
            skipped.append((p["id"], un, why))
    return dict(sorted(out.items(), key=lambda kv: kv[0].lower())), skipped


if __name__ == "__main__":
    dump = json.load(open(sys.argv[2])) if sys.argv[1:2] == ["--from-cache"] else fetch()
    data, skipped = match(dump)
    OUT.parent.mkdir(exist_ok=True)
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    n = sum(map(len, data.values()))
    print(f"{len(dump['posts'])} posts, {n} matched to {len(data)} contributors, {len(skipped)} not attached")
    for s in skipped:
        print("  skip", *s)
