import json
d = json.load(open('gf.json'))
lst = d['familyMetadataList']
lst.sort(key=lambda e: e.get('popularity', 99999))

CAT = {'Sans Serif':'Sans','Serif':'Serif','Monospace':'Mono','Display':'Display','Handwriting':'Handwriting'}

def bucket(e):
    if any(a.get('tag')=='wght' for a in e.get('axes',[])):
        return 'VAR'
    ws=set()
    for k in e.get('fonts',{}):
        num=''.join(c for c in k if c.isdigit())
        if num: ws.add(int(num))
    if not ws: return 'STD'
    if len(ws)>=4: return 'VAR'
    if ws=={400}: return 'ONE'
    if {400,700}<=ws: return 'STD'
    return 'W359'

system = [
    ("Helvetica","Sans","ONE_STD"),("Arial","Sans","ONE_STD"),
    ("Times New Roman","Serif","ONE_STD"),("Courier New","Mono","ONE_STD"),
    ("Georgia","Serif","ONE_STD"),("Verdana","Sans","ONE_STD"),
    ("Tahoma","Sans","ONE_STD"),("Trebuchet MS","Sans","ONE_STD"),
    ("Calibri","Sans","ONE_STD"),("Cambria","Serif","ONE_STD"),
    ("Garamond","Serif","ONE_STD"),("Comic Sans MS","Handwriting","ONE_STD"),
    ("Impact","Display","ONE_ONE"),("Symbol","Sans","ONE_ONE"),("ZapfDingbats","Sans","ONE_ONE"),
]
seen=set()
lines=[]
for fam,cat,w in system:
    seen.add(fam)
    wt = 'STD' if w=='ONE_STD' else 'ONE'
    lines.append(f'    FontInfo {{ family: "{fam}", category: {cat}, google: false, weights: {wt} }},')
n_google=0
for e in lst:
    fam=e['family']
    if fam in seen: continue
    seen.add(fam)
    cat=CAT.get(e.get('category'),'Sans')
    lines.append(f'    FontInfo {{ family: "{fam}", category: {cat}, google: true, weights: {bucket(e)} }},')
    n_google+=1
open('/tmp/catalog_entries.txt','w').write('\n'.join(lines))
print(f'system={len(system)} google={n_google} total={len(system)+n_google}')
