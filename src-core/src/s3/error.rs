use crate::error::CoreError;

/// S4Drive S3 error classification.
/// Maps raw S3 error codes to structured S4Drive errors.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedError {
    pub code: String,
    pub message: String,
    pub http_status: u16,
    pub is_retryable: bool,
}

/// Classify an S3 error code into a structured error.
pub fn classify_error_code(code: &str, http_status: u16) -> ClassifiedError {
    let (message, is_retryable) = match code {
        "PreconditionFailed" => ("Conditional write failed (ETag mismatch)".into(), false),
        "NoSuchKey" => ("Object not found in bucket".into(), false),
        "NoSuchBucket" => ("Bucket does not exist".into(), false),
        "AccessDenied" => (
            "Access denied: check credentials and bucket permissions".into(),
            false,
        ),
        "InvalidAccessKeyId" => ("Invalid access key".into(), false),
        "SignatureDoesNotMatch" => ("Secret key does not match".into(), false),
        "BucketAlreadyExists" => ("Bucket name already taken".into(), false),
        "BucketAlreadyOwnedByYou" => ("Bucket already owned by you".into(), false),
        "EntityTooLarge" => ("Object size exceeds maximum allowed".into(), false),
        "InvalidPart" => ("Invalid multipart upload part".into(), true),
        "InvalidPartOrder" => ("Multipart parts uploaded out of order".into(), true),
        "NoSuchUpload" => ("Multipart upload ID not found".into(), true),
        "OperationAborted" => ("Conflicting conditional S3 operation".into(), true),
        "Conflict" => ("S3 operation conflict".into(), false),
        "InternalError" => ("Internal server error".into(), true),
        "ServiceUnavailable" => ("Service temporarily unavailable".into(), true),
        "SlowDown" => ("Slow down: reduce request rate".into(), true),
        "RequestTimeout" => ("Request timed out".into(), true),
        "RequestTimeTooSkewed" => ("Clock skew: check system time".into(), false),
        "InvalidRequest" => ("Invalid request".into(), false),
        "MalformedXML" => ("Malformed request".into(), false),
        _ => {
            if http_status >= 500 {
                (
                    format!("Server error (HTTP {}): {}", http_status, code),
                    true,
                )
            } else if http_status == 429 {
                ("Rate limited".into(), true)
            } else {
                (format!("S3 error: {} (HTTP {})", code, http_status), false)
            }
        }
    };

    ClassifiedError {
        code: code.to_string(),
        message,
        http_status,
        is_retryable,
    }
}

/// Check if a CoreError is retryable.
pub fn is_retryable(err: &CoreError) -> bool {
    match err {
        CoreError::S3(msg) => {
            let msg = msg.to_ascii_lowercase();
            msg.contains("timeout")
                || msg.contains("internalerror")
                || msg.contains("internal server error")
                || msg.contains("serviceunavailable")
                || msg.contains("service unavailable")
                || msg.contains("slowdown")
                || msg.contains("requesttimeout")
                || msg.contains("operationaborted")
                || msg.contains("5xx")
                || msg.contains("500")
                || msg.contains("502")
                || msg.contains("503")
                || msg.contains("504")
                || msg.contains("429")
        }
        CoreError::Network(_) => true,
        _ => false,
    }
}

/// Human-readable explanation of a condition write failure (412).
pub fn explain_412(context: &str) -> String {
    format!(
        "Another device changed '{}' before this device could commit. \
         S4Drive will re-read the current state and attempt to merge.",
        context
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_known_errors() {
        let e = classify_error_code("PreconditionFailed", 412);
        assert!(!e.is_retryable);
        assert_eq!(e.http_status, 412);

        let e = classify_error_code("NoSuchKey", 404);
        assert!(!e.is_retryable);
    }

    #[test]
    fn test_5xx_is_retryable() {
        let e = classify_error_code("InternalError", 500);
        assert!(e.is_retryable);
    }

    #[test]
    fn test_4xx_not_retryable() {
        let e = classify_error_code("InvalidAccessKeyId", 403);
        assert!(!e.is_retryable);
    }

    #[test]
    fn test_is_retryable() {
        assert!(is_retryable(&CoreError::S3("timeout: connection".into())));
        assert!(is_retryable(&CoreError::S3(
            "HTTP 503 ServiceUnavailable".into()
        )));
        assert!(is_retryable(&CoreError::S3("409 OperationAborted".into())));
        assert!(is_retryable(&CoreError::Network("connection reset".into())));
        assert!(!is_retryable(&CoreError::Auth("access denied".into())));
    }
}
