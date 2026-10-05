//! CPU affinity and connection steering (Linux; no-ops elsewhere).

/// CPUs this process may run on (honours Docker's cpuset), in order.
#[cfg(target_os = "linux")]
pub(super) fn allowed_cpus() -> Vec<usize> {
    // SAFETY: cpu_set_t is plain data; sched_getaffinity fills it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return Vec::new();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&c| libc::CPU_ISSET(c, &set))
            .collect()
    }
}

/// No CPU affinity API off Linux: `pin_cores` is ignored.
#[cfg(not(target_os = "linux"))]
pub(super) fn allowed_cpus() -> Vec<usize> {
    Vec::new()
}

#[cfg(not(target_os = "linux"))]
pub(super) fn pin_to_cpu(_cpu: usize) {}

/// Attaches a classic BPF program to the SO_REUSEPORT group of `listener`:
/// a connection whose SYN is processed on `cpus[i]` goes to the i-th socket
/// of the group (bind order = shard order). Other CPUs return an
/// out-of-range index, for which the kernel falls back to its hash.
#[cfg(target_os = "linux")]
pub(super) fn steer_by_cpu(
    listener: &std::net::TcpListener,
    cpus: &[usize],
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let insn = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    // A = current CPU.
    let mut prog = vec![insn(
        libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
        0,
        0,
        (libc::SKF_AD_OFF + libc::SKF_AD_CPU) as u32,
    )];
    for (i, &cpu) in cpus.iter().enumerate() {
        // if A == cpu { return i } (else skip the return)
        prog.push(insn(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            0,
            1,
            cpu as u32,
        ));
        prog.push(insn(libc::BPF_RET | libc::BPF_K, 0, 0, i as u32));
    }
    prog.push(insn(libc::BPF_RET | libc::BPF_K, 0, 0, u32::MAX));
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_mut_ptr(),
    };
    // SAFETY: valid listening fd; `fprog` points into `prog`, alive for
    // the call (the kernel copies the program).
    let rc = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ATTACH_REUSEPORT_CBPF,
            &fprog as *const libc::sock_fprog as *const libc::c_void,
            std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Pins the calling thread to `cpu` (best effort).
#[cfg(target_os = "linux")]
pub(super) fn pin_to_cpu(cpu: usize) {
    // SAFETY: cpu_set_t is plain data; tid 0 = calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            tracing::warn!(cpu, "sched_setaffinity failed");
        }
    }
}
