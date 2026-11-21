use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// Trait for DNS resolution.
#[async_trait::async_trait]
pub trait Resolver: Send + Sync {
    /// Resolve a hostname to IP addresses.
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error>;
}

/// System resolver using the platform's default resolver.
pub struct SystemResolver;

#[async_trait::async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        let addrs = tokio::net::lookup_host(format!("{}:0", host))
            .await?
            .map(|addr| addr.ip())
            .collect::<Vec<_>>();

        if addrs.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No addresses found",
            ));
        }

        Ok(addrs)
    }
}

/// Test resolver that allows scripting responses.
#[allow(dead_code)]
pub struct TestResolver {
    responses: Mutex<HashMap<String, Vec<Vec<IpAddr>>>>,
    call_count: Mutex<HashMap<String, usize>>,
}

impl TestResolver {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(HashMap::new()),
            call_count: Mutex::new(HashMap::new()),
        }
    }

    /// Add a response sequence for a host.
    #[allow(dead_code)]
    pub fn add_response(&self, host: &str, responses: Vec<Vec<IpAddr>>) {
        self.responses
            .lock()
            .unwrap()
            .insert(host.to_string(), responses);
    }

    /// Get the number of times a host was resolved.
    #[allow(dead_code)]
    pub fn call_count(&self, host: &str) -> usize {
        self.call_count
            .lock()
            .unwrap()
            .get(host)
            .copied()
            .unwrap_or(0)
    }
}

impl Default for TestResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Resolver for TestResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        let mut responses = self.responses.lock().unwrap();
        let mut call_count = self.call_count.lock().unwrap();

        *call_count.entry(host.to_string()).or_insert(0) += 1;

        if let Some(responses_vec) = responses.get_mut(host) {
            if responses_vec.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No addresses configured",
                ));
            }
            Ok(responses_vec.remove(0))
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Host not configured",
            ))
        }
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_system_resolver() {
        let resolver = SystemResolver;
        let result = resolver.resolve("localhost").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_test_resolver() {
        let resolver = TestResolver::new();
        let public = "8.8.8.8".parse::<IpAddr>().unwrap();
        let loopback = "127.0.0.1".parse::<IpAddr>().unwrap();

        resolver.add_response("rebind.test", vec![vec![public], vec![loopback]]);

        let first = resolver.resolve("rebind.test").await.unwrap();
        assert_eq!(first, vec![public]);
        assert_eq!(resolver.call_count("rebind.test"), 1);

        let second = resolver.resolve("rebind.test").await.unwrap();
        assert_eq!(second, vec![loopback]);
        assert_eq!(resolver.call_count("rebind.test"), 2);
    }
}
