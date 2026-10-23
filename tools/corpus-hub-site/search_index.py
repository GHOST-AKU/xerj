"""search_index.py - the hub's search: one static, prebuilt index + a small script.

No server: out/search/index.json is a plain static asset (Pages serves static
files free and unlimited; no Function, no Worker request). The nav box on every
page and /search/ load it on first use and rank in the browser. Agents can fetch
the same JSON directly (llms.txt points to it).

Every value is read from the registry / contributor ledger already used by
gen.py and contributors.py; a figure we do not have is 0 or "", never estimated.

Two files, so the box answers names first and fast:
  search/index.json  names (loaded on first focus)
    {"v":1, "updated", "fields", "c":[[slug or "", repos, stars, domain, status]], "p":[[login, name, stars, corpora, kind]]}
    slug "" = derived from the first repo: lower case, "/" -> "--", "." and "_" -> "-"
    repos = "owner/repo" space-separated (<= 4; aggregate corpora list their first 4)
    stars = GitHub stars from the outreach ledger, 0 = none recorded (registry manifests carry none)
    status = registry status, or "review" for a candidate corpus in licence review
    kind = 0 signed-up contributor, 1 upstream author / curator (attribution page)
  search/more.json   keywords, same row order as "c" (loaded right after)
    {"c":[[languages, topics, use case (<= 90 chars)]]}
"""
import json
import re

FIELDS = {
    "c": ["slug", "repos", "stars", "domain", "status"],
    "more": ["langs", "topics", "usecase"],
    "p": ["login", "name", "stars", "corpora", "kind"],
}
# https://github.com/owner/name, git://, ssh://git@, and git@github.com:owner/name.
# A bare repo field ("fabric") is not an owner, so it does not match.
GH = re.compile(
    r"^(?:https?://github\.com/|git://github\.com/|ssh://git@github\.com/|git@github\.com:)([^/\s:]+/[^/#?\s]+)",
    re.I,
)


def github_owner_repo(url, repo=""):
    """`owner/name` from a GitHub remote, or from a repo field that already is one.

    A bare name (`fabric`) returns "" — the slug already says that, and a guessed
    owner would point an agent at the wrong project.
    """
    m = GH.match((url or "").strip())
    if m:
        return m.group(1).removesuffix(".git").rstrip("/")
    repo = (repo or "").strip().removesuffix(".git")
    if repo.count("/") == 1 and " " not in repo and ":" not in repo:
        return repo
    return ""


def _gh(url):
    return github_owner_repo(url, "")


def build(corpora, people, cands, upstream, vols, plain, stamp):
    rows_c, rows_p = [], []
    reg = {c["slug"]: c for c in corpora}
    # registry corpora
    for c in corpora:
        repos = [github_owner_repo(r.get("url", ""), r.get("repo", "")) for r in c.get("repos") or []]
        repos += [github_owner_repo(s.get("url", ""), s.get("repo", "") or s.get("slug", ""))
                  for s in c.get("sources") or []]
        repos = [r for r in dict.fromkeys(repos) if r][:4]
        t = c.get("tags") or {}
        rows_c.append([c["slug"], " ".join(repos), 0, c.get("cat", ""), c.get("status", ""),
                       "", " ".join(t.get("topics") or []), plain(c.get("usecase", ""))[:90]])
    # candidate corpora (author / themed): repo, stars, langs, topics from the ledger
    for slug, e in sorted(cands.items()):
        if slug in reg:
            continue
        repos, stars, langs, topics = [], 0, [], []
        for _, w in e["works"]:
            r = _gh(w.get("repo_url", "")) or w.get("repo", "")
            if r and r not in repos:
                repos.append(r)
            if isinstance(w.get("repo_stars"), int):
                stars = max(stars, w["repo_stars"])
            langs += [x for x in w.get("langs") or [] if x not in langs]
            topics += [x for x in w.get("topics") or [] if x not in topics]
        shelf = next((w.get("shelf") for _, w in e["works"] if w.get("shelf")), "")
        rows_c.append([slug, " ".join(repos[:4]), stars, shelf, "review",
                       " ".join(langs[:4]), " ".join(topics[:6]), ""])
    for login, pp in people.items():
        v = vols[login]
        rows_p.append([login, pp["name"] if pp["name"] and pp["name"] != login else "",
                       v["gh_stars"] or 0, v["corpora"], 0])
    for u in upstream.values():
        n = len({r[0] for r in u["repos"]} | set(u["curates"]))
        rows_p.append([u["login"], "", 0, n, 1])
    rows_c.sort(key=lambda r: (-r[2], r[0]))
    rows_p.sort(key=lambda r: (r[4], -r[2], r[0].lower()))
    return {"v": 1, "updated": stamp, "fields": FIELDS, "c": rows_c, "p": rows_p}


