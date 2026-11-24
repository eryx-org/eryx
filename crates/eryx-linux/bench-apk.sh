# Runs inside the unkernel guest: install real Alpine packages, then use them.
t() { echo "@@T start $1"; eval "$2" >/tmp/out 2>&1; echo "@@T end $1"; tail -2 /tmp/out; }
grep -q community /etc/apk/repositories || sudo sh -c 'sed -n "s/main$/community/p" /etc/apk/repositories | head -1 >> /etc/apk/repositories'
t apk-add 'sudo apk add -q git jq ripgrep sqlite python3 curl'
t git 'cd /tmp && git init -q r && cd r && echo hi > a && git add a && git -c user.email=a@b -c user.name=x commit -qm init && git log --oneline | wc -l; cd /tmp'
t jq 'echo "{\"a\":[1,2,3]}" | jq -c ".a|add"'
t rg 'rg -c root /etc/passwd'
t sqlite 'sqlite3 :memory: "select sqlite_version(), 1+1"'
t python-start 'python3 -c "print(42)"'
t python-sum 'python3 -c "print(sum(range(10**6)))"'
t curl 'curl -sI https://example.com | head -1'
df -h / | tail -1
echo BENCH-DONE
