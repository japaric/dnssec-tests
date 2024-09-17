use std::{
    process::{self, Command, Stdio},
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
};

use rand::Rng;

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
        const NUM_TRIES: usize = 3;

        let count = network_count();
        let network_name = format!("{network_name}-{pid}-{count}");

        let mut rng = rand::thread_rng();
        for _ in 0..NUM_TRIES {
            // the probability this subnet collides with another network created by the framework is
            // 1/3824 = ~2.6e-4
            //
            // after 3 retries that probability of a collision is reduced to 1.78e-11
            //
            // the probably will be bigger if more than one subnet has already been created by
            // the framework but the base probability will only increase by an "N times" factor. 2
            // other networks make the probability 2/3824, 3 make it 3/3824, etc.
            //
            // creating a large Docker network _outside_ the framework can greatly increase the
            // probability of a collision. for example, `docker create --subnet 172.18.0.0/16`
            // increases the base probability to 256/3824 or 6.69e-2; after the 3 retries, the
            // probability is still high at 3e-3
            //
            // to prevent collisions with Docker networks created outside of the framework we could
            // use the private address range 10.0.0.0/8 but that can then collide with other
            // services like VPNs, wireguard, etc.
            let subnet_pick = rng.gen_range(0..SUBNET_MAX);
            let subnet = subnet(subnet_pick);

            let mut command = Command::new("docker");
            command
                .args(["network", "create"])
                .args(["--internal", "--attachable", "--subnet", &subnet])
                .arg(&network_name);

            // create network
            let output = command.output()?;

            if !output.status.success() {
                continue;
            }

            return Ok(Self {
                name: network_name,
                config: NetworkConfig { subnet },
            });
        }

        Err(format!(
            "failed to allocate a network in the address ranges
- 172.18.0.0/16 - 172.31.0.0/16 and
- 192.168.16.0/20 - 192.168.24.0/20

after {NUM_TRIES} tries"
        )
        .into())
    }
}

const SUBNET_SPLIT: u32 = (31 - 18 + 1) * 256;
const SUBNET_MAX: u32 = SUBNET_SPLIT + (255 - 16 + 1);

fn subnet(n: u32) -> String {
    assert!(n < SUBNET_MAX);

    // use subnets that `docker network create` would use like
    // - 172.18.0.0/16 .. 172.31.0.0/16
    // - 192.168.16.0/20 .. 192.168.240.0/20
    //
    // but split in smaller subnets
    //
    // on Linux, 172.17.0.0/16 is used as the default "bridge" network (see `docker network list`)
    // so we don't use that subnet
    if n < SUBNET_SPLIT {
        let a = 18 + (n / 256);
        let b = n % 256;

        format!("172.{a}.{b}.0/24")
    } else {
        let n = n - SUBNET_SPLIT;
        let a = 16 + n;

        format!("192.168.{a}.0/24")
    }
}

/// Collects all important configs.
pub struct NetworkConfig {
    /// The CIDR subnet mask, e.g. "172.21.0.0/16"
    subnet: String,
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

    #[test]
    fn subnet_works() {
        assert_eq!("172.18.0.0/24", subnet(0));
        assert_eq!("172.18.1.0/24", subnet(1));
        assert_eq!("172.31.255.0/24", subnet(14 * 256 - 1));
        assert_eq!("192.168.16.0/24", subnet(14 * 256));
        assert_eq!("192.168.17.0/24", subnet(14 * 256 + 1));
        assert_eq!("192.168.255.0/24", subnet(14 * 256 + 239));
    }

    #[test]
    #[should_panic]
    fn subnet_overflows() {
        let _boom = subnet(14 * 256 + 240);
    }
}