def slug_of(repo):
    return re.sub(r"[._]", "-", repo.lower().replace("/", "--"))


def write(out, idx):
    """Split build()'s rows into index.json (names) and more.json (keywords)."""
    d = out / "search"
    d.mkdir(exist_ok=True)
    names, more = [], []
    for slug, repos, stars, dom, status, langs, topics, use in idx["c"]:
        first = repos.split(" ")[0] if repos else ""
        names.append(["" if first and slug_of(first) == slug else slug, repos, stars, dom, status])
        more.append([langs, topics, use])
    dump = lambda o: json.dumps(o, separators=(",", ":"), ensure_ascii=False)
    (d / "index.json").write_text(dump({"v": 1, "updated": idx["updated"], "fields": FIELDS,
                                        "more": "more.json", "c": names, "p": idx["p"]}))
    (d / "more.json").write_text(dump({"c": more}))
    return len(names), len(idx["p"])


# ---- the browser side --------------------------------------------------------
# Nav box on every page + the /search/ page. Plain JS, no library. Loads
# search/index.json on first focus (names), then search/more.json (topics,
# languages, use case) in the background. Ranking: exact > prefix > word start
# > substring > fuzzy (letters in order), every query word must match; then
# GitHub stars and status break ties.
JS = r"""(function(){
'use strict';
var base=new URL('../search/',document.currentScript.src).href, root=new URL('../',document.currentScript.src).href;
var C=null,P=null,M=null,loading=null,T={};
window.__hubSearch=T;
T.projectStem=projectStem; T.collisionToken=collisionToken; T.score=score; T.search=search; T.row=row;
T._set=function(c,p){ C=c; P=p||[]; };
function norm(s){return (s||'').normalize('NFKD').replace(/[\u0300-\u036f]/g,'').toLowerCase();}
function slugOf(r){return r.toLowerCase().replace('/','--').replace(/[._]/g,'-');}
var PROJECT_SUFFIXES=['.js','.mjs','.cjs','.jsx','.ts','.tsx','.py','.rb','.go','.php','.rs'];
var GLUED_SUFFIXES=['js','py'];
function projectStem(token){
  var i,s,stem;
  for(i=0;i<PROJECT_SUFFIXES.length;i++){ s=PROJECT_SUFFIXES[i];
    if(token.length>s.length && token.slice(-s.length)===s){ stem=token.slice(0,-s.length);
      if(stem.length>=2 && stem.indexOf('.')<0 && stem.indexOf('/')<0) return stem; } }
  for(i=0;i<GLUED_SUFFIXES.length;i++){ s=GLUED_SUFFIXES[i];
    if(token.length<=s.length+2) continue;
    if(token.slice(-s.length)===s){ stem=token.slice(0,-s.length);
      if(/^[a-z0-9]+$/.test(stem)) return stem; } }
  return '';
}
function collisionToken(q,o){
  if(!o||o.t!=='c') return '';
  var terms=norm(q).split(/\s+/).filter(Boolean), slug=norm(o.slug);
  var bag=norm((o.repos||[]).join(' ')+' '+(o.use||''));
  for(var i=0;i<terms.length;i++){ var t=terms[i];
    if(t===slug) continue;
    if(bag && bag.indexOf(t)>=0) continue;
    if(projectStem(t)===slug) return t; }
  return '';
}
function load(){
  if(loading) return loading;
  var t0=performance.now();
  loading=fetch(base+'index.json').then(function(r){if(!r.ok) throw new Error(r.status);return r.json();}).then(function(d){
    C=d.c.map(function(r){
      var repos=r[1]?r[1].split(' '):[], slug=r[0]||slugOf(repos[0]||''), k=[norm(slug)];
      repos.forEach(function(x){var l=norm(x),i=l.indexOf('/');k.push(l,l.slice(i+1),l.slice(0,i));});
      return {t:'c',slug:slug,repos:repos,stars:r[2],dom:r[3],st:r[4],k:k,w:null};
    });
    P=d.p.map(function(r){return {t:'p',login:r[0],name:r[1],stars:r[2],n:r[3],up:r[4]===1,k:[norm(r[0]),norm(r[1])].concat(norm(r[1]).split(/\s+/).filter(Boolean))};});
    T.loadMs=Math.round(performance.now()-t0); T.rows=C.length+P.length;
    fetch(base+(d.more||'more.json')).then(function(r){return r.ok?r.json():null;}).then(function(m){
      if(!m) return; m.c.forEach(function(r,i){var c=C[i]; if(!c) return;
        c.w=norm(r[0]+' '+r[1]).split(/\s+/).filter(Boolean); c.use=r[2]; c.u=norm(r[2]).split(/[^a-z0-9+#.]+/).filter(Boolean);});
      M=1; T.moreMs=Math.round(performance.now()-t0); boxes.forEach(function(b){if(b.q) b.run();});
    }).catch(function(){});
  });
  return loading;
}
function sc(t,s){
  if(!s) return 0;
  if(s===t) return 100;
  if(s.lastIndexOf(t,0)===0) return 80;
  var i=s.indexOf(t);
  while(i>0){ if('-/_. '.indexOf(s[i-1])>=0) return 65; i=s.indexOf(t,i+1); }
  if(s.indexOf(t)>0) return 45;
  if(t.length>=3 && s.length<=t.length*3){ var j=0; for(var n=0;n<s.length&&j<t.length;n++) if(s[n]===t[j]) j++; if(j===t.length) return 15; }
  return 0;
}
function score(o,terms){
  var tot=0;
  for(var a=0;a<terms.length;a++){
    var t=terms[a],b=0,x;
    for(var n=0;n<o.k.length;n++){ x=sc(t,o.k[n]); if(x>b) b=x; if(b===100) break; }
    if(o.w&&b<35){ for(n=0;n<o.w.length;n++){ x=o.w[n]===t?35:(t.length>=2&&o.w[n].lastIndexOf(t,0)===0?25:0); if(x>b) b=x; } }
    if(o.u&&b<12&&t.length>=3){ for(n=0;n<o.u.length;n++) if(o.u[n].lastIndexOf(t,0)===0){ b=12; break; } }
    if(!b && o.t==='c'){ var stem=projectStem(t); if(stem && stem===norm(o.slug)) b=40; }
    if(!b) return 0;
    tot+=b;
  }
  tot+=Math.log10((o.stars||0)+1)*4;
  if(o.t==='c'){ if(o.st==='live') tot+=6; else if(o.st==='killed'||o.st==='withdrawn') tot-=15; }
  else if(o.up) tot-=8;
  return tot-o.k[0].length*0.05;
}
function search(q,lim){
  var terms=norm(q).split(/\s+/).filter(Boolean), t0=performance.now();
  if(!terms.length||!C) return null;
  function top(arr){var r=[];for(var i=0;i<arr.length;i++){var s=score(arr[i],terms);if(s>0) r.push([s,arr[i]]);}
    r.sort(function(a,b){return b[0]-a[0];}); return {n:r.length,top:r.length?r[0][0]:0,items:r.slice(0,lim).map(function(x){return x[1];})};}
  var res={c:top(C),p:top(P)}; T.lastQueryMs=+(performance.now()-t0).toFixed(2); return res;
}
function esc(s){return String(s).replace(/[&<>"]/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c];});}
function hl(s,q){var t=norm(q).split(/\s+/).filter(Boolean)[0]||'',i=t?norm(s).indexOf(t):-1;
  return i<0?esc(s):esc(s.slice(0,i))+'<b>'+esc(s.slice(i,i+t.length))+'</b>'+esc(s.slice(i+t.length));}
function kfmt(n){return n>=1e6?(n/1e6).toFixed(1).replace(/\.0$/,'')+'M':n>=1e3?(n/1e3).toFixed(1).replace(/\.0$/,'')+'K':String(n);}
var ST={review:'in review',live:'',candidate:'candidate',deferred:'deferred',withdrawn:'withdrawn',killed:'killed'};
function row(o,q,id){
  var h,title,sub,meta='';
  if(o.t==='c'){
    h=root+'corpus/'+encodeURIComponent(o.slug); title=hl(o.slug,q);
    var repos=o.repos||[];
    var r=repos.filter(function(x){return slugOf(x)!==o.slug;});
    var who=r.length?r[0]+(o.repos.length>1?' +'+(o.repos.length-1):''):(o.repos.length>1?o.repos.length+' repos':o.repos[0]||'');
    var note=collisionToken(q,o);
    if(note) sub='not '+esc(note)+' · '+esc(who||o.slug)+(o.use?' · '+esc(o.use):'');
    else sub=esc(who)+(o.use?(who?' · ':'')+esc(o.use):'');
    var s=ST[o.st]===undefined?o.st:ST[o.st]; if(s) meta+='<span class="hs-st">'+esc(s)+'</span>';
  } else {
    h=root+'contributors/'+encodeURIComponent(o.login); title=o.name?hl(o.name,q)+' <i>@'+hl(o.login,q)+'</i>':hl(o.login,q);
    sub=o.up?'upstream author':(o.n+' corpus'+(o.n===1?'':'es'));
  }
  if(o.stars) meta+='<span class="hs-star">★ '+kfmt(o.stars)+'</span>';
  var av=o.t==='p'?'<img class="hs-av" alt="" loading="lazy" width="20" height="20" src="https://github.com/'+encodeURIComponent(o.login)+'.png?size=40">':'<span class="hs-av hs-c" title="domain">'+esc(o.dom||'·')+'</span>';
  return '<a class="hs-row" role="option" id="'+id+'" href="'+h+'">'+av+'<span class="hs-txt"><span class="hs-t">'+title+'</span><span class="hs-s">'+sub+'</span></span><span class="hs-m">'+meta+'</span></a>';
}
var boxes=[];
function Box(input,out,full){
  var self=this,act=-1,uid='hs'+boxes.length;
  this.q='';
  out.id=out.id||uid+'-list'; input.setAttribute('aria-controls',out.id);
  this.run=function(){
    var q=self.q=input.value.trim();
    if(!q){ out.innerHTML=''; close(); return; }
    if(!C){ out.innerHTML='<div class="hs-note">Loading…</div>'; open(); load().then(self.run,function(){out.innerHTML='<div class="hs-note">Search is unavailable.</div>';}); return; }
    var r=search(q,full?40:5),h='',i=0;
    function grp(name,g){ if(!g.n) return; h+='<div class="hs-g">'+name+'<span>'+g.n.toLocaleString('en-US')+'</span></div>'; g.items.forEach(function(o){h+=row(o,q,uid+'-'+(i++));}); }
    if(r.p.top>r.c.top){ grp('People',r.p); grp('Corpora',r.c); } else { grp('Corpora',r.c); grp('People',r.p); }
    if(!i) h='<div class="hs-note">No match for “'+esc(q)+'”.</div>';
    else if(!full&&(r.c.n>5||r.p.n>5)) h+='<a class="hs-all" href="'+root+'search/?q='+encodeURIComponent(q)+'">All results</a>';
    out.innerHTML=h; act=-1; open();
    if(full){ var u=new URL(location.href); u.searchParams.set('q',q); history.replaceState(null,'',u); }
  };
  function open(){ if(!full){ out.style.setProperty('--hs-top',Math.round(input.getBoundingClientRect().bottom+6)+'px'); out.hidden=false; input.setAttribute('aria-expanded','true'); } }
  function close(){ if(!full){ out.hidden=true; input.setAttribute('aria-expanded','false'); input.removeAttribute('aria-activedescendant'); } }
  function rows(){ return out.querySelectorAll('.hs-row,.hs-all'); }
  function mark(n){ var r=rows(); if(!r.length) return; act=(n+r.length)%r.length;
    r.forEach(function(e,i){e.classList.toggle('on',i===act);}); r[act].id=r[act].id||uid+'-all'; input.setAttribute('aria-activedescendant',r[act].id); r[act].scrollIntoView({block:'nearest'}); }
  input.addEventListener('focus',function(){ load(); if(input.value.trim()) self.run(); });
  input.addEventListener('input',self.run);
  input.addEventListener('keydown',function(e){
    if(e.key==='ArrowDown'){ e.preventDefault(); mark(act+1); }
    else if(e.key==='ArrowUp'){ e.preventDefault(); mark(act-1); }
    else if(e.key==='Escape'){ close(); if(!full) input.blur(); }
    else if(e.key==='Enter'){ var r=rows(); if(act>=0&&r[act]){ e.preventDefault(); location.href=r[act].href; } else if(full){ e.preventDefault(); var f=rows()[0]; if(f) location.href=f.href; } }
  });
  if(!full) document.addEventListener('click',function(e){ if(!input.form.contains(e.target)) close(); });
  boxes.push(this);
}
document.querySelectorAll('form.hubsearch').forEach(function(f){
  var out=document.createElement('div'); out.className='hs-pop'; out.setAttribute('role','listbox'); out.hidden=true; f.appendChild(out);
  new Box(f.querySelector('input'),out,false);
});
var pg=document.getElementById('hs-page');
if(pg){ var inp=pg.querySelector('input'),res=document.getElementById('hs-res'); var b=new Box(inp,res,true);
  var q=new URLSearchParams(location.search).get('q'); if(q){ inp.value=q; } load().then(function(){ if(inp.value.trim()) b.run(); }); inp.focus(); }
document.addEventListener('keydown',function(e){
  if(e.key!=='/'||e.metaKey||e.ctrlKey||e.altKey) return;
  var a=document.activeElement; if(a&&(a.tagName==='INPUT'||a.tagName==='TEXTAREA'||a.isContentEditable)) return;
  var i=document.querySelector('#hs-page input')||document.querySelector('form.hubsearch input'); if(i){ e.preventDefault(); i.focus(); }
});
})();
"""

