use crate::types::ConfigError;
use std::collections::HashMap;

pub(crate) fn expand_env_vars(text: &str) -> Result<String, ConfigError> {
    if !text.contains("${") {
        return Ok(text.to_owned());
    }

    let env_vars: HashMap<String, String> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let expanded = substitute_or_preserve_unterminated(text, &env_vars)?;

    let present_vars: HashMap<String, String> = env_vars
        .keys()
        .map(|key| (key.clone(), String::new()))
        .collect();
    let masked = substitute_or_preserve_unterminated(text, &present_vars)?;
    if let Some(var_name) = first_braced_placeholder(&masked) {
        return Err(ConfigError::Validation(format!(
            "environment variable '{var_name}' is not set"
        )));
    }

    Ok(expanded)
}

fn substitute_or_preserve_unterminated(
    text: &str,
    variables: &HashMap<String, String>,
) -> Result<String, ConfigError> {
    varsubst::substitute(text, variables).or_else(|err| match err {
        varsubst::SubstError::UnclosedBrace { .. } => Ok(text.to_owned()),
        other @ varsubst::SubstError::InvalidVarName { .. } => Err(ConfigError::Validation(
            format!("invalid environment variable substitution: {other}"),
        )),
    })
}

fn first_braced_placeholder(text: &str) -> Option<&str> {
    let start = text.find("${")?.saturating_add(2);
    let end = text[start..].find('}')?;
    Some(&text[start..start + end])
}

#[cfg(test)]
mod tests {
    use super::expand_env_vars;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: tests that mutate process environment hold ENV_LOCK.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(val) => {
                    // SAFETY: tests that mutate process environment hold ENV_LOCK.
                    unsafe { std::env::set_var(self.key, val) }
                }
                None => {
                    // SAFETY: tests that mutate process environment hold ENV_LOCK.
                    unsafe { std::env::remove_var(self.key) }
                }
            }
        }
    }

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().expect("env lock should not be poisoned")
    }

    #[test]
    fn braced_env_var_expands() {
        let _guard = env_lock();
        let _var = EnvVarGuard::set("LOGFWD_ENV_TEST_BRACED", "expanded");

        let expanded =
            expand_env_vars("value=${LOGFWD_ENV_TEST_BRACED}").expect("env should expand");

        assert_eq!(expanded, "value=expanded");
    }

    #[test]
    fn missing_env_var_is_rejected() {
        let err =
            expand_env_vars("${LOGFWD_ENV_TEST_MISSING}").expect_err("missing env should fail");

        assert!(
            err.to_string().contains("LOGFWD_ENV_TEST_MISSING"),
            "error should name missing variable: {err}"
        );
    }

    #[test]
    fn unterminated_env_var_is_preserved() {
        let expanded = expand_env_vars("value=${LOGFWD_ENV_TEST_UNTERMINATED")
            .expect("unterminated variable should be preserved");

        assert_eq!(expanded, "value=${LOGFWD_ENV_TEST_UNTERMINATED");
    }

    #[test]
    fn dollar_name_syntax_is_literal() {
        let _guard = env_lock();
        let _var = EnvVarGuard::set("LOGFWD_ENV_TEST_SHORT", "expanded");

        let expanded =
            expand_env_vars("value=$LOGFWD_ENV_TEST_SHORT").expect("env should not expand");

        assert_eq!(expanded, "value=$LOGFWD_ENV_TEST_SHORT");
    }

    #[test]
    fn default_value_syntax_is_rejected() {
        let err = expand_env_vars("value=${LOGFWD_ENV_TEST_DEFAULT:fallback}")
            .expect_err("default syntax should be rejected");

        assert!(
            err.to_string()
                .contains("invalid environment variable substitution"),
            "error should describe invalid substitution: {err}"
        );
    }

    #[test]
    fn regex_backslashes_and_end_anchor_without_env_are_literal() {
        let regex = r"/([^/]+)/[^/]+\\.log$";

        let expanded = expand_env_vars(regex).expect("regex should not be treated as env syntax");

        assert_eq!(expanded, regex);
    }

    #[test]
    fn backslashes_with_env_placeholder_are_literal() {
        let _guard = env_lock();
        let _var = EnvVarGuard::set("LOGFWD_ENV_TEST_FILE", "app.log");

        let expanded = expand_env_vars(r"C:\logs\${LOGFWD_ENV_TEST_FILE}")
            .expect("backslashes should remain literal");

        assert_eq!(expanded, r"C:\logs\app.log");
    }

    #[test]
    fn env_value_containing_placeholder_text_is_not_recursively_expanded() {
        let _guard = env_lock();
        let _var = EnvVarGuard::set("LOGFWD_ENV_TEST_LITERAL", "${NOT_RECURSIVE}");

        let expanded = expand_env_vars("${LOGFWD_ENV_TEST_LITERAL}")
            .expect("env value should be inserted literally");

        assert_eq!(expanded, "${NOT_RECURSIVE}");
    }
}
