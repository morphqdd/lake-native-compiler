#!/usr/bin/env bash
set -e
TOTAL=10000
LEVELS="1 4 16 64 256 1024"
SERVERS=(
  "lake|/tmp/build/tcp_server|8080"
  "c-sync|/tmp/tcp_c|8083"
  "tokio|/tmp/tcp_rust/target/release/tcp_srv|8081"
  "go|/tmp/tcp_go|8082"
)
OUT=/tmp/bench_results.csv
echo "server,threads,total,success,failed,sec,rps,mean_us,p50_us,p95_us,p99_us,max_us" > $OUT
for entry in "${SERVERS[@]}"; do
  IFS='|' read -r NAME BIN PORT <<< "$entry"
  echo "=== $NAME ==="
  $BIN >/dev/null 2>&1 &
  SPID=$!
  sleep 0.4
  for c in $LEVELS; do
    LINE=$(timeout 60 /tmp/load $TOTAL $c $PORT 2>/dev/null) || LINE="$c,timeout,0,$TOTAL,60,0,0,0,0,0,0"
    echo "$NAME,$LINE" >> $OUT
    echo "  c=$c: $LINE"
  done
  kill -9 $SPID 2>/dev/null; wait 2>/dev/null
  sleep 0.3
done
echo "--- DONE ---"