STYLE = """
/* search (search_index.py): nav box + /search/ */
.nav form.hubsearch{position:relative;align-self:center;margin:0;text-transform:none;letter-spacing:0;
flex:1 1 0;min-width:8rem;max-width:15rem}
.hubsearch input,#hs-page input{font:500 var(--fs-13,13px)/1.3 var(--font-data);color:var(--z-ink);background:#fff;
border:1px solid var(--z-line);padding:.42em .7em;width:100%;box-sizing:border-box;outline:none;border-radius:0;-webkit-appearance:none;appearance:none}
.hubsearch input:focus,#hs-page input:focus{border-color:var(--z-mute)}
.hubsearch input::placeholder,#hs-page input::placeholder{color:var(--z-mute);opacity:.75}
.hs-pop{position:absolute;right:0;top:calc(100% + 6px);width:26rem;max-width:calc(100vw - 24px);max-height:70vh;overflow:auto;
background:#fff;border:1px solid var(--z-line);box-shadow:0 8px 24px rgba(0,0,0,.08);z-index:60}
.hs-pop[hidden]{display:none}
.hs-g{display:flex;justify-content:space-between;padding:.7em .8em .3em;font:600 10px/1 var(--font-data);
letter-spacing:var(--track-label,.12em);text-transform:uppercase;color:var(--z-mute)}
.hs-g span{font-weight:500}
a.hs-row{display:flex;align-items:center;gap:.6em;padding:.45em .8em;color:var(--z-ink);text-decoration:none;
text-transform:none;letter-spacing:0;font:400 13px/1.3 var(--font-data)}
a.hs-row:hover,a.hs-row.on,a.hs-all.on{background:var(--z-faint)}
.hs-av{flex:none;width:20px;height:20px;border-radius:50%;background:var(--z-faint)}
.hs-av.hs-c{width:46px;height:auto;border-radius:0;background:none;border:1px solid var(--z-line);box-sizing:border-box;
font:600 9px/18px var(--font-data);letter-spacing:.06em;text-align:center;color:var(--z-mute);padding:0 3px}
.hs-txt{display:flex;flex-direction:column;min-width:0;flex:1}
.hs-t{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.hs-t{font-weight:600}.hs-t b{color:var(--z-accent)}.hs-t i{font-style:normal;font-weight:400;color:var(--z-mute)}
.hs-s{color:var(--z-mute);font-size:12px;white-space:normal;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.hs-m{flex:none;display:flex;gap:.5em;align-items:center;color:var(--z-mute);font-size:12px}
.hs-st{border:1px solid var(--z-line);padding:0 .4em;font-size:11px}
a.hs-all{display:block;padding:.6em .8em;border-top:1px solid var(--z-faint);color:var(--z-accent);text-decoration:none;
text-transform:none;letter-spacing:0;font:600 12px/1.3 var(--font-data)}
.hs-note{padding:.8em;color:var(--z-mute);text-transform:none;letter-spacing:0;font:400 13px/1.3 var(--font-data)}
#hs-page{max-width:46rem;margin:0 auto;padding:var(--sp-8,32px) var(--page-x)}
#hs-page h1{margin:0 0 .5em}
#hs-page input{width:100%;box-sizing:border-box;font-size:16px;padding:.6em .8em}
#hs-res{margin-top:var(--sp-4,16px)}
#hs-res a.hs-row{padding:.55em .2em;border-bottom:1px solid var(--z-faint)}
#hs-res .hs-g{padding:1.2em .2em .4em}
@media (max-width:900px){
.nav .brand{order:-2}
.nav form.hubsearch{order:-1;max-width:none}
.hubsearch input{font-size:16px;padding:.3em .6em}
.hs-pop{position:fixed;left:var(--page-x);right:var(--page-x);top:var(--hs-top,120px);width:auto;max-width:none;max-height:calc(100vh - var(--hs-top,120px) - 12px)}
}
"""

PAGE_BODY = """<main id="hs-page"><h1>Search</h1>
<form role="search" action="" onsubmit="return false"><input type="search" name="q" placeholder="Repo, corpus, person or topic"
aria-label="Search corpora, repositories and contributors" autocomplete="off" spellcheck="false"></form>
<div id="hs-res" role="listbox"><noscript><p>Search needs JavaScript. Browse <a href="../">the corpora</a> or
<a href="../contributors/">the contributors</a>.</p></noscript></div></main>"""

NAV_FORM = ('<form class="hubsearch" role="search" action="{rel}search/"><input type="search" name="q" '
            'placeholder="Search repos, people" aria-label="Search corpora, repositories and contributors" '
            'autocomplete="off" spellcheck="false" role="combobox" aria-expanded="false" aria-autocomplete="list"></form>')
