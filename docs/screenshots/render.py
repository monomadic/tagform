import json, sys, html
snap = json.load(open(sys.argv[1])); out = sys.argv[2]
ROWS = len(snap); COLS = len(snap[0])
CW, CH, FS = 8.4, 18, 14
PAD = 14
W, H = COLS*CW + PAD*2, ROWS*CH + PAD*2
NAMED = {"black":"#000000","red":"#cc0000","green":"#4e9a06","brown":"#c4a000","blue":"#3465a4","magenta":"#75507b","cyan":"#06989a","white":"#d3d7cf",
 "brightblack":"#555753","brightred":"#ef2929","brightgreen":"#8ae234","brightyellow":"#fce94f","brightblue":"#729fcf","brightmagenta":"#ad7fa8","brightcyan":"#34e2e2","brightwhite":"#eeeeec"}
def col(c, default):
    if c == "default": return default
    if c in NAMED: return NAMED[c]
    if len(c) == 6: return "#"+c
    return default
# background: most common bg of the screen
from collections import Counter
bgc = Counter(cell[2] for row in snap for cell in row).most_common(1)[0][0]
BG = col(bgc, "#101010"); FG = "#d0d0d0"
o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W:.0f}" height="{H:.0f}" viewBox="0 0 {W:.0f} {H:.0f}" font-family="JetBrains Mono, SF Mono, Menlo, DejaVu Sans Mono, monospace" font-size="{FS}">']
o.append(f'<rect width="100%" height="100%" rx="10" fill="{BG}"/>')
# backgrounds
for y,row in enumerate(snap):
    x=0
    while x < COLS:
        ch,fg,bg,b,i,u,rv = row[x]
        if rv: fg,bg = bg,fg
        x0=x
        while x < COLS and (row[x][2] if not row[x][6] else row[x][1]) == (bg) : x+=1
        c = col(bg, BG)
        if c != BG:
            o.append(f'<rect x="{PAD+x0*CW:.1f}" y="{PAD+y*CH}" width="{(x-x0)*CW:.1f}" height="{CH}" fill="{c}"/>')
# thumbnail: cells drawn with block/octant glyphs -> replace with the real frame
import base64
def isblock(ch):
    if not ch: return False
    o=ord(ch)
    return 0x2580<=o<=0x259F or 0x1FB00<=o<=0x1FBFF or 0x1CC00<=o<=0x1CEBF or 0x2500<=o<=0x257F and False
hdr=next((y for y,row in enumerate(snap) if "────" in "".join(c[0] or " " for c in row)), 0)
cells=[(x,y) for y,row in enumerate(snap) for x,c in enumerate(row) if isblock(c[0]) and y<hdr]
if cells and len(sys.argv)>3:
    xs=[c[0] for c in cells]; ys=[c[1] for c in cells]
    x0,x1,y0,y1=min(xs),max(xs)+1,min(ys),max(ys)+1
    for y in range(y0,y1):
        for x in range(x0,x1):
            snap[y][x]=(" ",snap[y][x][1],bgc,False,False,False,False)
    data=base64.b64encode(open(sys.argv[3],"rb").read()).decode()
    o.append(f'<image x="{PAD+x0*CW:.1f}" y="{PAD+y0*CH}" width="{(x1-x0)*CW:.1f}" height="{(y1-y0)*CH}" preserveAspectRatio="none" href="data:image/png;base64,{data}"/>')
# text runs
for y,row in enumerate(snap):
    x=0
    while x < COLS:
        ch,fg,bg,b,i,u,rv = row[x]
        if rv: fg,bg = bg,fg
        key=(fg,b,i,u,rv)
        x0=x; s=""
        while x < COLS:
            c2=row[x]; f2=c2[2] if c2[6] else c2[1]
            if (f2,c2[3],c2[4],c2[5],c2[6])!=key: break
            s+=c2[0] if c2[0] else " "; x+=1
        if s.strip():
            style = f'fill="{col(fg,FG)}"' + (' font-weight="bold"' if b else '') + (' font-style="italic"' if i else '') + (' text-decoration="underline"' if u else '')
            o.append(f'<text x="{PAD+x0*CW:.1f}" y="{PAD+y*CH+FS-0.5}" xml:space="preserve" {style}>{html.escape(s)}</text>')
o.append("</svg>")
open(out,"w").write("\n".join(o))
