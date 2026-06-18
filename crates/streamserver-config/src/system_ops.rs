use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};

use crate::{deploy_mode, native_unit_basename, parse_env_file};

pub(crate) struct RestartTask {
    pub(crate) units: Vec<String>,
    pub(crate) label: String,
    pub(crate) receiver: Receiver<anyhow::Result<()>>,
}

impl fmt::Debug for RestartTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestartTask")
            .field("units", &self.units)
            .finish_non_exhaustive()
    }
}

pub(crate) struct UninstallTask {
    pub(crate) install_dir: PathBuf,
    pub(crate) receiver: Receiver<anyhow::Result<()>>,
}

impl fmt::Debug for UninstallTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UninstallTask")
            .field("install_dir", &self.install_dir)
            .finish_non_exhaustive()
    }
}

pub(crate) fn spawn_restart_task(units: Vec<String>) -> RestartTask {
    let (sender, receiver) = mpsc::channel();
    let task_units = units.clone();
    let label = task_units.join(", ");
    thread::spawn(move || {
        let result = restart_and_wait_instance(&task_units);
        let _ = sender.send(result);
    });
    RestartTask {
        units,
        label,
        receiver,
    }
}

pub(crate) fn spawn_uninstall_task(install_dir: PathBuf) -> UninstallTask {
    let (sender, receiver) = mpsc::channel();
    let task_install_dir = install_dir.clone();
    thread::spawn(move || {
        let result = uninstall_instance(&task_install_dir);
        let _ = sender.send(result);
    });
    UninstallTask {
        install_dir,
        receiver,
    }
}

pub(crate) fn validate_instance_dir_for_delete(install_dir: &Path) -> anyhow::Result<()> {
    let install_dir = install_dir
        .canonicalize()
        .with_context(|| format!("实例目录不存在：{}", install_dir.display()))?;
    if install_dir == Path::new("/") {
        bail!("拒绝删除根目录");
    }
    if install_dir.parent().is_none() {
        bail!("实例目录不安全，拒绝删除：{}", install_dir.display());
    }
    for required in [".env", "bin/streamserver-config"] {
        if !install_dir.join(required).exists() {
            bail!(
                "目录缺少实例标识文件 {}，拒绝删除：{}",
                required,
                install_dir.display()
            );
        }
    }
    let env_values = parse_env_file(&install_dir.join(".env"))?;
    if deploy_mode(&env_values) != "native" {
        bail!("不是 native 实例目录，拒绝删除：{}", install_dir.display());
    }
    Ok(())
}

pub(crate) fn instance_running(values: &BTreeMap<String, String>) -> bool {
    native_unit_candidates(values)
        .iter()
        .any(|unit| unit_is_active(unit))
}

pub(crate) fn native_unit_candidates(values: &BTreeMap<String, String>) -> Vec<String> {
    let mut units = Vec::new();
    if let Some(unit) = values
        .get("SYSTEMD_TARGET")
        .filter(|value| !value.trim().is_empty())
    {
        units.push(unit.clone());
    }
    for unit in native_service_unit_candidates(values) {
        if !units.contains(&unit) {
            units.push(unit);
        }
    }
    if units.is_empty() {
        units.push(format!("{}.target", native_unit_basename(values)));
    }
    units
}

pub(crate) fn native_service_unit_candidates(values: &BTreeMap<String, String>) -> Vec<String> {
    let role = values
        .get("INSTALL_ROLE")
        .map(|value| value.trim())
        .unwrap_or_default();
    let keys: &[&str] = match role {
        "control-plane" => &["SYSTEMD_POSTGRES_UNIT", "SYSTEMD_CORE_UNIT"],
        "worker-host-cpu" | "worker-host-gpu" => &["SYSTEMD_ZLM_UNIT", "SYSTEMD_AGENT_UNIT"],
        "all-in-one-host-cpu" | "all-in-one-host-gpu" => &[
            "SYSTEMD_POSTGRES_UNIT",
            "SYSTEMD_CORE_UNIT",
            "SYSTEMD_ZLM_UNIT",
            "SYSTEMD_AGENT_UNIT",
        ],
        _ => &[
            "SYSTEMD_POSTGRES_UNIT",
            "SYSTEMD_CORE_UNIT",
            "SYSTEMD_ZLM_UNIT",
            "SYSTEMD_AGENT_UNIT",
        ],
    };

    let mut units = Vec::new();
    for key in keys {
        if let Some(unit) = values.get(*key).filter(|value| !value.trim().is_empty()) {
            if !units.contains(unit) {
                units.push(unit.clone());
            }
        }
    }
    units
}

