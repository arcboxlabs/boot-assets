use std::process::Command;

use arcbox_boot::util::set_executable;
use fs_err as fs;

use super::{AGENT_BIN, storage_recovery_script};

const MARKER: &str = "arcbox-storage-recovery-v1";
const EARLY_BOOT: &[&str] = &[
    "mountpoint -q /proc",
    "mount -t proc proc /proc",
    "mountpoint -q /sys",
    "mount -t sysfs sysfs /sys",
    "mountpoint -q /dev",
    "mount -t devtmpfs devtmpfs /dev",
    "cat /proc/cmdline",
];

#[test]
fn missing_boot_flag_powers_off_without_executing_agent() {
    assert_recovery(
        "console=hvc0 ro",
        MARKER,
        1,
        &[
            "printf arcbox-storage-recovery: %s; powering off\\n arcbox.storage_recovery=1 is required",
            "poweroff -f",
        ],
    );
}

#[test]
fn missing_agent_marker_powers_off_without_executing_agent() {
    assert_recovery(
        "arcbox.storage_recovery=1",
        "",
        1,
        &[
            "mount -t virtiofs arcbox /arcbox",
            "grep -aFq arcbox-storage-recovery-v1 ./agent",
            "printf arcbox-storage-recovery: %s; powering off\\n guest agent does not support storage recovery",
            "poweroff -f",
        ],
    );
}

#[test]
fn accepted_recovery_executes_only_storage_recovery() {
    assert_recovery(
        "console=hvc0 arcbox.storage_recovery=1 ro",
        MARKER,
        0,
        &[
            "mount -t virtiofs arcbox /arcbox",
            "grep -aFq arcbox-storage-recovery-v1 ./agent",
            "agent storage-recovery",
        ],
    );
}

fn assert_recovery(cmdline: &str, marker: &str, exit_code: i32, expected_calls: &[&str]) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("busybox"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> trace
case "$1" in
  mountpoint) exit 1 ;;
  mount|poweroff) exit 0 ;;
  cat) printf '%s\n' "$CMDLINE" ;;
  grep) shift; exec /usr/bin/grep "$@" ;;
  printf) shift; printf "$@" ;;
  *) exit 99 ;;
esac
"#,
    )
    .unwrap();
    fs::write(
        root.join("agent"),
        format!("#!/bin/sh\n# {marker}\nprintf 'agent %s\\n' \"$*\" >> trace\n"),
    )
    .unwrap();
    for binary in ["busybox", "agent"] {
        set_executable(&root.join(binary)).unwrap();
    }

    // Redirect host-facing paths only; execute the rendered boot logic unchanged.
    let script = storage_recovery_script()
        .unwrap()
        .replace("bb=/bin/busybox", "bb=./busybox")
        .replace(AGENT_BIN, "./agent")
        .replace("/dev/console", "./console");
    fs::write(root.join("recovery.sh"), script).unwrap();
    let output = Command::new("/bin/sh")
        .arg("recovery.sh")
        .current_dir(root)
        .env_clear()
        .env("PATH", root)
        .env("CMDLINE", cmdline)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(exit_code), "{output:?}");
    let trace = fs::read_to_string(root.join("trace")).unwrap();
    let expected: Vec<_> = EARLY_BOOT.iter().chain(expected_calls).copied().collect();
    assert_eq!(trace.lines().collect::<Vec<_>>(), expected, "{output:?}");
}
