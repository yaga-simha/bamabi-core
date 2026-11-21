//! Thin Bevy ECS adapters over [`bamabi_trust`], storing cryptographic mod receipts
//! ([`VerifiedModSignature`]) and trust policies ([`ModTrustPolicy`]) as Bevy `Component` / `Resource`.
//!
//! Optional OpenSSH Ed25519 keypair generation and certificate issuance (`keygen` module,
//! `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`,
//! `issue_ed25519_certificate_openssh`, `write_openssh_keypair`) are exposed when the
//! `keygen` feature is enabled (which implies `signature-verification` and enables
//! `bamabi_trust/keygen`).

use bevy_ecs::prelude::{Component, Resource};
use core::ops::Deref;
use std::path::Path;

pub use bamabi_trust::bampack::{
    BAMPACK_MAGIC, BAMPACK_VERSION, BamPackBuilder, BamPackEntry, BamPackExtractError,
    BamPackExtractLimits, DEFAULT_BAMPACK_NAMESPACE, DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES,
    MAX_BAMPACK_ENTRIES, MAX_BAMPACK_PATH_DEPTH, MAX_BAMPACK_PATH_LEN, VerifiedBamPack,
    parse_bampack_entries_unverified, validate_bampack_entry_path,
};
pub use bamabi_trust::embedded_sig::{
    BAM_SIG_MACHO_SECTION_NAME, BAM_SIG_SECTION_NAME, EmbeddedSignatureSection,
    extract_embedded_bam_sig, zeroed_binary_for_bam_sig,
};
#[cfg(feature = "keygen")]
pub use bamabi_trust::keygen::{
    self, GeneratedKeypair, IssuedCertificate, generate_ed25519_keypair,
    issue_ed25519_certificate_openssh, write_openssh_keypair,
};
pub use bamabi_trust::{
    Algorithm, Certificate, CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE,
    DEFAULT_KRL_SIGNER_PRINCIPAL, DEFAULT_MAX_MOD_BINARY_BYTES, DEFAULT_SIGNATURE_NAMESPACE,
    Fingerprint, HashAlg, KrlBuilder, LineEnding, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES,
    ModSigningError, ModTrustConfigError, PrivateKey, PublicKey, SignatureEnforcement,
    SignatureVerificationError, bampack, default_signature_path_for, embedded_sig,
    sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate, ssh_key,
};

/// Verified cryptographic identity attached as an ECS [`Component`] to each spawned
/// [`LoadedModInfo`](crate::LoadedModInfo) entity (and stored on [`LoadedMod`](crate::LoadedMod))
/// when signature verification succeeds.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct VerifiedModSignature {
    /// OpenSSH SHA-256 fingerprint (`"SHA256:..."`) of the leaf signing key.
    pub signer_fingerprint: String,
    /// OpenSSH SHA-256 fingerprint (`"SHA256:..."`) of the issuing Root CA, if certificate-backed.
    pub ca_fingerprint: Option<String>,
    /// Certificate `key_id` string assigned by the CA, if certificate-backed.
    pub key_id: Option<String>,
    /// Authorized principals embedded in the signing certificate, if certificate-backed.
    pub principals: Vec<String>,
    /// Certificate serial number assigned by the CA, if certificate-backed.
    pub serial: Option<u64>,
    /// Verified `SSHSIG` domain-separation namespace.
    pub namespace: String,
    /// SHA-256 digest of the exact verified shared-library bytes mapped into memory.
    pub binary_sha256: [u8; 32],
}

impl From<bamabi_trust::VerifiedModSignature> for VerifiedModSignature {
    #[inline]
    fn from(v: bamabi_trust::VerifiedModSignature) -> Self {
        Self {
            signer_fingerprint: v.signer_fingerprint,
            ca_fingerprint: v.ca_fingerprint,
            key_id: v.key_id,
            principals: v.principals,
            serial: v.serial,
            namespace: v.namespace,
            binary_sha256: v.binary_sha256,
        }
    }
}

impl From<&bamabi_trust::VerifiedModSignature> for VerifiedModSignature {
    #[inline]
    fn from(v: &bamabi_trust::VerifiedModSignature) -> Self {
        Self {
            signer_fingerprint: v.signer_fingerprint.clone(),
            ca_fingerprint: v.ca_fingerprint.clone(),
            key_id: v.key_id.clone(),
            principals: v.principals.clone(),
            serial: v.serial,
            namespace: v.namespace.clone(),
            binary_sha256: v.binary_sha256,
        }
    }
}

