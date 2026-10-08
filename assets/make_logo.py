# Renders assets/logo.txt as assets/logo.svg: each glyph placed on a fixed
# grid so box-drawing joins hold whatever monospace font the viewer has.
#   python3 assets/make_logo.py
import html, sys
lines=open('assets/logo.txt').read().rstrip('\n').split('\n')
fs=20; cw=fs*0.6; lh=fs*1.17; padx=30; pady=26
cols=max(len(l) for l in lines)
W=round(padx*2+cols*cw); H=round(pady*2+len(lines)*lh+4)
def cls(ch):
    if ch=='◉': return 'change'
    if ch=='○': return 'idle'
    if ch in '●╲╱': return 'call'
    return 'word'
out=[]
for row,l in enumerate(lines):
    y=pady+row*lh+fs*0.86
    body=l
    if row==len(lines)-1 and len(l)>17:
        out.append(f'<text class="tag" x="{padx+17*cw:.1f}" y="{y:.1f}">{html.escape(l[17:].strip())}</text>')
        body=l[:17]
    runs=[]; cur=None
    for col,ch in enumerate(body):
        if ch==' ': cur=None; continue
        c=cls(ch)
        if cur and cur[0]==c and cur[2]+len(cur[1])==col: cur[1]+=ch
        else: cur=[c,ch,col]; runs.append(cur)
    for c,text,col in runs:
        xs=' '.join(f'{padx+(col+i)*cw:.1f}' for i in range(len(text)))
        extra=' filter="url(#glow)"' if c=='change' else ''
        out.append(f'<text class="{c}"{extra} x="{xs}" y="{y:.1f}">{html.escape(text)}</text>')
svg=f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label="prognost — a prognosis for a code change. A change at the bottom, the calls up to it lit above.">
  <style>
    text {{ font-family: "SF Mono", Menlo, Consolas, "DejaVu Sans Mono", "Liberation Mono", monospace; font-size: {fs}px; white-space: pre; }}
    .call {{ fill: #6fe3f2; }}
    .idle {{ fill: #47606c; }}
    .change {{ fill: #ffb547; }}
    .word {{ fill: #e8eef2; }}
    .tag {{ fill: #8fa3ad; font-size: {round(fs*0.8)}px; }}
  </style>
  <defs>
    <filter id="glow" x="-100%" y="-100%" width="300%" height="300%">
      <feGaussianBlur stdDeviation="3" result="b"/>
      <feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
  </defs>
  <rect x="1" y="1" width="{W-2}" height="{H-2}" rx="14" fill="#0e1a22" stroke="#24404d" stroke-width="2"/>
''' + '\n'.join('  '+o for o in out) + '\n</svg>\n'
open('assets/logo.svg','w').write(svg)
print(W,H)
