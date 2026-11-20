//! The check a client makes of the host's TLS certificate: the configured trust, a tolerance for a
//! clock that differs from the host's, and the reason a certificate was refused.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use coqui_peer::PeerId;
use rustls::CertificateError;
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

/// The reason the last certificate was refused, shared by every configuration built from one trust.
#[derive(Debug, Clone, Default)]
pub(crate) struct Refusals(Arc<Mutex<Option<String>>>);

impl Refusals {
    pub(crate) fn last(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, reason: String) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    }
}

/// Two sets of refusals are never what tells two trusts apart.
impl PartialEq for Refusals {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Refusals {}

/// Verifies as `inner` does, except that a certificate whose validity starts or ends within `skew`
/// of now is judged at the time that far off, and a refusal is described for the caller.
#[derive(Debug)]
pub(crate) struct Checked {
    pub(crate) inner: Arc<dyn ServerCertVerifier>,
    pub(crate) skew: Duration,
    pub(crate) refusals: Refusals,
}

impl ServerCertVerifier for Checked {
    fn verify_server_cert(
        &self,
        certificate: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let check = |at: UnixTime| {
            self.inner
                .verify_server_cert(certificate, intermediates, name, ocsp_response, at)
        };
        let error = match check(now) {
            Ok(verified) => return Ok(verified),
            Err(error) => error,
        };
        let seconds = now.as_secs();
        let moved = match &error {
            rustls::Error::InvalidCertificate(
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. },
            ) => Some(UnixTime::since_unix_epoch(Duration::from_secs(
                seconds.saturating_add(self.skew.as_secs()),
            ))),
            rustls::Error::InvalidCertificate(
                CertificateError::Expired | CertificateError::ExpiredContext { .. },
            ) => Some(UnixTime::since_unix_epoch(Duration::from_secs(
                seconds.saturating_sub(self.skew.as_secs()),
            ))),
            _ => None,
        };
        if let Some(at) = moved.filter(|_| !self.skew.is_zero())
            && let Ok(verified) = check(at)
        {
            return Ok(verified);
        }
        self.refusals
            .record(describe(&error, certificate.as_ref(), seconds));
        Err(error)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner
            .verify_tls12_signature(message, certificate, signed)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner
            .verify_tls13_signature(message, certificate, signed)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
}

/// Why `error` refused the certificate `der`, naming the certificate by its hash so a pin taken from
/// another one shows.
fn describe(error: &rustls::Error, der: &[u8], now: u64) -> String {
    let hash = PeerId::of(der);
    let validity = x509_parser::parse_x509_certificate(der)
        .ok()
        .map(|(_, certificate)| {
            let validity = certificate.validity();
            (
                u64::try_from(validity.not_before.timestamp()).unwrap_or(0),
                u64::try_from(validity.not_after.timestamp()).unwrap_or(0),
            )
        });
    match (error, validity) {
        (
            rustls::Error::InvalidCertificate(
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. },
            ),
            Some((from, _)),
        ) => format!(
            "the host's certificate {hash} is not valid until {} ({} s after this machine's clock): \
             a certificate made just now meets a clock that is behind the host's; set the clock, wait, \
             or allow for the difference with --clock-skew SPAN",
            rfc3339(from),
            from.saturating_sub(now)
        ),
        (
            rustls::Error::InvalidCertificate(
                CertificateError::Expired | CertificateError::ExpiredContext { .. },
            ),
            Some((_, until)),
        ) => format!(
            "the host's certificate {hash} expired at {} ({} s before this machine's clock): renew \
             it, or, if this machine's clock is ahead, allow for the difference with --clock-skew SPAN",
            rfc3339(until),
            now.saturating_sub(until)
        ),
        _ => format!("the host presented the certificate {hash}, which is not trusted ({error})"),
    }
}

/// `seconds` since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let rest = seconds % 86_400;
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn a_time_is_written_as_utc_text() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_165_054), "2026-11-20T08:57:34Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