impl From<&VerifiedModSignature> for bamabi_trust::VerifiedModSignature {
    #[inline]
    fn from(v: &VerifiedModSignature) -> Self {
        Self {
            signer_fingerprint: v.signer_fingerprint.clone(),
            ca_fingerprint: v.ca_fingerprint.clone(),
            key_id: v.key_id.clone(),
            principals: v.principals.clone(),
            serial: v.serial,
            namespace: v.namespace.clone(),
            binary_sha256: v.binary_sha256,
        }
    }
}

impl From<VerifiedModSignature> for bamabi_trust::VerifiedModSignature {
    #[inline]
    fn from(v: VerifiedModSignature) -> Self {
        Self {
            signer_fingerprint: v.signer_fingerprint,
            ca_fingerprint: v.ca_fingerprint,
            key_id: v.key_id,
            principals: v.principals,
            serial: v.serial,
            namespace: v.namespace,
            binary_sha256: v.binary_sha256,
        }
    }
}

/// Cryptographic trust policy configuring Ed25519 signature verification before `dlopen`.
///
/// Stored as a Bevy [`Resource`] in `World` (defaults to [`ModTrustPolicy::disabled`] when
/// omitted unless explicit enforcement is configured). Backed by zero-Bevy
/// [`bamabi_trust::ModTrustPolicy`].
///
/// # Read-Only Policy Getters (via [`Deref`])
///
/// All read-only inspection methods on [`bamabi_trust::ModTrustPolicy`] are directly reachable
/// through [`Deref`] without separate wrapper methods:
/// - [`krl_signer_principal`](bamabi_trust::ModTrustPolicy::krl_signer_principal)
/// - [`trusted_ca_count`](bamabi_trust::ModTrustPolicy::trusted_ca_count)
/// - [`trusted_public_key_count`](bamabi_trust::ModTrustPolicy::trusted_public_key_count)
/// - [`trusted_krl_signer_count`](bamabi_trust::ModTrustPolicy::trusted_krl_signer_count)
/// - [`min_krl_version`](bamabi_trust::ModTrustPolicy::min_krl_version)
/// - [`applied_krl_version`](bamabi_trust::ModTrustPolicy::applied_krl_version)
/// - [`is_cert_serial_revoked`](bamabi_trust::ModTrustPolicy::is_cert_serial_revoked)
/// - [`is_cert_key_id_revoked`](bamabi_trust::ModTrustPolicy::is_cert_key_id_revoked)
/// - [`enforcement`](bamabi_trust::ModTrustPolicy::enforcement)
/// - [`is_enforced`](bamabi_trust::ModTrustPolicy::is_enforced)
/// - [`namespace`](bamabi_trust::ModTrustPolicy::namespace)
/// - [`required_principal`](bamabi_trust::ModTrustPolicy::required_principal)
/// - [`validity_policy`](bamabi_trust::ModTrustPolicy::validity_policy)
/// - [`max_binary_bytes`](bamabi_trust::ModTrustPolicy::max_binary_bytes)
/// - [`binds_manifest_name_to_principal`](bamabi_trust::ModTrustPolicy::binds_manifest_name_to_principal)
/// - [`has_trust_anchors`](bamabi_trust::ModTrustPolicy::has_trust_anchors)
#[derive(Resource, Clone, Debug)]
pub struct ModTrustPolicy(bamabi_trust::ModTrustPolicy);

impl Default for ModTrustPolicy {
    #[inline]
    fn default() -> Self {
        Self::disabled()
    }
}

