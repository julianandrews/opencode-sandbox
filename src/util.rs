pub fn uid() -> String {
    // SAFETY: getuid is a simple syscall with no safety requirements
    // (no pointer arguments, no preconditions, always succeeds).
    unsafe { libc::getuid() }.to_string()
}

pub fn gid() -> String {
    // SAFETY: getgid is a simple syscall with no safety requirements
    // (no pointer arguments, no preconditions, always succeeds).
    unsafe { libc::getgid() }.to_string()
}

/// True when the process is not running as root. For a local Podman this is
/// the rootless/rootful split that decides whether `--userns=keep-id` is
/// needed. Remote Podman (`CONTAINER_HOST`) is not supported.
pub fn is_rootless() -> bool {
    // SAFETY: geteuid is a simple syscall with no safety requirements.
    (unsafe { libc::geteuid() }) != 0
}

/// SELinux is enforcing when `/sys/fs/selinux/enforce` is exactly "1".
/// Non-SELinux hosts (and containers without the selinuxfs mount) are false.
pub fn selinux_enforcing() -> bool {
    std::fs::read_to_string("/sys/fs/selinux/enforce")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_is_valid_number() {
        let uid = uid();
        assert!(!uid.is_empty());
        assert!(uid.parse::<u32>().is_ok(), "uid should be a valid u32");
    }

    #[test]
    fn gid_is_valid_number() {
        let gid = gid();
        assert!(!gid.is_empty());
        assert!(gid.parse::<u32>().is_ok(), "gid should be a valid u32");
    }
}
