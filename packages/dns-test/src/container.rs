use core::str;
use std::fs;
use std::net::Ipv4Addr;
use std::process::{self, ExitStatus};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicUsize;
use std::sync::{atomic, Arc};

use tempfile::{NamedTempFile, TempDir};

use crate::{Error, Implementation, Result};

pub struct Container {
    inner: Arc<Inner>,
}

const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");

impl Container {
    /// Starts the container in a "parked" state
    pub fn run(implementation: Implementation) -> Result<Self> {
        // TODO make this configurable and support hickory & bind
        let dockerfile = implementation.dockerfile();
        let docker_build_dir = TempDir::new()?;
        let docker_build_dir = docker_build_dir.path();
        fs::write(docker_build_dir.join("Dockerfile"), dockerfile)?;

        let image_tag = format!("{PACKAGE_NAME}-{implementation}");

        let mut command = Command::new("docker");
        command
            .args(["build", "-t"])
            .arg(&image_tag)
            .arg(docker_build_dir);

        implementation.once().call_once(|| {
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "--- STDOUT ---\n{stdout}\n--- STDERR ---\n{stderr}"
            );
        });

        let mut command = Command::new("docker");
        let pid = process::id();
        let count = container_count();
        let name = format!("{PACKAGE_NAME}-{implementation}-{pid}-{count}");
        command
            .args(["run", "--rm", "--detach", "--name", &name])
            .arg("-it")
            .arg(image_tag)
            .args(["sleep", "infinity"]);

        let output: Output = checked_output(&mut command)?.try_into()?;
        let id = output.stdout;

        let ipv4_addr = get_ipv4_addr(&id)?;

        let inner = Inner {
            id,
            name,
            ipv4_addr,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn cp(&self, path_in_container: &str, file_contents: &str) -> Result<()> {
        const CHMOD_RW_EVERYONE: &str = "666";

        let mut temp_file = NamedTempFile::new()?;
        fs::write(&mut temp_file, file_contents)?;

        let src_path = temp_file.path().display().to_string();
        let dest_path = format!("{}:{path_in_container}", self.inner.id);

        let mut command = Command::new("docker");
        command.args(["cp", &src_path, &dest_path]);
        checked_output(&mut command)?;

        self.status_ok(&["chmod", CHMOD_RW_EVERYONE, path_in_container])?;

        Ok(())
    }

    /// Similar to `std::process::Command::output` but runs `command_and_args` in the container
    pub fn output(&self, command_and_args: &[&str]) -> Result<Output> {
        let mut command = Command::new("docker");
        command
            .args(["exec", "-t", &self.inner.id])
            .args(command_and_args);

        command.output()?.try_into()
    }

    /// Similar to `Self::output` but checks `command_and_args` ran successfully and only
    /// returns the stdout
    pub fn stdout(&self, command_and_args: &[&str]) -> Result<String> {
        let Output {
            status,
            stderr,
            stdout,
        } = self.output(command_and_args)?;

        if status.success() {
            Ok(stdout)
        } else {
            eprintln!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}");

            Err(format!("[{}] `{command_and_args:?}` failed", self.inner.name).into())
        }
    }

    /// Similar to `std::process::Command::status` but runs `command_and_args` in the container
    pub fn status(&self, command_and_args: &[&str]) -> Result<ExitStatus> {
        let mut command = Command::new("docker");
        command
            .args(["exec", "-t", &self.inner.id])
            .args(command_and_args);

        Ok(command.status()?)
    }

    /// Like `Self::status` but checks that `command_and_args` executed successfully
    pub fn status_ok(&self, command_and_args: &[&str]) -> Result<()> {
        let status = self.status(command_and_args)?;

        if status.success() {
            Ok(())
        } else {
            Err(format!("[{}] `{command_and_args:?}` failed", self.inner.name).into())
        }
    }

    pub fn spawn(&self, cmd: &[&str]) -> Result<Child> {
        let mut command = Command::new("docker");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.args(["exec", "-t", &self.inner.id]).args(cmd);

        let inner = command.spawn()?;
        Ok(Child {
            inner: Some(inner),
            _container: self.inner.clone(),
        })
    }

    pub fn ipv4_addr(&self) -> Ipv4Addr {
        self.inner.ipv4_addr
    }
}

fn container_count() -> usize {
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    COUNT.fetch_add(1, atomic::Ordering::Relaxed)
}

struct Inner {
    name: String,
    id: String,
    // TODO probably also want the IPv6 address
    ipv4_addr: Ipv4Addr,
}

/// NOTE unlike `std::process::Child`, the drop implementation of this type will `kill` the
/// child process
// this wrapper over `std::process::Child` stores a reference to the container the child process
// runs inside of, to prevent the scenario of the container being destroyed _before_
// the child is killed
pub struct Child {
    inner: Option<process::Child>,
    _container: Arc<Inner>,
}

impl Child {
    pub fn wait(mut self) -> Result<Output> {
        let output = self.inner.take().expect("unreachable").wait_with_output()?;
        output.try_into()
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            let _ = inner.kill();
        }
    }
}

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stderr: String,
    pub stdout: String,
}

impl TryFrom<process::Output> for Output {
    type Error = Error;

    fn try_from(output: process::Output) -> Result<Self> {
        let mut stderr = String::from_utf8(output.stderr)?;
        while stderr.ends_with(|c| matches!(c, '\n' | '\r')) {
            stderr.pop();
        }

        let mut stdout = String::from_utf8(output.stdout)?;
        while stdout.ends_with(|c| matches!(c, '\n' | '\r')) {
            stdout.pop();
        }

        Ok(Self {
            status: output.status,
            stderr,
            stdout,
        })
    }
}

fn checked_output(command: &mut Command) -> Result<process::Output> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!("`{command:?}` failed").into())
    }
}

fn get_ipv4_addr(container_id: &str) -> Result<Ipv4Addr> {
    let mut command = Command::new("docker");
    command
        .args([
            "inspect",
            "-f",
            "{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
        ])
        .arg(container_id);

    let output = command.output()?;
    if !output.status.success() {
        return Err(format!("`{command:?}` failed").into());
    }

    let ipv4_addr = str::from_utf8(&output.stdout)?.trim().to_string();

    Ok(ipv4_addr.parse()?)
}

// this ensures the container gets deleted and does not linger after the test runner process ends
impl Drop for Inner {
    fn drop(&mut self) {
        // running this to completion would block the current thread for several seconds so just
        // fire and forget
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_works() -> Result<()> {
        let container = Container::run(Implementation::Unbound)?;

        let output = container.output(&["true"])?;
        assert!(output.status.success());

        Ok(())
    }

    #[test]
    fn ipv4_addr_works() -> Result<()> {
        let container = Container::run(Implementation::Unbound)?;
        let ipv4_addr = container.ipv4_addr();

        let output = container.output(&["ping", "-c1", &format!("{ipv4_addr}")])?;
        assert!(output.status.success());

        Ok(())
    }

    #[test]
    fn cp_works() -> Result<()> {
        let container = Container::run(Implementation::Unbound)?;

        let path = "/tmp/somefile";
        let contents = "hello";
        container.cp(path, contents)?;

        let output = container.output(&["cat", path])?;
        dbg!(&output);

        assert!(output.status.success());
        assert_eq!(contents, output.stdout);

        Ok(())
    }
}