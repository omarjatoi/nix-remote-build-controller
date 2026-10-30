//! Helpers for classifying Kubernetes API errors.

/// Returns the API status carried by `err`, if it is an API error.
fn status(err: &kube::Error) -> Option<&kube::core::Status> {
    match err {
        kube::Error::Api(status) => Some(status),
        _ => None,
    }
}

/// The object does not exist (HTTP 404).
pub fn is_not_found(err: &kube::Error) -> bool {
    status(err).is_some_and(|s| s.is_not_found())
}

/// The write raced with another writer: the resourceVersion was stale (HTTP 409
/// with reason Conflict).
pub fn is_conflict(err: &kube::Error) -> bool {
    status(err).is_some_and(|s| s.is_conflict())
}

/// A create failed because the object already exists.
pub fn is_already_exists(err: &kube::Error) -> bool {
    status(err).is_some_and(|s| s.is_already_exists())
}

/// The API server rejected the object as invalid (HTTP 422). Sending the same
/// object again cannot succeed.
pub fn is_invalid(err: &kube::Error) -> bool {
    status(err).is_some_and(|s| s.is_invalid())
}

/// Builds an API error, as the API server would return it. Used by the
/// in-memory fake API in tests.
#[cfg(any(test, feature = "testing"))]
pub fn new(code: u16, reason: &str, message: &str) -> kube::Error {
    kube::Error::Api(
        kube::core::Status::failure(message, reason)
            .with_code(code)
            .boxed(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_api_errors() {
        let not_found = new(404, "NotFound", "gone");
        let conflict = new(409, "Conflict", "stale");
        let exists = new(409, "AlreadyExists", "exists");
        let invalid = new(422, "Invalid", "bad name");
        assert!(is_not_found(&not_found) && !is_conflict(&not_found));
        assert!(is_conflict(&conflict) && !is_already_exists(&conflict));
        assert!(is_already_exists(&exists) && !is_conflict(&exists));
        assert!(is_invalid(&invalid) && !is_invalid(&conflict));
    }
}
