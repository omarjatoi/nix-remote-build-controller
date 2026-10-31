//! Error reports for the command line and logs.

/// Renders an error and its causes as `outer: inner: root`, skipping causes
/// whose text the message already contains (kube and russh errors embed their
/// sources in their own message, so a plain chain would repeat them).
#[must_use]
pub fn chain(err: &anyhow::Error) -> String {
    let mut out = String::new();
    for cause in err.chain() {
        let msg = cause.to_string();
        if out.contains(&msg) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&msg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_chain_skips_repeated_causes() {
        let root = anyhow::anyhow!("no such file");
        let err = root
            .context("read kubeconfig: no such file")
            .context("get Kubernetes config");
        assert_eq!(
            chain(&err),
            "get Kubernetes config: read kubeconfig: no such file"
        );
        let plain = anyhow::anyhow!("a").context("b");
        assert_eq!(chain(&plain), "b: a");
    }
}
