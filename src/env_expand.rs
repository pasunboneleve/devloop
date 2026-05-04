use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};

pub(crate) fn expand_value(value: &str, field: &str) -> Result<String> {
    let mut rendered = String::with_capacity(value.len());
    let mut chars = value.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        if ch != '$' {
            rendered.push(ch);
            continue;
        }

        let Some((_, next)) = chars.peek().copied() else {
            return Err(anyhow!("{field} contains trailing '$'"));
        };

        if next == '$' {
            chars.next();
            rendered.push('$');
            continue;
        }

        let name = parse_reference_name(field, next, &mut chars)?;
        let replacement = std::env::var(&name)
            .with_context(|| format!("{field} references missing environment variable '{name}'"))?;
        rendered.push_str(&replacement);
    }

    Ok(rendered)
}

pub(crate) fn expand_vec(values: &[String], field: &str) -> Result<Vec<String>> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| expand_value(value, &format!("{field}[{index}]")))
        .collect()
}

pub(crate) fn expand_map(
    values: &BTreeMap<String, String>,
    field: &str,
) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|(key, value)| Ok((key.clone(), expand_value(value, &format!("{field}.{key}"))?)))
        .collect()
}

fn parse_reference_name(
    field: &str,
    first: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Result<String> {
    if first == '{' {
        chars.next();
        let mut name = String::new();
        let mut closed = false;
        for (_, candidate) in chars.by_ref() {
            if candidate == '}' {
                closed = true;
                break;
            }
            name.push(candidate);
        }
        if !closed {
            return Err(anyhow!("{field} contains unclosed environment reference"));
        }
        if !is_valid_env_name(&name) {
            return Err(anyhow!(
                "{field} contains invalid environment variable name '{name}'"
            ));
        }
        return Ok(name);
    }

    if is_env_name_start(first) {
        let mut name = String::new();
        while let Some((_, candidate)) = chars.peek().copied() {
            if !is_env_name_continue(candidate) {
                break;
            }
            chars.next();
            name.push(candidate);
        }
        return Ok(name);
    }

    Err(anyhow!(
        "{field} contains invalid environment reference '${first}'"
    ))
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_env_name_start) && chars.all(is_env_name_continue)
}

fn is_env_name_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_env_name_continue(ch: char) -> bool {
    is_env_name_start(ch) || ch.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::{expand_map, expand_value, expand_vec};
    use crate::test_support::EnvVarGuard;
    use std::collections::BTreeMap;

    #[test]
    fn expands_bare_environment_references() {
        let _guard = EnvVarGuard::set("CONTAINER_PORT", Some("18080"));

        assert_eq!(
            expand_value("http://127.0.0.1:$CONTAINER_PORT/", "probe.url").expect("expand value"),
            "http://127.0.0.1:18080/"
        );
    }

    #[test]
    fn expands_braced_environment_references() {
        let _guard = EnvVarGuard::set("CONTAINER_PORT", Some("18080"));

        assert_eq!(
            expand_value("${CONTAINER_PORT}", "env.PORT").expect("expand value"),
            "18080"
        );
    }

    #[test]
    fn keeps_literals_without_references() {
        assert_eq!(
            expand_value("http://127.0.0.1:8080/", "probe.url").expect("expand value"),
            "http://127.0.0.1:8080/"
        );
    }

    #[test]
    fn escapes_literal_dollars() {
        let _guard = EnvVarGuard::set("NAME", Some("value"));

        assert_eq!(
            expand_value("cost=$$5 name=$NAME", "command[1]").expect("expand value"),
            "cost=$5 name=value"
        );
    }

    #[test]
    fn reports_missing_environment_variables_with_context() {
        let _guard = EnvVarGuard::set("MISSING_DEVLOOP_TEST_VAR", None);
        let error =
            expand_value("$MISSING_DEVLOOP_TEST_VAR", "command[1]").expect_err("missing var");

        assert!(error.to_string().contains("command[1]"));
        assert!(error.to_string().contains("MISSING_DEVLOOP_TEST_VAR"));
    }

    #[test]
    fn reports_malformed_references() {
        let error = expand_value("${", "env.PORT").expect_err("malformed var");

        assert!(error.to_string().contains("unclosed environment reference"));
    }

    #[test]
    fn expands_vectors_with_index_context() {
        let _guard = EnvVarGuard::set("CONTAINER_PORT", Some("18080"));
        let expanded = expand_vec(
            &[
                "cloudflared".into(),
                "http://127.0.0.1:$CONTAINER_PORT".into(),
            ],
            "command",
        )
        .expect("expand vec");

        assert_eq!(expanded, vec!["cloudflared", "http://127.0.0.1:18080"]);
    }

    #[test]
    fn expands_maps_with_key_context() {
        let _guard = EnvVarGuard::set("CONTAINER_PORT", Some("18080"));
        let expanded = expand_map(
            &BTreeMap::from([("PORT".into(), "$CONTAINER_PORT".into())]),
            "env",
        )
        .expect("expand map");

        assert_eq!(expanded["PORT"], "18080");
    }
}
