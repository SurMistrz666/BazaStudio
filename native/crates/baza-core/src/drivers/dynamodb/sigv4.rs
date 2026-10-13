//! AWS Signature Version 4 for one request with its body in hand: the
//! canonical request, the string to sign, the signing key and the
//! `Authorization` header, as the AWS documentation defines them.
//!
//! Only what a DynamoDB call needs is here: a `POST` with a JSON body, no
//! query string, a handful of headers. The functions are written for any
//! method and path all the same, so that the published test vectors of AWS
//! can be run against them (see the tests).
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The keys a request is signed with. The session token is not part of
/// the signature: it travels as a header, which is signed like the others.
pub(super) struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.secret_key.zeroize();
        if let Some(token) = self.session_token.as_mut() {
            token.zeroize();
        }
    }
}

/// What is signed.
pub(super) struct Request<'a> {
    pub method: &'a str,
    /// The absolute path of the URL, already as it is sent: `/`.
    pub path: &'a str,
    /// The query string in canonical form (sorted, encoded); empty for
    /// DynamoDB.
    pub query: &'a str,
    /// Every header that is signed, by any letter case of its name.
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    // HMAC takes a key of any length.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// A path as the canonical request wants it: every segment URI-encoded
/// twice, as for every service but S3. Unreserved characters stay as they
/// are, so a plain path is unchanged.
fn canonical_path(path: &str) -> String {
    let encode = |text: &str| {
        text.bytes().fold(String::new(), |mut out, byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                out.push(byte as char);
            } else {
                let _ = write!(out, "%{byte:02X}");
            }
            out
        })
    };
    let encoded = path
        .split('/')
        .map(|segment| encode(&encode(segment)))
        .collect::<Vec<_>>()
        .join("/");
    if encoded.is_empty() {
        "/".into()
    } else {
        encoded
    }
}

/// The headers in canonical form and the list of their names: names in
/// lower case and in order, values trimmed with runs of spaces made one.
fn canonical_headers(headers: &[(&str, &str)]) -> (String, String) {
    let mut sorted: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.trim().to_ascii_lowercase(),
                value.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    sorted.sort();
    let canonical = sorted.iter().fold(String::new(), |mut out, (name, value)| {
        let _ = writeln!(out, "{name}:{value}");
        out
    });
    let signed = sorted
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (canonical, signed)
}

/// Step 1: the canonical request, and the names of the signed headers.
pub(super) fn canonical_request(request: &Request<'_>) -> (String, String) {
    let (headers, signed) = canonical_headers(request.headers);
    (
        format!(
            "{}\n{}\n{}\n{headers}\n{signed}\n{}",
            request.method,
            canonical_path(request.path),
            request.query,
            sha256(request.body)
        ),
        signed,
    )
}

/// The credential scope of a day, a region and a service.
fn scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/aws4_request")
}

/// Step 2: the string to sign. `timestamp` is `YYYYMMDD'T'HHMMSS'Z'`.
pub(super) fn string_to_sign(
    timestamp: &str,
    region: &str,
    service: &str,
    canonical: &str,
) -> String {
    format!(
        "{ALGORITHM}\n{timestamp}\n{}\n{}",
        scope(&timestamp[..8.min(timestamp.len())], region, service),
        sha256(canonical.as_bytes())
    )
}

/// Step 3: the signing key of a day, a region and a service.
pub(super) fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let day = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let region = hmac(&day, region.as_bytes());
    let service = hmac(&region, service.as_bytes());
    hmac(&service, b"aws4_request")
}

/// The value of the `Authorization` header for a request. The headers have
/// to hold everything that is sent and signed: `host`, `x-amz-date` with
/// this same timestamp, and the session token when there is one.
pub(super) fn authorization(
    request: &Request<'_>,
    credentials: &Credentials,
    timestamp: &str,
    region: &str,
    service: &str,
) -> String {
    let (canonical, signed) = canonical_request(request);
    let to_sign = string_to_sign(timestamp, region, service, &canonical);
    let date = &timestamp[..8.min(timestamp.len())];
    let key = signing_key(&credentials.secret_key, date, region, service);
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    format!(
        "{ALGORITHM} Credential={}/{}, SignedHeaders={signed}, Signature={signature}",
        credentials.access_key,
        scope(date, region, service)
    )
}

