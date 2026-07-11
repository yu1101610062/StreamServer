const DEFAULT_TEST_DATABASE_URL: &str = "postgresql://postgres:test@127.0.0.1/postgres";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TestDatabaseConfig {
    pub(crate) required: bool,
    pub(crate) admin_url: String,
}

pub(crate) fn config_from_values(
    require_value: Option<&str>,
    database_url: Option<&str>,
) -> anyhow::Result<TestDatabaseConfig> {
    let required = match require_value.map(str::trim) {
        None | Some("") | Some("0") => false,
        Some("1") => true,
        Some(value) => anyhow::bail!("REQUIRE_TEST_DATABASE must be 0 or 1, got {value:?}"),
    };
    let configured_url = database_url
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let admin_url = match (required, configured_url) {
        (true, None) => {
            anyhow::bail!("TEST_DATABASE_URL must be configured when REQUIRE_TEST_DATABASE=1")
        }
        (_, Some(value)) => value.to_string(),
        (false, None) => DEFAULT_TEST_DATABASE_URL.to_string(),
    };

    Ok(TestDatabaseConfig {
        required,
        admin_url,
    })
}

pub(crate) fn config_from_env() -> anyhow::Result<TestDatabaseConfig> {
    let require_value = std::env::var("REQUIRE_TEST_DATABASE").ok();
    let database_url = std::env::var("TEST_DATABASE_URL").ok();
    config_from_values(require_value.as_deref(), database_url.as_deref())
}

pub(crate) fn finish_setup<T>(
    required: bool,
    result: anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    match result {
        Ok(database) => Ok(Some(database)),
        Err(error) if required => Err(anyhow::anyhow!(
            "required test database setup failed: {error:#}"
        )),
        Err(error) => {
            eprintln!("skipping database-backed test: {error:#}");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{config_from_values, finish_setup};

    #[test]
    fn required_mode_rejects_a_missing_database_url() {
        let error = config_from_values(Some("1"), None)
            .expect_err("required database mode must reject a missing URL");

        assert!(error.to_string().contains("TEST_DATABASE_URL"));
    }

    #[test]
    fn required_mode_propagates_database_setup_errors() {
        for failure in ["database creation failed", "migration failed"] {
            let result: anyhow::Result<Option<()>> =
                finish_setup(true, Err(anyhow::anyhow!(failure)));

            let error = result.expect_err("required database setup must fail the test");
            assert!(error.to_string().contains(failure));
        }
    }

    #[test]
    fn optional_mode_keeps_local_database_skips() {
        let result: anyhow::Result<Option<()>> =
            finish_setup(false, Err(anyhow::anyhow!("database unavailable")));

        assert!(result.expect("optional setup may skip").is_none());
    }
}
