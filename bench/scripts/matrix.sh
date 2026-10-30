# Shared by run-bench.sh (Docker, one host) and run-remote.sh (three hosts):
# the workload list and its seeded shuffle, so both harnesses run the same
# matrix in the same order.
#
# Inputs:  MATRIX (unset = HTTP/CONNECT matrix, `ws` = WebSocket matrix), UP_HTTP (origin host:port, default upstream:8000), UP_SINK (sink
#          host:port, default sink:9000), BENCH_SEED, BENCH_ONLY.
# Needs:   the caller defines run_test and run_hold, and $out (the run directory, see tools/record.py).
# Output:  SEED (the seed used) and the `shuffled` array of commands to eval.

UP_HTTP="${UP_HTTP:-upstream:8000}"
UP_SINK="${UP_SINK:-sink:9000}"

matrix_select_and_shuffle() {
    local -a all
    if [[ "${MATRIX:-}" == ws ]]; then
        # WebSocket through the upgrade path (bench-client ws; origin route /ws).
        all=(
            "run_test ws-64b ws ws://$UP_HTTP/ws --conns 64 --duration 15 --size 64"
            "run_test ws-64kb ws ws://$UP_HTTP/ws --conns 8 --duration 15 --size 65536 --window 8"
            "run_test ws-short ws ws://$UP_HTTP/ws --conns 64 --duration 15 --size 64 --short"
        )
    else
    all=(
        # 1 KB keep-alive: 64 connections, so that the ceiling is not the
        # harness' own RTT (conns/latency).
        "run_test http-1kb http http://$UP_HTTP/1kb --conns 64 --duration 15"
        "run_test http-10kb http http://$UP_HTTP/10kb --conns 16 --duration 15"
        "run_test http-1mb http http://$UP_HTTP/1mb --conns 16 --duration 15"
        "run_test http-10mb http http://$UP_HTTP/10mb --conns 8 --duration 15"
        "run_test http-1kb-short http http://$UP_HTTP/1kb --conns 64 --duration 15 --no-keepalive"
        "run_test flood-8 flood $UP_SINK --conns 8 --duration 10"
        "run_hold 1000 10"
        "run_hold 5000 10"
        "run_hold 10000 10"
        "run_hold 25000 10"
    )
    fi
    local -a tests=("${all[@]}")
    # BENCH_ONLY=<test-name>: run a single step (validate/iterate cheaply —
    # a failed step is re-run alone instead of the whole matrix).
    if [[ -n "${BENCH_ONLY:-}" ]]; then
        # Outside the matrix (BENCH_ONLY only): 128 flood tunnels, beyond
        # the kernel cap on 1 MB pipes (fs.pipe-user-pages-soft).
        [[ "${MATRIX:-}" == ws ]] || all+=("run_test flood-128 flood $UP_SINK --conns 128 --duration 10")
        tests=()
        local t
        for t in "${all[@]}"; do
            if [[ "$t" == *" ${BENCH_ONLY} "* || "$t" == "run_${BENCH_ONLY} "* || "$t" == *" ${BENCH_ONLY} 10" ]]; then
                tests+=("$t")
            fi
        done
        [[ ${#tests[@]} -gt 0 ]] || { echo "no test matches BENCH_ONLY=${BENCH_ONLY}" >&2; exit 2; }
    fi
    if [[ -n "${BENCH_SEED:-}" ]]; then SEED="$BENCH_SEED"; else SEED=$RANDOM$RANDOM; fi
    echo "seed=$SEED"
    python3 tools/record.py seed "$out" "$SEED"
    # Seeded Fisher-Yates: reproducible, and removes the order bias (the
    # network is not in the same state after hold-25k as after 1kb).
    shuffled=()
    local n=${#tests[@]} i j
    for ((i=0; i<n; i++)); do
        SEED=$(( (SEED * 1103515245 + 12345) & 0x7fffffff ))
        j=$(( i + (SEED % (n - i)) ))
        shuffled[i]=${tests[j]}
        tests[j]=${tests[i]}
    done
}

# --- Concurrency sweep -------------------------------------------------------
# A fixed connection count measures the harness as much as the proxy: a
# proxy that needs many connections to fill its core (3proxy: 19 % CPU at 64
# connections, 99 % at 1024) is understated, one that degrades with
# concurrency is flattered. Every load test therefore runs at several levels;
# the tables keep the best valid level of each proxy (tools/aggregate.py).
# NO_SWEEP=1 runs only the level written in the workload list.
sweep_levels() { # <test name>
    # SWEEP_OVERRIDE="256 1024 4096": the connection levels to run for every load test (multi-core curve).
    [[ -n "${SWEEP_OVERRIDE:-}" ]] && { echo "$SWEEP_OVERRIDE"; return; }
    case "$1" in
        http-1kb|http-10kb|http-1kb-short|ws-64b) echo "64 256 1024" ;;
        http-1mb) echo "16 64 256" ;;
        http-10mb) echo "8 32 128" ;;
        flood-8|ws-64kb) echo "8 32" ;;
        ws-short) echo "64 256" ;;
        *) echo "" ;;
    esac
}

run_entry() { # <workload command>
    local entry="$1" name levels lvl
    name=$(awk '{print $2}' <<<"$entry")
    levels=$([[ "${NO_SWEEP:-0}" == 1 ]] && echo "" || sweep_levels "$name")
    if [[ -z "$levels" ]]; then eval "$entry"; return; fi
    for lvl in $levels; do
        eval "$(sed -E "s/--conns [0-9]+/--conns $lvl/" <<<"$entry")"
        sleep "${SWEEP_PAUSE:-5}"   # let TIME_WAIT sockets drain between levels
    done
}

# --- Recording ---------------------------------------------------------------
# One result = the generator's JSON document + raw counter snapshots before and
# after (taken by the caller's snap function into $raw/before and $raw/after);
# tools/record.py computes everything else (CPU per run, memory, TCP counters,
# saturation, validity) and appends the result to $out/run.json. Exit status of
# record.py: 0 recorded, 3 recorded but INVALID, 4 nothing to record.
record_add() { # <record.py add options after the run directory...>
    local rc=0
    python3 tools/record.py add "$out" --before "$raw/before" --after "$raw/after" "$@" || rc=$?
    case $rc in
        0|4) ;;
        3) INVALID=1 ;;
        *) echo "!! tools/record.py failed (status $rc)" >&2; INVALID=1 ;;
    esac
}