/// The timestamp of a moment as the `x-amz-date` header carries it.
pub(super) fn timestamp(moment: chrono::DateTime<chrono::Utc>) -> String {
    moment.format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The credentials, the moment and the scope of the AWS Signature
    // Version 4 test suite and of the examples in the AWS documentation.
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const STAMP: &str = "20150830T123600Z";

    fn example() -> Credentials {
        Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: SECRET.into(),
            session_token: None,
        }
    }

    fn suite<'a>(
        method: &'a str,
        headers: &'a [(&'a str, &'a str)],
        body: &'a [u8],
    ) -> Request<'a> {
        Request {
            method,
            path: "/",
            query: "",
            headers,
            body,
        }
    }

    /// The signing key of the documentation ("Calculate the signature").
    #[test]
    fn the_signing_key_is_derived_as_documented() {
        assert_eq!(
            hex(&signing_key(SECRET, "20150830", "us-east-1", "iam")),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    /// The complete example of the documentation: `GET` of IAM `ListUsers`.
    #[test]
    fn the_documented_iam_example_is_signed_step_by_step() {
        let headers = [
            ("Host", "iam.amazonaws.com"),
            (
                "Content-Type",
                "application/x-www-form-urlencoded; charset=utf-8",
            ),
            ("X-Amz-Date", STAMP),
        ];
        let request = Request {
            method: "GET",
            path: "/",
            query: "Action=ListUsers&Version=2010-05-08",
            headers: &headers,
            body: b"",
        };
        let (canonical, signed) = canonical_request(&request);
        assert_eq!(
            canonical,
            "GET\n/\nAction=ListUsers&Version=2010-05-08\ncontent-type:application/x-www-form-urlencoded; charset=utf-8\nhost:iam.amazonaws.com\nx-amz-date:20150830T123600Z\n\ncontent-type;host;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(signed, "content-type;host;x-amz-date");
        assert_eq!(
            sha256(canonical.as_bytes()),
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        assert_eq!(
            string_to_sign(STAMP, "us-east-1", "iam", &canonical),
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/iam/aws4_request\nf536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        assert_eq!(
            authorization(&request, &example(), STAMP, "us-east-1", "iam"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    /// `get-vanilla` of the test suite, with its intermediate values.
    #[test]
    fn the_suite_vector_get_vanilla_matches() {
        let headers = [("Host", "example.amazonaws.com"), ("X-Amz-Date", STAMP)];
        let request = suite("GET", &headers, b"");
        let (canonical, _) = canonical_request(&request);
        assert_eq!(
            canonical,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            string_to_sign(STAMP, "us-east-1", "service", &canonical),
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(
            authorization(&request, &example(), STAMP, "us-east-1", "service"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// `post-vanilla` and `post-x-www-form-urlencoded` (a body that is
    /// hashed into the request).
    #[test]
    fn the_suite_vectors_for_post_match() {
        let headers = [("Host", "example.amazonaws.com"), ("X-Amz-Date", STAMP)];
        assert!(
            authorization(
                &suite("POST", &headers, b""),
                &example(),
                STAMP,
                "us-east-1",
                "service"
            )
            .ends_with(
                "Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
            )
        );
        let headers = [
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", STAMP),
        ];
        let request = suite("POST", &headers, b"Param1=value1");
        assert!(
            canonical_request(&request)
                .0
                .ends_with("9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e")
        );
        assert_eq!(
            authorization(&request, &example(), STAMP, "us-east-1", "service"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        );
    }

    /// `post-sts-header-before`: a session token is one more signed header.
    #[test]
    fn the_suite_vector_with_a_session_token_matches() {
        let token = "AQoDYXdzEPT//////////wEXAMPLEtc764bNrC9SAPBSM22wDOk4x4HIZ8j4FZTwdQWLWsKWHGBuFqwAeMicRXmxfpSPfIeoIYRqTflfKD8YUuwthAx7mSEI/qkPpKPi/kMcGdQrmGdeehM4IC1NtBmUpp2wUE8phUZampKsburEDy0KPkyQDYwT7WZ0wq5VSXDvp75YU9HFvlRd8Tx6q6fE8YQcHNVXAkiY9q6d+xo0rKwT38xVqr7ZD0u0iPPkUL64lIZbqBAz+scqKmlzm8FDrypNC9Yjc8fPOLn9FX9KSYvKTr4rvx3iSIlTJabIQwj2ICCR/oLxBA==";
        let headers = [
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", STAMP),
            ("X-Amz-Security-Token", token),
        ];
        assert_eq!(
            authorization(
                &suite("POST", &headers, b""),
                &example(),
                STAMP,
                "us-east-1",
                "service"
            ),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=85d96828115b5dc0cfc3bd16ad9e210dd772bbebba041836c64533a82be05ead"
        );
    }

    #[test]
    fn headers_and_paths_are_put_in_canonical_form() {
        // Names in lower case and in order, values trimmed, runs of spaces
        // made one (`get-header-value-multiline` and `-trim` of the suite).
        let (canonical, signed) = canonical_headers(&[
            ("X-Amz-Date", STAMP),
            ("My-Header1", "  a   b   c  "),
            ("Host", "example.amazonaws.com"),
        ]);
        assert_eq!(
            canonical,
            "host:example.amazonaws.com\nmy-header1:a b c\nx-amz-date:20150830T123600Z\n"
        );
        assert_eq!(signed, "host;my-header1;x-amz-date");
        assert_eq!(canonical_path("/"), "/");
        assert_eq!(canonical_path(""), "/");
        assert_eq!(canonical_path("/dynamo/v1"), "/dynamo/v1");
        // Anything else is encoded twice, as every service but S3 wants.
        assert_eq!(canonical_path("/a b"), "/a%2520b");
        assert_eq!(
            timestamp(chrono::DateTime::from_timestamp(1_440_938_160, 0).unwrap()),
            STAMP
        );
    }

    /// What a DynamoDB call looks like: the headers this driver sends.
    #[test]
    fn a_dynamodb_call_signs_its_target_and_its_body() {
        let body = br#"{"TableName":"Music"}"#;
        let headers = [
            ("content-type", "application/x-amz-json-1.0"),
            ("host", "dynamodb.eu-central-1.amazonaws.com"),
            ("x-amz-date", STAMP),
            ("x-amz-target", "DynamoDB_20120810.DescribeTable"),
        ];
        let header = authorization(
            &suite("POST", &headers, body),
            &example(),
            STAMP,
            "eu-central-1",
            "dynamodb",
        );
        assert!(header.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/eu-central-1/dynamodb/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-target, Signature="
        ));
        let signature = header.rsplit('=').next().unwrap();
        assert!(signature.len() == 64 && signature.bytes().all(|b| b.is_ascii_hexdigit()));
        // Another body, another signature.
        let other = authorization(
            &suite("POST", &headers, br#"{"TableName":"Other"}"#),
            &example(),
            STAMP,
            "eu-central-1",
            "dynamodb",
        );
        assert_ne!(header, other);
    }
}
