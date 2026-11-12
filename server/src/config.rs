use anyhow::{Context, Result, bail};
use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Settings {
    pub source_dir: PathBuf,
    pub state_dir: PathBuf,
    pub export_dir: PathBuf,
    pub web_dir: PathBuf,
    pub bind: SocketAddr,
    pub app_origin: String,
    pub secure_cookie: bool,
    pub trust_proxy_headers: bool,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let source_dir = PathBuf::from(
            env::var("PLATE_REVIEW_SOURCE_DIR").context("PLATE_REVIEW_SOURCE_DIR is required")?,
        )
        .canonicalize()
        .context("Cannot access PLATE_REVIEW_SOURCE_DIR")?;
        let state_dir = PathBuf::from(
            env::var("PLATE_REVIEW_STATE_DIR").unwrap_or_else(|_| "./var/state".into()),
        );
        let export_dir = PathBuf::from(
            env::var("PLATE_REVIEW_EXPORT_DIR").context("PLATE_REVIEW_EXPORT_DIR is required")?,
        );
        let web_dir =
            PathBuf::from(env::var("PLATE_REVIEW_WEB_DIR").unwrap_or_else(|_| "./web/dist".into()));
        let bind = env::var("PLATE_REVIEW_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()
            .context("PLATE_REVIEW_BIND must be an IP address and port")?;
        let app_origin =
            env::var("PLATE_REVIEW_APP_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:5173".into());
        let secure_cookie = env_bool("PLATE_REVIEW_SECURE_COOKIE", true)?;
        let allow_tailscale_http = env_bool("PLATE_REVIEW_ALLOW_TAILSCALE_HTTP", false)?;
        validate_app_origin_with_options(&app_origin, allow_tailscale_http, secure_cookie)?;
        Ok(Self {
            source_dir,
            state_dir,
            export_dir,
            web_dir,
            bind,
            app_origin,
            secure_cookie,
            trust_proxy_headers: env_bool("PLATE_REVIEW_TRUST_PROXY_HEADERS", false)?,
        })
    }

    pub fn ensure_directories(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.state_dir)
            .context("Cannot create application state directory")?;
        std::fs::create_dir_all(&self.export_dir).context("Cannot create export directory")?;
        let database = self.state_dir.join("plate-review.sqlite");
        Ok(database)
    }
}

#[cfg(test)]
fn validate_app_origin(value: &str) -> Result<()> {
    validate_app_origin_with_options(value, false, true)
}

fn validate_app_origin_with_options(
    value: &str,
    allow_tailscale_http: bool,
    secure_cookie: bool,
) -> Result<()> {
    let uri = value
        .parse::<http::Uri>()
        .context("PLATE_REVIEW_APP_ORIGIN must be an origin URL")?;
    let local_http = uri.scheme_str() == Some("http")
        && matches!(
            uri.host(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        );
    let tailscale_http = allow_tailscale_http
        && uri.scheme_str() == Some("http")
        && uri.host().is_some_and(is_tailscale_ipv4);
    let secure_http = uri.scheme_str() == Some("https") && uri.host().is_some();
    let has_path = !uri.path().is_empty() && uri.path() != "/";
    let has_credentials = uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'));
    if (!local_http && !tailscale_http && !secure_http)
        || has_path
        || value.ends_with('/')
        || uri.query().is_some()
        || has_credentials
    {
        bail!("PLATE_REVIEW_APP_ORIGIN must be a valid HTTPS origin outside local development");
    }
    if tailscale_http && secure_cookie {
        bail!("PLATE_REVIEW_SECURE_COOKIE must be false for an HTTP Tailscale origin");
    }
    Ok(())
}

fn is_tailscale_ipv4(host: &str) -> bool {
    host.parse::<Ipv4Addr>().is_ok_and(|ip| {
        let [first, second, _, _] = ip.octets();
        first == 100 && (64..=127).contains(&second)
    })
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    match env::var(name) {
        Ok(value) if value.eq_ignore_ascii_case("true") || value == "1" => Ok(true),
        Ok(value) if value.eq_ignore_ascii_case("false") || value == "0" => Ok(false),
        Ok(_) => anyhow::bail!("{name} must be true or false"),
        Err(_) => Ok(default),
    }
}

pub fn database_path() -> PathBuf {
    PathBuf::from(env::var("PLATE_REVIEW_STATE_DIR").unwrap_or_else(|_| "./var/state".into()))
        .join("plate-review.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_loopback_http_and_public_https_origins() {
        assert!(validate_app_origin("http://127.0.0.1:5173").is_ok());
        assert!(validate_app_origin("http://localhost:8080").is_ok());
        assert!(validate_app_origin("https://review.example.org").is_ok());
    }

    #[test]
    fn rejects_non_tls_and_non_origin_urls() {
        assert!(validate_app_origin("http://review.example.org").is_err());
        assert!(validate_app_origin("http://127.0.0.1.evil.example").is_err());
        assert!(validate_app_origin("https://review.example.org/app").is_err());
        assert!(validate_app_origin("https://review.example.org/").is_err());
        assert!(validate_app_origin("https://review.example.org?next=other").is_err());
    }
}
