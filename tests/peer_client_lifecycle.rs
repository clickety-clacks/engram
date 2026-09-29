#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use engram::access::client::{PeerClient, PeerRequest};
use engram::config::TopologyPeer;
use serde_json::json;

struct ProcessGroupGuard(libc::pid_t);

impl ProcessGroupGuard {
    fn terminate(&self) {
        if self.0 > 0 {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn pid_from_file(path: &PathBuf) -> libc::pid_t {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(pid) = contents.trim().parse::<libc::pid_t>()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "peer did not record its pipe holder"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn peer_client_drop_does_not_join_readers_held_open_outside_peer_group() {
    let temp = tempfile::tempdir().expect("temporary lifecycle fixture");
    let marker = temp.path().join("pipe-holder-pid");
    let script = temp.path().join("peer.sh");
    fs::write(
        &script,
        r##"#!/bin/sh
marker="$1"
IFS= read -r request || exit 0
id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
# Noninteractive shells do not consistently put background jobs in their own
# process group. Start a new session explicitly so this child keeps the pipe
# open after PeerClient terminates the peer group.
python3 -c 'import os, sys, time; os.setsid(); open(sys.argv[1], "w").write(str(os.getpid())); time.sleep(60)' "$marker" &
printf 'PEER_TAIL_SENTINEL\n' >&2
printf '{"id":%s,"data":{"partial":"preserved"}}\n' "$id"
printf '{"id":%s,"end":true,"ok":true,"stats":{"done":true}}\n' "$id"
"##,
    )
    .expect("write peer fixture");

    let peer = TopologyPeer {
        ssh: None,
        command: Some(vec![
            "/bin/sh".into(),
            script.to_string_lossy().into_owned(),
            marker.to_string_lossy().into_owned(),
        ]),
        engram: "/unused".into(),
        exports: vec![],
    };
    let mut client = PeerClient::spawn(&peer).expect("spawn fixture peer");
    let responses = client.round(
        &[PeerRequest::new("probe", vec![], json!({}))],
        Duration::from_secs(2),
    );
    let pipe_holder = pid_from_file(&marker);
    let cleanup = ProcessGroupGuard(pipe_holder);
    assert_eq!(
        unsafe { libc::getpgid(pipe_holder) },
        pipe_holder,
        "fixture pipe holder must escape the peer process group"
    );
    let response = responses[0].as_ref().expect("complete peer response");
    assert_eq!(response.data[0]["partial"], "preserved");
    assert_eq!(response.stats["done"], true);

    let stderr_deadline = Instant::now() + Duration::from_secs(1);
    while !client.stderr_text().contains("PEER_TAIL_SENTINEL") && Instant::now() < stderr_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(client.stderr_text().contains("PEER_TAIL_SENTINEL"));

    let (finished_tx, finished_rx) = mpsc::channel();
    let drop_thread = thread::spawn(move || {
        drop(client);
        let _ = finished_tx.send(());
    });
    let returned_before_cleanup = finished_rx.recv_timeout(Duration::from_secs(5)).is_ok();

    cleanup.terminate();
    let returned_after_cleanup =
        returned_before_cleanup || finished_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    if returned_after_cleanup {
        drop_thread.join().expect("PeerClient drop thread");
    } else {
        drop(drop_thread);
    }

    assert!(
        returned_before_cleanup,
        "PeerClient teardown waited for pipe EOF after terminating its process group"
    );
    assert!(
        returned_after_cleanup,
        "PeerClient teardown did not finish after pipe cleanup"
    );
}
