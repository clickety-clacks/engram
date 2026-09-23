//! Process launch for the SSH and test-command peer transports.

use std::ffi::OsString;
use std::io;
use std::process::{Command, Stdio};

use crate::config::TopologyPeer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
}

pub fn launch_spec(peer: &TopologyPeer) -> io::Result<LaunchSpec> {
    if let Some(argv) = &peer.command {
        let Some((program, args)) = argv.split_first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "peer command must contain an executable",
            ));
        };
        return Ok(LaunchSpec {
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
        });
    }

    let Some(destination) = peer.ssh.as_deref() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peer must configure either ssh or command transport",
        ));
    };
    if destination.is_empty() || peer.engram.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peer SSH destination and executable must be non-empty",
        ));
    }

    Ok(LaunchSpec {
        program: "ssh".into(),
        args: vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ForwardAgent=no".into(),
            "-T".into(),
            destination.into(),
            format!("exec {} peer-serve --stdio", shell_quote(&peer.engram)).into(),
        ],
    })
}

pub fn spawn(peer: &TopologyPeer) -> io::Result<std::process::Child> {
    let spec = launch_spec(peer)?;
    Command::new(spec.program)
        .args(spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_launch_uses_batch_mode_no_agent_forwarding_and_quotes_only_the_executable() {
        let peer = TopologyPeer {
            ssh: Some("eezo".into()),
            command: None,
            engram: "/opt/Engram build/it's-safe".into(),
            exports: vec!["default".into()],
        };
        let launch = launch_spec(&peer).expect("SSH launch");
        assert_eq!(launch.program, OsString::from("ssh"));
        assert_eq!(launch.args[0], "-o");
        assert_eq!(launch.args[1], "BatchMode=yes");
        assert_eq!(launch.args[2], "-o");
        assert_eq!(launch.args[3], "ForwardAgent=no");
        assert_eq!(launch.args[4], "-T");
        assert_eq!(launch.args[5], "eezo");
        assert_eq!(
            launch.args[6],
            "exec '/opt/Engram build/it'\\''s-safe' peer-serve --stdio"
        );
    }

    #[test]
    fn command_transport_preserves_argv_without_shell_reinterpretation() {
        let peer = TopologyPeer {
            ssh: None,
            command: Some(vec!["/tmp/peer helper".into(), "arg with spaces".into()]),
            engram: "/unused".into(),
            exports: vec![],
        };
        let launch = launch_spec(&peer).expect("command launch");
        assert_eq!(launch.program, OsString::from("/tmp/peer helper"));
        assert_eq!(launch.args, vec![OsString::from("arg with spaces")]);
    }
}
