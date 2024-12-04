use std::{
    net::Ipv4Addr,
    process::{self, Command, Stdio},
    sync::{
        atomic::{self, AtomicUsize},
        Arc, Mutex,
    },
};

use crate::Result;

/// Represents a network in which to put containers into.
#[derive(Clone)]
pub struct Network(Arc<NetworkInner>);

impl Network {
    /// Returns the name of the network.
    pub fn name(&self) -> &str {
        self.0.name.as_str()
    }

    /// Returns the subnet mask
    pub fn netmask(&self) -> &str {
        &self.0.config.subnet
    }
}

struct NetworkInner {
    name: String,
    config: NetworkConfig,
}

impl Network {
    pub fn new() -> Result<Self> {
        let pid = process::id();
        let network_name = env!("CARGO_PKG_NAME");
        Ok(Self(Arc::new(NetworkInner::new(pid, network_name)?)))
    }
}

/// This ensure the Docker network is deleted after the test runner process ends.
impl Drop for NetworkInner {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["network", "rm", "--force", self.name.as_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl NetworkInner {
    pub fn new(pid: u32, network_name: &str) -> Result<Self> {
        static LOCK: Mutex<()> = Mutex::new(());

        let guard = LOCK.lock()?;
        let count = network_count();
        let network_name = format!("{network_name}-{pid}-{count}");

        let in_use = in_use()?;
        let subnet = format!("{}/24", choose_network(&in_use));
        let mut command = Command::new("docker");
        command
            .args(["network", "create"])
            .args(["--internal", "--attachable", "--subnet", &subnet])
            .arg(&network_name);

        // create network
        let output = command.output()?;
        drop(guard);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(format!("--- STDOUT ---\n{stdout}\n--- STDERR ---\n{stderr}").into());
        }

        // inspect & parse network details
        let config = get_network_config(&network_name)?;

        Ok(Self {
            name: network_name,
            config,
        })
    }
}

fn choose_network(in_use: &[(Ipv4Addr, u32)]) -> Ipv4Addr {
    for c in 0..=255 {
        let candidate = Ipv4Addr::new(192, 168, c, 0);

        if !overlaps_with_any(candidate, in_use) {
            return candidate;
        }
    }

    for b in 16..=31 {
        for c in 0..=255 {
            let candidate = Ipv4Addr::new(172, b, c, 0);

            if !overlaps_with_any(candidate, in_use) {
                return candidate;
            }
        }
    }

    for b in 0..=255 {
        for c in 0..=255 {
            let candidate = Ipv4Addr::new(10, b, c, 0);

            if !overlaps_with_any(candidate, in_use) {
                return candidate;
            }
        }
    }

    // should wait until a docker network is released
    unimplemented!()
}

fn overlaps_with(lhs: Ipv4Addr, rhs: Ipv4Addr, rhs_netmask_bits: u32) -> bool {
    // LHS has netmask /24
    if rhs_netmask_bits == 24 {
        let [a1, b1, c1, _] = lhs.octets();
        let [a2, b2, c2, _] = rhs.octets();

        (a1, b1, c1) == (a2, b2, c2)
    } else if rhs_netmask_bits == 16 {
        let [a1, b1, _, _] = lhs.octets();
        let [a2, b2, _, _] = rhs.octets();

        (a1, b1) == (a2, b2)
    } else if rhs_netmask_bits == 8 {
        let [a1, _, _, _] = lhs.octets();
        let [a2, _, _, _] = rhs.octets();

        a1 == a2
    } else {
        unreachable!()
    }
}

fn overlaps_with_any(lhs: Ipv4Addr, in_use: &[(Ipv4Addr, u32)]) -> bool {
    for (rhs, rhs_netmask_bits) in in_use {
        if overlaps_with(lhs, *rhs, *rhs_netmask_bits) {
            return true;
        }
    }

    false
}

fn in_use() -> Result<Vec<(Ipv4Addr, u32)>> {
    let ifconfig = String::from_utf8(Command::new("ifconfig").output()?.stdout)?;
    let mut in_use = vec![];
    for line in ifconfig.lines() {
        if let Some((_before, after)) = line.split_once("inet ") {
            let mut parts = after.split_whitespace();
            if let (Some(ip_addr), Some(_), Some(netmask)) =
                (parts.next(), parts.next(), parts.next())
            {
                if let Ok(ip_addr) = ip_addr.parse() {
                    let netmask_bits = netmask
                        .split('.')
                        .filter_map(|part| part.parse::<u8>().ok())
                        .map(|mask| mask.count_ones())
                        .sum::<u32>();

                    in_use.push((ip_addr, netmask_bits))
                }
            }
        }
    }

    Ok(in_use)
}

/// Collects all important configs.
pub struct NetworkConfig {
    /// The CIDR subnet mask, e.g. "172.21.0.0/16"
    subnet: String,
}

/// Return network config
fn get_network_config(network_name: &str) -> Result<NetworkConfig> {
    let mut command = Command::new("docker");
    command
        .args([
            "network",
            "inspect",
            "-f",
            "{{range .IPAM.Config}}{{.Subnet}}{{end}}",
        ])
        .arg(network_name);

    let output = command.output()?;
    if !output.status.success() {
        return Err(format!("{command:?} failed").into());
    }

    let subnet = std::str::from_utf8(&output.stdout)?.trim().to_string();
    Ok(NetworkConfig { subnet })
}

fn network_count() -> usize {
    static COUNT: AtomicUsize = AtomicUsize::new(1);

    COUNT.fetch_add(1, atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use crate::container::{Container, Image};

    use super::*;

    fn exists_network(network_name: &str) -> bool {
        let mut command = Command::new("docker");
        command.args(["network", "ls", "--format={{ .Name }}"]);

        let output = command.output().expect("Failed to get output");
        let stdout = String::from_utf8_lossy(&output.stdout);

        stdout.trim().lines().any(|line| line == network_name)
    }

    #[test]
    fn create_works() -> Result<()> {
        let network = Network::new();
        assert!(network.is_ok());

        let network = network.expect("Failed to construct network");
        assert!(exists_network(network.name()));
        Ok(())
    }

    #[test]
    fn remove_network_works() -> Result<()> {
        let network = Network::new().expect("Failed to create network");
        let network_name = network.name().to_string();
        let container =
            Container::run(&Image::Client, &network).expect("Failed to start container");

        assert!(exists_network(&network_name));
        drop(network);
        assert!(exists_network(&network_name));

        drop(container);
        assert!(!exists_network(&network_name));

        Ok(())
    }

    #[test]
    fn stress() {
        let mut networks = vec![];
        for index in 0..256 {
            let network = Network::new().unwrap_or_else(|e| panic!("{}: {e}", index));
            eprintln!("{}", network.0.config.subnet);
            networks.push(network);
        }
    }
}
