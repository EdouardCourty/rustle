# Shared environment: CPU pinning auto-detected at run time.
# Source this before docker compose commands: 
#   . "$(dirname "$0")/env.sh"
#
# The exposed core layout changes between reboots ("0,2", "1,3", "0-1"):
# this reads the cgroup cpuset instead of hardcoding anything.

# Multi-core runs: PROXY_NCPU=<n> gives the proxy the first n CPUs, the
# rest go to generator/upstream/sink. PROXY_CPU / BENCH_CPU (cpuset lists)
# may also be set explicitly and are then kept as-is.
if [ -z "${_BENCH_ENV_LOADED:-}" ]; then
    _BENCH_ENV_LOADED=1
    ALL_CPUS=$(cat /sys/fs/cgroup/cpuset.cpus.effective 2>/dev/null \
        || grep Cpus_allowed_list /proc/self/status | awk '{print $2}')
    # "0-2,5" -> "0 1 2 5"
    _expand() { awk -F',' '{ for (i=1;i<=NF;i++) { split($i,a,"-"); e=(a[2]==""?a[1]:a[2]);
        for (c=a[1];c<=e;c++) printf "%s ", c } }' <<<"$1"; }
    _cpus=($(_expand "$ALL_CPUS"))
    _n=${PROXY_NCPU:-1}
    if (( _n >= ${#_cpus[@]} )); then
        echo "PROXY_NCPU=$_n leaves no CPU for the harness (${#_cpus[@]} available)" >&2
        _n=$(( ${#_cpus[@]} - 1 )); (( _n >= 1 )) || _n=1
    fi
    _join() { local IFS=,; echo "$*"; }
    PROXY_CPU=${PROXY_CPU:-$(_join "${_cpus[@]:0:_n}")}
    _rest=("${_cpus[@]:_n}")
    BENCH_CPU=${BENCH_CPU:-$(_join "${_rest[@]:-${_cpus[0]}}")}
    export PROXY_CPU BENCH_CPU ALL_CPUS
fi
