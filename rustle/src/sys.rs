//! Process-level tuning shared by every command.

/// Raises the open-file soft limit to the hard limit (capped at 1 Mi):
/// a splice tunnel holds ~6 descriptors, so the usual 1024 soft default
/// would cap a deployed proxy at ~170 tunnels. Returns (before, after).
#[cfg(unix)]
pub fn raise_nofile_limit() -> Option<(u64, u64)> {
    // SAFETY: getrlimit/setrlimit on a plain-data struct we own.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return None;
        }
        let before = lim.rlim_cur as u64;
        let mut want = (lim.rlim_max as u64).min(1 << 20);
        // macOS refuses values above kern.maxfilesperproc even when the
        // hard limit is "unlimited": back off until the kernel accepts.
        while want > before {
            let new = libc::rlimit {
                rlim_cur: want as libc::rlim_t,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return Some((before, want));
            }
            want /= 2;
        }
        Some((before, before))
    }
}

#[cfg(not(unix))]
pub fn raise_nofile_limit() -> Option<(u64, u64)> {
    None
}

/// True when running as root.
#[cfg(unix)]
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
pub fn is_root() -> bool {
    false
}
