# Shared environment: CPU pinning auto-detected at run time.
# Source this before docker compose commands: 
#   . "$(dirname "$0")/env.sh"
#
# The exposed core layout changes between reboots ("0,2", "1,3", "0-1"):
# this reads the cgroup cpuset instead of hardcoding anything.

if [ -z "${_BENCH_ENV_LOADED:-}" ]; then
    _BENCH_ENV_LOADED=1
    ALL_CPUS=$(cat /sys/fs/cgroup/cpuset.cpus.effective 2>/dev/null \
        || grep Cpus_allowed_list /proc/self/status | awk '{print $2}')
    PROXY_CPU=$(awk -F',' '{split($1,a,"-"); print a[1]}' <<<"$ALL_CPUS")
    BENCH_CPU=$(awk -F',' -v skip="$PROXY_CPU" '{
        n=split($0,t,","); out="";
        for (i=1;i<=n;i++) { split(t[i],a,"-"); s=a[1]; e=(a[2]==""?s:a[2]);
            for (c=s;c<=e;c++) if (c+0!=skip+0) { out=out (out==""?"":",") c } }
        print (out==""?skip:out)
    }' <<<"$ALL_CPUS")
    export PROXY_CPU BENCH_CPU ALL_CPUS
fi
