//! CI-only TLS boundary checks against an empty database.
//! No test binary or successful skip when configuration is absent.
use anyhow::{Context, Result, ensure};
use dagq::domain::store_setting::ConnectionString;
use dagq::infrastructure::{
    deployment_config::{DATABASE_CA_FILE, database_ca},
    postgres::client,
};

fn main() -> Result<()> {
    let url = std::env::var("DAGQ_TEST_POSTGRES_TLS_URL").context("TLS URL is required")?;
    let wrong_host =
        std::env::var("DAGQ_TEST_POSTGRES_WRONG_HOST_URL").context("wrong-host URL is required")?;
    let plaintext =
        std::env::var("DAGQ_TEST_POSTGRES_PLAIN_URL").context("plain URL is required")?;
    let ca = database_ca(
        Some("postgres"),
        std::env::var_os(DATABASE_CA_FILE),
        |path| std::fs::read(path).map_err(|_| ()),
    )
    .map_err(|reason| anyhow::anyhow!("{DATABASE_CA_FILE}: {reason}"))?
    .context("CA file is required")?;
    // The CI database starts empty; prepare its schema through trusted TLS
    // so subsequent connection failures cannot be due to missing tables.
    client::migrate(&ConnectionString::new(&url).with_ca_certificates(ca.clone()))
        .context("trusted TLS migration must work")?;
    // Connect also runs SET TIME ZONE, proving a usable authenticated session.
    for suffix in ["", "?sslmode=prefer", "?sslmode=require"] {
        client::connect(
            &ConnectionString::new(format!("{url}{suffix}")).with_ca_certificates(ca.clone()),
        )
        .context("trusted TLS connection must work")?;
    }
    ensure!(
        client::connect(&ConnectionString::new(&url)).is_err(),
        "untrusted CA was accepted"
    );
    ensure!(
        client::connect(&ConnectionString::new(wrong_host).with_ca_certificates(ca.clone()))
            .is_err(),
        "wrong hostname was accepted"
    );
    ensure!(
        client::connect(&ConnectionString::new(plaintext).with_ca_certificates(ca)).is_err(),
        "TLS silently fell back to plaintext"
    );
    Ok(())
}
