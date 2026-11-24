# Runs inside the unkernel guest: the same workloads as src/bin/eryx_shell.rs.
# The host times the gap between the @@T markers.
t() { echo "@@T start round$R-$1"; eval "$2" >/tmp/out 2>&1; echo "@@T end round$R-$1"; tail -1 /tmp/out; }
for R in 1 2 3; do
  t echo 'echo hi'
  t loop-20k 'i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done; echo $i'
  t sort-100k 'seq 100000 | sort -r | wc -l'
  t sha256-200k 'seq 200000 | sha256sum'
done
echo BENCH-DONE