pub(crate) fn can_run_root_commands() -> bool {
    is_root()
        || Command::new("sudo")
            .args(["-n", "true"])
            .output()
            .is_ok_and(|output| output.status.success())
}

fn uninstall_instance(install_dir: &Path) -> anyhow::Result<()> {
    validate_instance_dir_for_delete(install_dir)?;

    let uninstall_script = install_dir.join("uninstall.sh");
    if uninstall_script.is_file() {
        run_root_command(
            uninstall_script.to_string_lossy().as_ref(),
            &["--purge", "--yes"],
        )?;
        return Ok(());
    }

    let env_values = parse_env_file(&install_dir.join(".env"))?;
    for unit in native_unit_candidates(&env_values) {
        let _ = run_root_command("systemctl", &["stop", &unit]);
        let _ = run_root_command("systemctl", &["disable", &unit]);
        let unit_path = Path::new("/etc/systemd/system").join(&unit);
        let _ = run_root_command("rm", &["-f", unit_path.to_string_lossy().as_ref()]);
        let _ = run_root_command("systemctl", &["reset-failed", &unit]);
    }
    run_root_command("systemctl", &["daemon-reload"])?;

    run_root_command("rm", &["-rf", install_dir.to_string_lossy().as_ref()])?;
    Ok(())
}

fn restart_and_wait_instance(units: &[String]) -> anyhow::Result<()> {
    if units.is_empty() {
        bail!("未检测到可重启的 systemd 服务");
    }
    for unit in units {
        run_root_command("systemctl", &["restart", unit])?;
        wait_for_unit_active(unit, Duration::from_secs(90))?;
    }
    Ok(())
}

fn wait_for_unit_active(unit: &str, timeout: Duration) -> anyhow::Result<()> {
    let started_at = Instant::now();
    loop {
        if unit_is_active(unit) {
            return Ok(());
        }
        if started_at.elapsed() >= timeout {
            bail!("服务 {unit} 重启后未进入运行状态");
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn unit_is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|uid| uid.trim() == "0")
}

fn run_root_command(program: &str, args: &[&str]) -> anyhow::Result<()> {
    if is_root() {
        run_command_capture(program, args, None)
    } else {
        let mut sudo_args = vec!["-n", program];
        sudo_args.extend_from_slice(args);
        run_command_capture("sudo", &sudo_args, None)
    }
}

fn run_command_capture(program: &str, args: &[&str], cwd: Option<&Path>) -> anyhow::Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .with_context(|| format!("failed to run {program}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "无输出".to_string()
    };
    let args = args.join(" ");
    bail!(
        "{program} {args} exited with status {}: {detail}",
        output.status
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_values(role: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("INSTALL_ROLE".to_string(), role.to_string()),
            (
                "SYSTEMD_TARGET".to_string(),
                "ss-example.target".to_string(),
            ),
            (
                "SYSTEMD_POSTGRES_UNIT".to_string(),
                "ss-example-postgres.service".to_string(),
            ),
            (
                "SYSTEMD_CORE_UNIT".to_string(),
                "ss-example-core.service".to_string(),
            ),
            (
                "SYSTEMD_ZLM_UNIT".to_string(),
                "ss-example-zlm.service".to_string(),
            ),
            (
                "SYSTEMD_AGENT_UNIT".to_string(),
                "ss-example-agent.service".to_string(),
            ),
        ])
    }

    #[test]
    fn worker_restart_candidates_exclude_target_and_core_units() {
        assert_eq!(
            native_service_unit_candidates(&unit_values("worker-host-cpu")),
            vec![
                "ss-example-zlm.service".to_string(),
                "ss-example-agent.service".to_string(),
            ]
        );
    }

    #[test]
    fn all_in_one_restart_candidates_follow_service_dependency_order() {
        assert_eq!(
            native_service_unit_candidates(&unit_values("all-in-one-host-cpu")),
            vec![
                "ss-example-postgres.service".to_string(),
                "ss-example-core.service".to_string(),
                "ss-example-zlm.service".to_string(),
                "ss-example-agent.service".to_string(),
            ]
        );
    }
}