impl Deref for ModTrustPolicy {
    type Target = bamabi_trust::ModTrustPolicy;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<bamabi_trust::ModTrustPolicy> for ModTrustPolicy {
    #[inline]
    fn from(inner: bamabi_trust::ModTrustPolicy) -> Self {
        Self(inner)
    }
}

impl From<ModTrustPolicy> for bamabi_trust::ModTrustPolicy {
    #[inline]
    fn from(policy: ModTrustPolicy) -> Self {
        policy.0
    }
}

macro_rules! delegate_policy_methods {
    (
        $(
            $(#[$meta:meta])*
            fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) -> Result<Self>;
        )*
    ) => {
        $(
            $(#[$meta])*
            pub fn $name(self, $($arg: $arg_ty),*) -> Result<Self, ModTrustConfigError> {
                self.0.$name($($arg),*).map(Self)
            }
        )*
    };
    (
        infallible
        $(
            $(#[$meta:meta])*
            fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) -> Self;
        )*
    ) => {
        $(
            $(#[$meta])*
            #[must_use]
            pub fn $name(self, $($arg: $arg_ty),*) -> Self {
                Self(self.0.$name($($arg),*))
            }
        )*
    };
}

impl ModTrustPolicy {
    /// Borrows the underlying pure-Rust [`bamabi_trust::ModTrustPolicy`].
    #[inline]
    #[must_use]
    pub const fn as_inner(&self) -> &bamabi_trust::ModTrustPolicy {
        &self.0
    }

    /// Unwraps into the underlying pure-Rust [`bamabi_trust::ModTrustPolicy`].
    #[inline]
    #[must_use]
    pub fn into_inner(self) -> bamabi_trust::ModTrustPolicy {
        self.0
    }

    /// Creates a policy with signature verification disabled ([`SignatureEnforcement::Disabled`]).
    #[must_use]
    pub fn disabled() -> Self {
        Self(bamabi_trust::ModTrustPolicy::disabled())
    }

    /// Creates an enforcing ([`SignatureEnforcement::Required`]) policy with no trust anchors.
    #[must_use]
    pub fn strict() -> Self {
        Self(bamabi_trust::ModTrustPolicy::strict())
    }

    /// Creates an enforcing policy trusting the given OpenSSH Ed25519 Certificate Authority public key.
    pub fn from_ca_openssh(openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        bamabi_trust::ModTrustPolicy::from_ca_openssh(openssh_pub).map(Self)
    }

    /// Creates an enforcing policy trusting the given pinned OpenSSH Ed25519 public key.
    pub fn from_public_key_openssh(openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        bamabi_trust::ModTrustPolicy::from_public_key_openssh(openssh_pub).map(Self)
    }

    delegate_policy_methods! {
        /// Adds a trusted OpenSSH Ed25519 CA public key (deduplicated) and marks enforcement required.
        fn add_ca_openssh(openssh_pub: &str) -> Result<Self>;
        /// Reads and adds a trusted OpenSSH Ed25519 CA `.pub` file from disk.
        fn add_ca_file(path: impl AsRef<Path>) -> Result<Self>;
        /// Adds a pinned OpenSSH Ed25519 public key (deduplicated) and marks enforcement required.
        fn add_public_key_openssh(openssh_pub: &str) -> Result<Self>;
        /// Reads and adds a pinned OpenSSH Ed25519 `.pub` file from disk.
        fn add_public_key_file(path: impl AsRef<Path>) -> Result<Self>;
        /// Adds an explicitly trusted Ed25519 public key authorized to sign Key Revocation Lists
        /// without granting mod-signing authority.
        fn add_krl_signer_openssh(openssh_pub: &str) -> Result<Self>;
        /// Reads and adds an explicitly trusted KRL signer `.pub` file from disk.
        fn add_krl_signer_file(path: impl AsRef<Path>) -> Result<Self>;
        /// Sets the certificate principal required on CA-delegated KRL signing certificates
        /// (defaults to [`DEFAULT_KRL_SIGNER_PRINCIPAL`], `"krl-signer"`). Rejects empty strings.
        fn with_krl_signer_principal(principal: impl Into<String>) -> Result<Self>;
        /// Sets the expected `SSHSIG` domain-separation namespace (rejects empty strings).
        fn with_namespace(namespace: impl Into<String>) -> Result<Self>;
        /// Revokes an inclusive certificate serial range `[min, max]` across all trusted CAs.
        fn revoke_serial_range(min: u64, max: u64) -> Result<Self>;
        /// Revokes a leaf or CA OpenSSH Ed25519 public key.
        fn revoke_key_openssh(openssh_pub: &str) -> Result<Self>;
        /// Revokes a SHA-256 OpenSSH key fingerprint (`"SHA256:..."`).
        fn revoke_fingerprint(fingerprint: &str) -> Result<Self>;
        /// Merges revocations from a binary OpenSSH Key Revocation List (`SSHKRL`).
        fn add_krl_bytes(krl_bytes: &[u8]) -> Result<Self>;
        /// Reads and merges a binary `SSHKRL` file from disk.
        fn add_krl_file(path: impl AsRef<Path>) -> Result<Self>;
        /// Verifies and merges a signed `SSHKRL` along with its OpenSSH `SSHSIG` envelope.
        fn add_signed_krl_bytes(krl_bytes: &[u8], sig_pem_bytes: &[u8]) -> Result<Self>;
        /// Reads, verifies, and merges a signed `SSHKRL` and its `.sig` sidecar from disk.
        fn add_signed_krl_file(krl_path: impl AsRef<Path>, sig_path: Option<&Path>) -> Result<Self>;
        /// Parses and merges an OpenSSH KRL text revocation specification (`serial:`, `key:`, `sha256:`, `id:`).
        fn add_revocation_spec_str(spec: &str) -> Result<Self>;
    }

    delegate_policy_methods! {
        infallible
        /// Sets [`SignatureEnforcement`] explicitly.
        fn with_enforcement(enforcement: SignatureEnforcement) -> Self;
        /// Requires certificate-backed signatures to authorize `principal` in `valid_principals`.
        fn with_required_principal(principal: impl Into<String>) -> Self;
        /// Requires `BamManifest::name` to appear in certificate `valid_principals`.
        fn with_manifest_name_principal_binding(enabled: bool) -> Self;
        /// Sets the certificate validity time-checking policy.
        fn with_validity_policy(policy: CertificateValidityPolicy) -> Self;
        /// Sets the maximum mod shared-library file size buffered into memory for verification.
        fn with_max_binary_bytes(max_bytes: u64) -> Self;
        /// Revokes a certificate serial number across all trusted CAs.
        fn revoke_serial(serial: u64) -> Self;
        /// Revokes a certificate `key_id` across all trusted CAs.
        fn revoke_key_id(key_id: impl Into<String>) -> Self;
        /// Enforces a minimum rollback version on signed KRLs applied via [`Self::add_signed_krl_bytes`].
        fn with_min_krl_version(min_version: u64) -> Self;
    }

    /// Verifies `binary_bytes` against detached `sig_pem_bytes`.
    pub fn verify_bytes(
        &self,
        binary_bytes: &[u8],
        sig_pem_bytes: &[u8],
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        self.0
            .verify_bytes(binary_bytes, sig_pem_bytes)
            .map(VerifiedModSignature::from)
    }

    /// Verifies `binary_bytes` using an embedded `.bam_sig` / `__bam_sig` section.
    pub fn verify_embedded_bam_sig(
        &self,
        binary_bytes: &[u8],
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        self.0
            .verify_embedded_bam_sig(binary_bytes)
            .map(VerifiedModSignature::from)
    }

    /// Verifies `binary_bytes` using a detached sidecar when `Some`, or embedded `.bam_sig` when `None`.
    pub fn verify_binary_or_sidecar(
        &self,
        binary_bytes: &[u8],
        sidecar_sig: Option<&[u8]>,
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        self.0
            .verify_binary_or_sidecar(binary_bytes, sidecar_sig)
            .map(VerifiedModSignature::from)
    }

    /// Re-validates an already-issued [`VerifiedModSignature`] receipt against active anchors/KRLs.
    pub fn verify_receipt_still_trusted(
        &self,
        receipt: &VerifiedModSignature,
    ) -> Result<(), SignatureVerificationError> {
        let trust_receipt = bamabi_trust::VerifiedModSignature::from(receipt);
        self.0.verify_receipt_still_trusted(&trust_receipt)
    }

    /// Checks [`VerifiedModSignature::principals`] against `manifest_name` when principal binding is enabled.
    pub fn verify_manifest_principal(
        &self,
        receipt: &VerifiedModSignature,
        manifest_name: &str,
    ) -> Result<(), SignatureVerificationError> {
        let trust_receipt = bamabi_trust::VerifiedModSignature::from(receipt);
        self.0
            .verify_manifest_principal(&trust_receipt, manifest_name)
    }

    /// Parses and cryptographically verifies a `.bampack` archive in memory against this policy.
    pub fn verify_bampack_bytes(
        &self,
        archive_bytes: &[u8],
    ) -> Result<VerifiedBamPack, SignatureVerificationError> {
        self.0.verify_bampack_bytes(archive_bytes)
    }

    /// Reads (bounded by [`bamabi_trust::ModTrustPolicy::max_binary_bytes`]) and cryptographically
    /// verifies a `.bampack` file on disk.
    pub fn verify_bampack_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<VerifiedBamPack, SignatureVerificationError> {
        self.0.verify_bampack_file(path)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/signing_tests.rs"]
mod tests;
