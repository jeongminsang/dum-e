use std::io;

#[cfg(unix)]
pub async fn terminate_process_group(pid: u32, grace_period_ms: u64) -> io::Result<()> {
    unsafe {
        let pgid = pid as libc::pid_t;
        // 1. Send SIGTERM to process group
        libc::kill(-pgid, libc::SIGTERM);
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(grace_period_ms)).await;

    unsafe {
        let pgid = pid as libc::pid_t;
        // 2. Send SIGKILL to ensure complete termination of process tree
        libc::kill(-pgid, libc::SIGKILL);
    }

    Ok(())
}

#[cfg(not(unix))]
pub async fn terminate_process_group(_pid: u32, _grace_period_ms: u64) -> io::Result<()> {
    Ok(())
}
