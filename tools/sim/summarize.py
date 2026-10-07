import re,sys,statistics as st
for name in sys.argv[1:]:
    txt=re.sub(r'\x1b\[[0-9;]*m','',open(f"{name}.viewer.log").read())
    rows=[l for l in txt.splitlines() if "stream fps=" in l][3:]  # skip warm-up
    if not rows: print(name,"no data"); continue
    g=lambda k:[float(re.search(k+r'=([\d.]+)',r).group(1)) for r in rows]
    gap=[list(map(float,re.search(r'gap_ms=([\d./]+)',r).group(1).split('/'))) for r in rows]
    st_=[int(re.search(r'stutters=(\d+)/',r).group(1)) for r in rows]
    fps=g('fps'); loss=g('loss')
    print(f"{name:22} fps avg {st.mean(fps):6.1f} min {min(fps):4.0f} | gap p50 {st.mean(x[0] for x in gap):5.1f} p99 {st.mean(x[1] for x in gap):5.1f} worst {max(x[2] for x in gap):6.1f} ms | stutters/s {st.mean(st_):4.1f} | loss {st.mean(loss):4.2f}% | rtt {st.mean(g('rtt_ms')):5.1f} | mbps {st.mean(g('mbps')):5.1f}")
