#!/usr/bin/env python3
"""The GitHub stars badge in the hub nav: the GitHub mark and the star count of
xerj-org/xerj (for example 14.2k), linking to the repository.

The count in the HTML is a build-time fallback from data/xerj_stars.json, so the
badge is never empty and gen.py stays offline. In the browser, JS refreshes it
straight from api.github.com (not through the hub Worker, so it costs no Worker
requests), cached in localStorage for 10 minutes against the 60/hour
unauthenticated limit. Any failure keeps the shipped number.

Refresh the saved count (token from GH_TOKEN, GITHUB_TOKEN or `gh auth token`
if present, else unauthenticated):

    python3 tools/corpus-hub-site/gh_stars.py
"""
import datetime, json, os, subprocess, sys, urllib.request
from pathlib import Path

REPO = "xerj-org/xerj"
REPO_URL = f"https://github.com/{REPO}"
API = f"https://api.github.com/repos/{REPO}"
SAVED = Path(__file__).resolve().parent / "data/xerj_stars.json"

GH_MARK = ("M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49"
           "-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82"
           ".72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15"
           "-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82"
           " 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48"
           " 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z")


def saved_count():
    try:
        n = json.loads(SAVED.read_text(encoding="utf-8"))["stargazers_count"]
    except (OSError, ValueError, KeyError):
        return None
    return n if isinstance(n, int) and n > 0 else None


def compact(n):
    """14225 -> '14.2k', 950 -> '950', 2000 -> '2k'. Same rounding as JS below."""
    if n < 1000:
        return str(n)
    return f"{int(n / 100 + 0.5) / 10:.1f}".removesuffix(".0") + "k"


def badge(n=None):
    """Nav markup. With no saved count it is the GitHub mark alone (JS fills in a number)."""
    n = saved_count() if n is None else n
    label = f"XERJ on GitHub, {n:,} stars" if n else "XERJ on GitHub"
    txt = f"\u2605 {compact(n)}" if n else ""
    return (f'<a class="navstars" href="{REPO_URL}" title="XERJ on GitHub" aria-label="{label}" '
            f'target="_blank" rel="noopener"><svg viewBox="0 0 16 16" aria-hidden="true"><path d="{GH_MARK}"/></svg>'
            f'<span data-gh-stars="{n or 0}">{txt}</span></a>')


STYLE = """
/* GitHub stars badge (gh_stars.py): muted 1px frame like the nav CTA at rest, never
   purple, so it does not compete with the CTA; fixed-width count, no layout shift */
.nav a.navstars{display:inline-flex;align-items:center;gap:6px;align-self:center;padding:.25em .55em;
border:1px solid var(--z-line);color:var(--z-mute);white-space:nowrap;line-height:1;
font-variant-numeric:tabular-nums;letter-spacing:.04em}
.nav a.navstars:hover{color:var(--z-ink);border-color:var(--z-mute)}
.nav a.navstars svg{width:14px;height:14px;fill:currentColor;display:block;flex:none}
.nav a.navstars span{display:inline-block;min-width:7ch}
"""

JS = """<script>
(function(){
  var KEY='xerj.ghstars',TTL=600000,els=document.querySelectorAll('[data-gh-stars]');
  if(!els.length)return;
  function fmt(n){return n<1000?String(n):(Math.floor(n/100+0.5)/10).toFixed(1).replace(/\\.0$/,'')+'k'}
  function show(n){
    if(!(n>0)||n!==Math.floor(n))return;
    for(var i=0;i<els.length;i++){els[i].textContent='\\u2605 '+fmt(n);
      var a=els[i].closest('a');if(a)a.setAttribute('aria-label','XERJ on GitHub, '+n.toLocaleString('en-US')+' stars')}
  }
  var c=null;try{c=JSON.parse(localStorage.getItem(KEY)||'null')}catch(e){}
  if(c&&c.n)show(c.n);
  if((c&&Date.now()-c.t<TTL)||!window.fetch)return;
  fetch('https://api.github.com/repos/xerj-org/xerj',{headers:{Accept:'application/vnd.github+json'}})
    .then(function(r){return r.ok?r.json():null}).then(function(d){var n=d&&d.stargazers_count;if(!(n>0))return;
      show(n);try{localStorage.setItem(KEY,JSON.stringify({n:n,t:Date.now()}))}catch(e){}}).catch(function(){});
})();
</script>"""


def refresh():
    req = urllib.request.Request(API, headers={"Accept": "application/vnd.github+json", "User-Agent": "xerj-hub-site-gen"})
    tok = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not tok:
        try:
            tok = subprocess.run(["gh", "auth", "token"], capture_output=True, text=True).stdout.strip()
        except OSError:
            tok = ""
    if tok:
        req.add_header("Authorization", f"Bearer {tok}")
    with urllib.request.urlopen(req, timeout=20) as r:
        n = json.load(r)["stargazers_count"]
    if not isinstance(n, int) or n <= 0:
        raise SystemExit(f"gh_stars: unexpected stargazers_count {n!r}")
    now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
    SAVED.write_text(json.dumps({"repo": REPO, "stargazers_count": n, "source": API, "fetched_at": now},
                                indent=2) + "\n", encoding="utf-8")
    print(f"gh_stars: {n:,} stars ({compact(n)}) saved to {SAVED.name}")


if __name__ == "__main__":
    sys.exit(refresh())
