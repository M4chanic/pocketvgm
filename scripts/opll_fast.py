#!/usr/bin/env python3
"""Быстрая проверка OPLL (YM2413/VRC7) без большого стенда.

Большой стенд на тактовой железа считает секунду звука четыре минуты.
Здесь та же цепочка, но напрямую: регистры OPLL из VGM -> транслятор
(sim/opll_tb) -> ядро OPL3 (sim/opl3_tb) -> raw; эталон — vgm2wav того же
файла без APU. Секунда звука считается секунды. Абсолютный уровень здесь
не сравнивается (у libvgm его нет), только полосы и огибающая.

    make -C sim/opll_tb && make -C sim/opl3_tb
    python3 scripts/opll_fast.py файл.vgz 3            весь OPLL, 3 секунды
    python3 scripts/opll_fast.py файл.vgz 3 0b000100   только канал 2
    python3 scripts/opll_fast.py файл.vgz 3 0x3ff      бит 9 — ритм-секция YM2413
"""
import sys, gzip, struct, subprocess, math, wave, os, tempfile
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
S = tempfile.gettempdir()
sys.path.insert(0, os.path.join(ROOT, "scripts"))
import ab_suite
ab_suite.find_tools()
VGM2WAV = ab_suite.VGM2WAV
if not VGM2WAV:
    sys.exit("нет vgm2wav — соберите libvgm, см. шапку ab_compare.py")
src=sys.argv[1]; secs=float(sys.argv[2]); chmask=int(sys.argv[3],0) if len(sys.argv)>3 else 0x3ff
d=bytearray(gzip.open(src).read() if src.endswith('z') else open(src,'rb').read())
vrc7=(struct.unpack_from('<I',d,0x10)[0]>>31)&1
off=0x34+struct.unpack_from('<I',d,0x34)[0]
lines=[]; out=bytearray(d[:off]); i=off; t=0; pend=0
def flush():
    global pend
    if pend: lines.append('wait %d'%round(pend*49716/44100)); pend=0
while i<len(d) and t<secs*44100:
    b=d[i]
    if b==0x66: break
    if b==0x51:
        a,v=d[i+1],d[i+2]; keep=True
        if a>=0x10 and a<=0x38 and (a&0x0f)<9 and not (chmask>>(a&0x0f))&1: keep=False
        if a==0x0E and not (chmask>>9)&1: keep=False   # бит 9 маски — ритм-секция
        if keep: flush(); lines.append('%02x %02x'%(a,v)); out+=d[i:i+3]
        i+=3
    elif b==0x61: n=struct.unpack_from('<H',d,i+1)[0]; t+=n; pend+=n; out+=d[i:i+3]; i+=3
    elif b==0x62: t+=735; pend+=735; out+=d[i:i+1]; i+=1
    elif b==0x63: t+=882; pend+=882; out+=d[i:i+1]; i+=1
    elif 0x70<=b<=0x7f: n=(b&15)+1; t+=n; pend+=n; out+=d[i:i+1]; i+=1
    elif b==0x67: n=7+struct.unpack_from('<I',d,i+3)[0]; out+=d[i:i+n]; i+=n
    else:
        n=1 if b<0x30 else 2 if b<0x50 else 3
        if b==0xB4: pass
        else: out+=d[i:i+n]
        i+=n
flush(); out.append(0x66); struct.pack_into('<I',out,4,len(out)-4)
if off>=0x88: struct.pack_into('<I',out,0x84,0)   # без APU (у эталона он даёт постоянную составляющую); у коротких заголовков поля нет
open(f'{S}/of.vgm','wb').write(out)
subprocess.run([str(VGM2WAV),'--samplerate','48000',f'{S}/of.vgm',f'{S}/of_ref.wav'],capture_output=True)
p1=subprocess.run([f'{ROOT}/sim/opll_tb/opll_tb',str(vrc7),'step','q'],input='\n'.join(lines)+'\n',capture_output=True,text=True)
p2=subprocess.run([f'{ROOT}/sim/opl3_tb/opl3_tb',f'{S}/of_our.raw'],input=p1.stdout,capture_output=True,text=True)
raw=open(f'{S}/of_our.raw','rb').read(); our=struct.unpack('<%dh'%(len(raw)//2),raw); ro=49716
w=wave.open(f'{S}/of_ref.wav'); rr=w.getframerate(); ch=w.getnchannels(); n=w.getnframes()
ref=struct.unpack('<%dh'%(n*ch),w.readframes(n))[::ch]
def goertzel(s, r, f):
    ww=2*math.cos(2*math.pi*f/r); s1=s2=0.0
    for x in s:
        s0=x+ww*s1-s2; s2,s1=s1,s0
    return s1*s1+s2*s2-ww*s1*s2
BANDS=[(40,80),(80,160),(160,320),(320,640),(640,1250),(1250,2500),(2500,5000),(5000,10000)]
def bands(x,r):
    x=x[int(0.2*r):int(min(secs,len(x)/r)*r)]; m=sum(x)/len(x); x=[v-m for v in x]
    e=[]
    for lo,hi in BANDS:
        f=lo; s=0.0
        while f<hi: s+=goertzel(x,r,f); f*=2**(1/6)
        e.append(s)
    tot=sum(e) or 1
    return [v/tot for v in e], math.sqrt(sum(v*v for v in x)/len(x))
def env(x,r,step=0.125):
    m=sum(x)/len(x); x=[v-m for v in x]
    return [math.sqrt(sum(v*v for v in x[i:i+int(r*step)])/int(r*step)) for i in range(0,len(x)-int(r*step),int(r*step))]
eb,er=bands(ref,rr); ob,orr=bands(our,ro)
print('RMS эталон %.0f, наш(raw>>8) %.0f'%(er,orr))
for (lo,hi),a,b in zip(BANDS,eb,ob):
    print('  %5d-%-5d эталон %5.1f%%  наш %5.1f%%  %+6.1f дБ'%(lo,hi,a*100,b*100,10*math.log10((b+1e-9)/(a+1e-9))))
ee=env(ref,rr); oe=env(our,ro); k=min(len(ee),len(oe))
me=sum(ee[:k])/k; mo=sum(oe[:k])/k
c=sum((a-me)*(b-mo) for a,b in zip(ee,oe))/math.sqrt(sum((a-me)**2 for a in ee[:k])*sum((b-mo)**2 for b in oe[:k])+1e-9)
print('огибающая: корреляция %+.2f'%c)
print('  эталон:', ' '.join('%.0f'%v for v in ee[:k]))
print('  наш:   ', ' '.join('%.0f'%v for v in oe[:k]))
