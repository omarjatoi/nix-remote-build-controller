//! Parsing of the controller's builder-pod flags.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

/// Validates `--builder-image-pull-policy`. An empty value means the Kubernetes
/// default and maps to `None`.
pub fn parse_pull_policy(s: &str) -> Result<Option<String>, String> {
    match s {
        "" => Ok(None),
        "Always" | "IfNotPresent" | "Never" => Ok(Some(s.to_string())),
        _ => Err(format!(
            "invalid --builder-image-pull-policy {s:?} (want Always, IfNotPresent, Never or empty)"
        )),
    }
}

/// Turns `--builder-requests` / `--builder-limits` strings such as
/// `cpu=1,memory=2Gi` into resource requirements.
pub fn parse_resources(requests: &str, limits: &str) -> Result<ResourceRequirements, String> {
    Ok(ResourceRequirements {
        requests: parse_resource_list(requests)
            .map_err(|e| format!("invalid --builder-requests: {e}"))?,
        limits: parse_resource_list(limits)
            .map_err(|e| format!("invalid --builder-limits: {e}"))?,
        ..Default::default()
    })
}

fn parse_resource_list(s: &str) -> Result<Option<BTreeMap<String, Quantity>>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let mut list = BTreeMap::new();
    for kv in s.split(',') {
        let Some((name, value)) = kv.trim().split_once('=') else {
            return Err(format!("expected name=quantity, got {kv:?}"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("expected name=quantity, got {kv:?}"));
        }
        let value = value.trim();
        if !is_valid_quantity(value) {
            return Err(format!(
                "{name}: quantities must match the regular expression '^([+-]?[0-9.]+)([eEinumkKMGTP]*[-+]?[0-9]*)$'"
            ));
        }
        list.insert(name.to_string(), Quantity(value.to_string()));
    }
    Ok(Some(list))
}

/// Reports whether `s` is a valid Kubernetes quantity: a signed decimal number
/// followed by an optional binary suffix (`Ki`..`Ei`), decimal suffix (`n`,
/// `u`, `m`, `k`, `M`..`E`) or decimal exponent (`e3`, `E-2`). This is the same
/// grammar the CRD schema and the API server enforce.
pub fn is_valid_quantity(s: &str) -> bool {
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let Some(rest) = strip_decimal(s) else {
        return false;
    };
    match rest {
        "" | "n" | "u" | "m" | "k" | "M" | "G" | "T" | "P" | "E" => true,
        "Ki" | "Mi" | "Gi" | "Ti" | "Pi" | "Ei" => true,
        exp => {
            let Some(exp) = exp.strip_prefix(['e', 'E']) else {
                return false;
            };
            let exp = exp.strip_prefix(['+', '-']).unwrap_or(exp);
            strip_decimal(exp) == Some("")
        }
    }
}

/// Strips a decimal number (`12`, `1.5`, `3.`, `.25`) from the front of `s`,
/// returning the remainder, or `None` if `s` does not start with one.
fn strip_decimal(s: &str) -> Option<&str> {
    let int_len = s.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &s[int_len..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        if int_len == 0 && frac_len == 0 {
            return None;
        }
        return Some(&after_dot[frac_len..]);
    }
    if int_len == 0 {
        return None;
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_pull_policies() {
        assert_eq!(parse_pull_policy(""), Ok(None));
        assert_eq!(parse_pull_policy("Always"), Ok(Some("Always".into())));
        assert!(parse_pull_policy("always").is_err());
    }

    #[test]
    fn parses_resource_lists() {
        let rr = parse_resources("cpu=1, memory=2Gi", "cpu=500m,memory=8Gi").unwrap();
        let requests = rr.requests.unwrap();
        assert_eq!(requests["cpu"], Quantity("1".into()));
        assert_eq!(requests["memory"], Quantity("2Gi".into()));
        assert_eq!(rr.limits.unwrap()["cpu"], Quantity("500m".into()));

        let empty = parse_resources("", " ").unwrap();
        assert!(empty.requests.is_none() && empty.limits.is_none());

        assert!(parse_resources("cpu", "").is_err());
        assert!(parse_resources("=1", "").is_err());
        assert!(parse_resources("", "memory=lots").is_err());
    }

    #[test]
    fn quantity_grammar() {
        for ok in [
            "1", "250m", "2Gi", "1.5", ".5", "3.", "1e3", "1E-2", "+4k", "-1", "100Mi", "7n",
        ] {
            assert!(is_valid_quantity(ok), "{ok} should be valid");
        }
        for bad in [
            "", "Gi", "1.5.5", "1GB", "e3", "1e", "1ki", "--1", "1 Gi", ".",
        ] {
            assert!(!is_valid_quantity(bad), "{bad} should be invalid");
        }
    }
}
