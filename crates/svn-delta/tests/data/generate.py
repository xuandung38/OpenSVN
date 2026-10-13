import os, random, re, subprocess, sys, hashlib
random.seed(7)
words=["alpha","beta","gamma","delta","epsilon","zeta","eta","theta","iota","kappa"]
lines=[f"line {i} {' '.join(random.choice(words) for _ in range(6))}\n" for i in range(2800)]
v=[]
v.append("".join(lines))
l2=list(lines); l2[1400]="CHANGED MIDDLE\n"; l2.insert(10,"inserted early\n"); v.append("".join(l2))
l3=l2+["tail one\n","tail two\n"]; v.append("".join(l3))
OUT=sys.argv[1]
man=[]
def run(*a, **k): return subprocess.run(a, check=True, capture_output=True, **k).stdout
for mode in ["none","zlib","lz4"]:
    repo=f"{os.getcwd()}/repo-{mode}"; wc=f"{os.getcwd()}/wc-{mode}"
    run("svnadmin","create",repo)
    p=repo+"/db/fsfs.conf"; t=open(p).read().replace("# compression = lz4",f"compression = {mode}"); open(p,"w").write(t)
    run("svn","co","-q","file://"+repo,wc)
    for i,txt in enumerate(v):
        open(wc+"/big.txt","w").write(txt)
        if i==0: run("svn","add","-q",wc+"/big.txt")
        run("svn","ci","-q","-m","r",wc)
    for i,txt in enumerate(v,1):
        if mode=="none": open(f"{OUT}/text-r{i}.txt","w").write(txt)
    for rev in (1,2,3):
        rd=open(f"{repo}/db/revs/0/{rev}","rb").read()
        for m in re.finditer(rb"text: (\d+) (\d+) (\d+) (\d+) ([0-9a-f]{32})",rd):
            r,off,size,exp,md5=int(m[1]),int(m[2]),int(m[3]),int(m[4]),m[5].decode()
            if r!=rev or exp<1000: continue
            hdr_end=rd.index(b"\n",off)
            body=rd[hdr_end+1:hdr_end+1+size]
            assert body[:3]==b"SVN" and hashlib.md5(v[rev-1].encode()).hexdigest()==md5
            open(f"{OUT}/{mode}-r{rev}.svndiff","wb").write(body)
            man.append(f"{mode}-r{rev}.svndiff {'-' if rev==1 else f'text-r{rev-1}.txt'} text-r{rev}.txt {md5} {exp}")
open(f"{OUT}/manifest.txt","w").write("# svndiff-file base-file expected-file md5 length\n"+"\n".join(man)+"\n")
