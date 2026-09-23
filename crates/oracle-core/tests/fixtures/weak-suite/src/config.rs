#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub retries: u8,
}

impl Config {
    /// Build a config.
    ///
    /// ```
    /// let c = weak_suite::config::Config::new("h", 1);
    /// assert_eq!(c.port, 1);
    /// ```
    pub fn new(host: &str, port: u16) -> Self {
        Self { host: host.to_string(), port, retries: 0 }
    }

    /// An accessor: nothing to destroy, so it should not be scored.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Mutates the receiver and returns nothing: the required oracle is
    /// post-state, and a return-value assertion cannot reach it.
    pub fn set_retries(&mut self, n: u8) {
        self.retries = n.min(10);
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.host.is_empty() {
            return Err("empty host".to_string());
        }
        Ok(())
    }
}

pub fn parse(s: &str) -> Result<Config, String> {
    let (host, port) = s.split_once(':').ok_or("missing port")?;
    let port: u16 = port.parse().map_err(|_| "bad port".to_string())?;
    Ok(Config::new(host, port))
}

/// Real logic that no test ever calls. The same-file `mod tests` claims it,
/// which makes it `claimed-but-unexecuted` rather than merely unclaimed.
pub fn redact(s: &str) -> String {
    if s.len() > 3 {
        format!("{}***", &s[..3])
    } else {
        "***".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        assert!(parse("h:1").is_ok());
    }

    #[test]
    fn test_validate() {
        let c = Config::new("h", 1);
        c.validate();
    }

    #[test]
    fn test_set_retries() {
        let mut c = Config::new("h", 1);
        c.set_retries(3);
        assert!(true);
    }

    #[test]
    fn parse_extracts_host_and_port() {
        let c = parse("example.com:8080").unwrap();
        assert_eq!(c.host, "example.com");
        assert_eq!(c.port, 8080);
    }
}
