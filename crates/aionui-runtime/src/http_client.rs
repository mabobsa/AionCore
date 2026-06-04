use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::debug;

const EXTRA_CA_CERTS_ENV: &str = "AIONUI_EXTRA_CA_CERTS";
const DEFAULT_RUNTIME_USER_AGENT: &str = concat!("aioncore/", env!("CARGO_PKG_VERSION"));

pub fn build_http_client(connect_timeout: Duration, timeout: Duration) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .user_agent(DEFAULT_RUNTIME_USER_AGENT);

    let extra_root_certificates = load_extra_root_certificates()?;
    if !extra_root_certificates.is_empty() {
        debug!(
            cert_count = extra_root_certificates.len(),
            env_var = EXTRA_CA_CERTS_ENV,
            "loaded extra CA certificates for runtime HTTP client"
        );
    }
    for certificate in extra_root_certificates {
        builder = builder.add_root_certificate(certificate);
    }

    builder.build().map_err(|error| format!("build http client: {error}"))
}

fn load_extra_root_certificates() -> Result<Vec<reqwest::Certificate>, String> {
    let Some(path) = extra_ca_certs_path() else {
        return Ok(vec![]);
    };
    load_extra_root_certificates_from_path(&path)
}

fn extra_ca_certs_path() -> Option<PathBuf> {
    let value = std::env::var_os(EXTRA_CA_CERTS_ENV)?;
    let trimmed = value.to_string_lossy().trim().to_owned();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn load_extra_root_certificates_from_path(path: &Path) -> Result<Vec<reqwest::Certificate>, String> {
    let pem_bundle =
        fs::read(path).map_err(|error| format!("read {EXTRA_CA_CERTS_ENV} from {}: {error}", path.display()))?;
    let certificates = reqwest::Certificate::from_pem_bundle(&pem_bundle)
        .map_err(|error| format!("parse {EXTRA_CA_CERTS_ENV} from {}: {error}", path.display()))?;
    if certificates.is_empty() {
        return Err(format!(
            "parse {EXTRA_CA_CERTS_ENV} from {}: no certificates found",
            path.display()
        ));
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;

    use super::load_extra_root_certificates_from_path;

    const TEST_CA_PEM: &[u8] = br#"
-----BEGIN CERTIFICATE-----
MIIBtjCCAVugAwIBAgITBmyf1XSXNmY/Owua2eiedgPySjAKBggqhkjOPQQDAjA5
MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
Um9vdCBDQSAzMB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
Q0EgMzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCmXp8ZBf8ANm+gBG1bG8lKl
ui2yEujSLtf6ycXYqm0fc4E7O5hrOXwzpcVOho6AF2hiRVd9RFgdszflZwjrZt6j
QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGGMB0GA1UdDgQWBBSr
ttvXBp43rDCGB5Fwx5zEGbF4wDAKBggqhkjOPQQDAgNJADBGAiEA4IWSoxe3jfkr
BqWTrBqYaGFy+uGh0PsceGCmQ5nFuMQCIQCcAu/xlJyzlvnrxir4tiz+OpAUFteM
YyRIHN8wfdVoOw==
-----END CERTIFICATE-----
"#;

    #[test]
    fn load_extra_root_certificates_accepts_valid_pem_bundle() {
        let file = NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), TEST_CA_PEM).expect("write cert");

        let certificates = load_extra_root_certificates_from_path(file.path()).expect("certificates");

        assert_eq!(certificates.len(), 1);
    }

    #[test]
    fn load_extra_root_certificates_reports_missing_file() {
        let path = std::path::Path::new("/tmp/this-file-should-not-exist-aionui-extra-ca.pem");
        let error = load_extra_root_certificates_from_path(path).expect_err("missing file should fail");
        assert!(error.contains("AIONUI_EXTRA_CA_CERTS"));
        assert!(error.contains("No such file") || error.contains("os error"));
    }

    #[test]
    fn load_extra_root_certificates_reports_invalid_pem() {
        let file = NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), b"not a pem").expect("write invalid cert");

        let error = load_extra_root_certificates_from_path(file.path()).expect_err("invalid pem should fail");

        assert!(error.contains("AIONUI_EXTRA_CA_CERTS"));
        assert!(error.contains("parse"));
    }
}
