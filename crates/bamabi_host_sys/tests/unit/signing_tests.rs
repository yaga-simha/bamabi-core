use super::*;
use bevy_ecs::world::World;
use core::mem::discriminant;
use core::sync::atomic::{AtomicU64, Ordering};
use ssh_key::{Algorithm, Certificate, HashAlg, PrivateKey, certificate, rand_core::OsRng};
use std::path::{Path, PathBuf};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(tag: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bam_signing_unit_{tag}_{}_{}",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn gen_ed25519_key() -> PrivateKey {
    PrivateKey::random(&mut OsRng, Algorithm::Ed25519).expect("generate Ed25519 key")
}

fn issue_user_cert(
    ca: &PrivateKey,
    subject: &PrivateKey,
    serial: u64,
    key_id: &str,
    principals: &[&str],
) -> Certificate {
    let mut builder =
        certificate::Builder::new_with_random_nonce(&mut OsRng, subject.public_key(), 100, 10_000)
            .expect("cert builder");
    builder
        .cert_type(certificate::CertType::User)
        .expect("user cert");
    builder.serial(serial).expect("serial");
    builder.key_id(key_id).expect("key_id");
    for &principal in principals {
        builder.valid_principal(principal).expect("principal");
    }
    builder.sign(ca).expect("sign cert")
}

fn build_synthetic_elf64_with_bam_sig(sig_slot_len: usize) -> (Vec<u8>, usize) {
    let code_off = 64usize;
    let code_len = 32usize;
    let strtab = b"\0.text\0.shstrtab\0.bam_sig\0";
    let strtab_off = code_off + code_len;
    let sig_off = strtab_off + strtab.len();
    let shoff = sig_off + sig_slot_len;
    let shnum = 4usize;
    let mut buf = vec![0u8; shoff + shnum * 64];

    buf[0..4].copy_from_slice(b"\x7fELF");
    buf[4] = 2;
    buf[5] = 1;
    buf[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
    buf[58..60].copy_from_slice(&64u16.to_le_bytes());
    buf[60..62].copy_from_slice(&(shnum as u16).to_le_bytes());
    buf[62..64].copy_from_slice(&2u16.to_le_bytes());

    buf[code_off..code_off + code_len].fill(0x90);
    buf[strtab_off..strtab_off + strtab.len()].copy_from_slice(strtab);

    let write_shdr =
        |b: &mut [u8], idx: usize, name_off: u32, sh_type: u32, off: usize, sz: usize| {
            let s = shoff + idx * 64;
            b[s..s + 4].copy_from_slice(&name_off.to_le_bytes());
            b[s + 4..s + 8].copy_from_slice(&sh_type.to_le_bytes());
            b[s + 24..s + 32].copy_from_slice(&(off as u64).to_le_bytes());
            b[s + 32..s + 40].copy_from_slice(&(sz as u64).to_le_bytes());
        };
    write_shdr(&mut buf, 1, 1, 1, code_off, code_len);
    write_shdr(&mut buf, 2, 7, 3, strtab_off, strtab.len());
    write_shdr(&mut buf, 3, 17, 1, sig_off, sig_slot_len);
    (buf, sig_off)
}

fn assert_same_verification_error(
    wrapped: &SignatureVerificationError,
    inner: &SignatureVerificationError,
    context: &str,
) {
    assert_eq!(
        discriminant(wrapped),
        discriminant(inner),
        "{context}: error variant mismatch: wrapped={wrapped:?}, inner={inner:?}"
    );
    match (wrapped, inner) {
        (
            SignatureVerificationError::NamespaceMismatch {
                expected: w_exp,
                actual: w_act,
            },
            SignatureVerificationError::NamespaceMismatch {
                expected: i_exp,
                actual: i_act,
            },
        ) => {
            assert_eq!((w_exp, w_act), (i_exp, i_act), "{context}");
        }
        (
            SignatureVerificationError::PrincipalMismatch {
                expected: w_exp,
                actual: w_act,
            },
            SignatureVerificationError::PrincipalMismatch {
                expected: i_exp,
                actual: i_act,
            },
        ) => {
            assert_eq!(w_exp, i_exp, "{context}");
            assert_eq!(w_act, i_act, "{context}");
        }
        (
            SignatureVerificationError::CertificateSerialRevoked { serial: w_ser },
            SignatureVerificationError::CertificateSerialRevoked { serial: i_ser },
        ) => assert_eq!(w_ser, i_ser, "{context}"),
        (
            SignatureVerificationError::SignerRevoked { fingerprint: w_fp },
            SignatureVerificationError::SignerRevoked { fingerprint: i_fp },
        )
        | (
            SignatureVerificationError::UntrustedPublicKey { fingerprint: w_fp },
            SignatureVerificationError::UntrustedPublicKey { fingerprint: i_fp },
        )
        | (
            SignatureVerificationError::UntrustedCertificateAuthority {
                ca_fingerprint: w_fp,
            },
            SignatureVerificationError::UntrustedCertificateAuthority {
                ca_fingerprint: i_fp,
            },
        ) => assert_eq!(w_fp, i_fp, "{context}"),
        _ => {}
    }
}

#[test]
fn host_sys_trust_ecs_adapters_store_and_verify() {
    let key = gen_ed25519_key();
    let pub_str = key.public_key().to_openssh().unwrap();
    let policy = ModTrustPolicy::from_public_key_openssh(&pub_str).unwrap();
    let payload = b"host-sys-adapter-payload";
    let sig = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
    let receipt = policy.verify_bytes(payload, sig.as_bytes()).unwrap();
    policy.verify_receipt_still_trusted(&receipt).unwrap();

    let mut world = World::new();
    world.insert_resource(policy);
    let entity = world.spawn(receipt.clone()).id();
    assert_eq!(world.get::<VerifiedModSignature>(entity), Some(&receipt));
    assert!(world.resource::<ModTrustPolicy>().has_trust_anchors());
}

#[test]
fn delegate_add_krl_signer_openssh_grants_only_krl_authority() {
    let ca_key = gen_ed25519_key();
    let krl_signer = gen_ed25519_key();
    let krl_pub = krl_signer.public_key().to_openssh().unwrap();

    assert!(matches!(
        ModTrustPolicy::strict().add_krl_signer_openssh("not-an-openssh-key"),
        Err(ModTrustConfigError::InvalidPublicKey { .. })
    ));

    let base = ModTrustPolicy::from_ca_openssh(&ca_key.public_key().to_openssh().unwrap())
        .unwrap()
        .add_krl_signer_openssh(&krl_pub)
        .unwrap()
        .add_krl_signer_openssh(&krl_pub)
        .unwrap();

    assert_eq!(base.trusted_krl_signer_count(), 1);
    assert_eq!(base.trusted_public_key_count(), 0);

    let krl_bytes = KrlBuilder::new()
        .version(7)
        .revoke_cert_serial(Some(ca_key.public_key()), 99)
        .to_bytes()
        .unwrap();
    let krl_sig =
        sign_mod_bytes_with_key(&krl_signer, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_bytes).unwrap();
    let policy = base
        .add_signed_krl_bytes(&krl_bytes, krl_sig.as_bytes())
        .unwrap();

    assert_eq!(policy.applied_krl_version(), Some(7));
    let ca_fp = ca_key.public_key().fingerprint(HashAlg::Sha256);
    assert!(policy.is_cert_serial_revoked(&ca_fp, 99));

    let mod_payload = b"mod-bytes-signed-by-krl-key";
    let mod_sig =
        sign_mod_bytes_with_key(&krl_signer, DEFAULT_SIGNATURE_NAMESPACE, mod_payload).unwrap();
    assert!(matches!(
        policy.verify_bytes(mod_payload, mod_sig.as_bytes()),
        Err(SignatureVerificationError::UntrustedPublicKey { .. })
    ));
}

#[test]
fn delegate_add_krl_signer_file_reads_key_and_rejects_missing() {
    let tmp = TempDirGuard::new("krl_signer_file");
    let ca_key = gen_ed25519_key();
    let krl_signer = gen_ed25519_key();
    let key_path = tmp.path().join("krl_signer.pub");
    let pub_text = krl_signer.public_key().to_openssh().unwrap();
    std::fs::write(&key_path, format!("{pub_text}\n")).unwrap();

    let missing_path = tmp.path().join("does_not_exist.pub");
    assert!(matches!(
        ModTrustPolicy::strict().add_krl_signer_file(&missing_path),
        Err(ModTrustConfigError::Io { .. })
    ));

    let policy = ModTrustPolicy::from_ca_openssh(&ca_key.public_key().to_openssh().unwrap())
        .unwrap()
        .add_krl_signer_file(&key_path)
        .unwrap();
    assert_eq!(policy.trusted_krl_signer_count(), 1);

    let krl_bytes = KrlBuilder::new()
        .version(3)
        .revoke_cert_key_id(None, "compromised-id")
        .to_bytes()
        .unwrap();
    let krl_sig =
        sign_mod_bytes_with_key(&krl_signer, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_bytes).unwrap();
    let updated = policy
        .add_signed_krl_bytes(&krl_bytes, krl_sig.as_bytes())
        .unwrap();
    let ca_fp = ca_key.public_key().fingerprint(HashAlg::Sha256);
    assert!(updated.is_cert_key_id_revoked(&ca_fp, "compromised-id"));
}

#[test]
fn delegate_with_krl_signer_principal_overrides_and_rejects_empty() {
    let ca_key = gen_ed25519_key();
    let default_cert_key = gen_ed25519_key();
    let custom_cert_key = gen_ed25519_key();

    assert_eq!(
        ModTrustPolicy::strict().krl_signer_principal(),
        DEFAULT_KRL_SIGNER_PRINCIPAL
    );
    assert!(matches!(
        ModTrustPolicy::strict().with_krl_signer_principal(""),
        Err(ModTrustConfigError::InvalidRevocationList { .. })
    ));

    let policy = ModTrustPolicy::from_ca_openssh(&ca_key.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500))
        .with_krl_signer_principal("custom-krl-admin")
        .unwrap();
    assert_eq!(policy.krl_signer_principal(), "custom-krl-admin");

    let krl_bytes = KrlBuilder::new()
        .version(11)
        .revoke_cert_serial(None, 404)
        .to_bytes()
        .unwrap();

    let default_cert = issue_user_cert(
        &ca_key,
        &default_cert_key,
        1,
        "default-krl",
        &[DEFAULT_KRL_SIGNER_PRINCIPAL],
    );
    let wrong_sig = sign_mod_bytes_with_certificate(
        &default_cert_key,
        &default_cert,
        DEFAULT_KRL_SIGNATURE_NAMESPACE,
        &krl_bytes,
    )
    .unwrap();
    assert!(matches!(
        policy
            .clone()
            .add_signed_krl_bytes(&krl_bytes, wrong_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));

    let custom_cert = issue_user_cert(
        &ca_key,
        &custom_cert_key,
        2,
        "custom-krl",
        &["custom-krl-admin"],
    );
    let ok_sig = sign_mod_bytes_with_certificate(
        &custom_cert_key,
        &custom_cert,
        DEFAULT_KRL_SIGNATURE_NAMESPACE,
        &krl_bytes,
    )
    .unwrap();
    let merged = policy
        .add_signed_krl_bytes(&krl_bytes, ok_sig.as_bytes())
        .unwrap();
    assert_eq!(merged.applied_krl_version(), Some(11));
}

#[test]
fn delegate_verify_bampack_bytes_verifies_and_rejects_tamper() {
    let key = gen_ed25519_key();
    let pack_bytes = BamPackBuilder::new()
        .add_entry("mod.toml", b"[mod]\nname = \"pack_mod\"\n".to_vec())
        .unwrap()
        .add_entry("bin/mod.bin", vec![1, 2, 3, 4])
        .unwrap()
        .build_signed_with_key(&key, DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();

    let policy = ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
        .unwrap()
        .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();

    let verified = ModTrustPolicy::verify_bampack_bytes(&policy, &pack_bytes).unwrap();
    assert_eq!(verified.entries.len(), 2);
    assert_eq!(verified.entries[0].path, "mod.toml");
    assert_eq!(verified.entries[1].path, "bin/mod.bin");

    let mut tampered = pack_bytes;
    tampered[26] ^= 0x55;
    assert!(policy.verify_bampack_bytes(&tampered).is_err());
}

#[test]
fn delegate_verify_bampack_file_verifies_from_disk_and_rejects_missing() {
    let tmp = TempDirGuard::new("bampack_file");
    let ca = gen_ed25519_key();
    let signer = gen_ed25519_key();
    let cert = issue_user_cert(&ca, &signer, 12, "pack-file-cert", &["pack_mod"]);

    let pack_bytes = BamPackBuilder::new()
        .add_entry("manifest.toml", b"name = \"pack_mod\"\n".to_vec())
        .unwrap()
        .build_signed_with_certificate(&signer, &cert, DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();
    let pack_path = tmp.path().join("bundle.bampack");
    std::fs::write(&pack_path, &pack_bytes).unwrap();

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500))
        .with_required_principal("pack_mod");

    let verified = ModTrustPolicy::verify_bampack_file(&policy, &pack_path).unwrap();
    assert_eq!(verified.entries.len(), 1);
    assert_eq!(verified.signature.serial, Some(12));

    let extract_dir = tmp.path().join("extracted");
    let extracted_paths = verified
        .extract_to_dir_with_limits(
            &extract_dir,
            BamPackExtractLimits {
                max_entries: 4,
                max_total_bytes: 4096,
            },
        )
        .unwrap();
    assert_eq!(extracted_paths, vec![extract_dir.join("manifest.toml")]);

    assert!(matches!(
        policy.verify_bampack_file(tmp.path().join("missing.bampack")),
        Err(SignatureVerificationError::SourceBinaryRead { .. })
    ));
}

#[test]
fn verified_mod_signature_from_round_trips_owned_and_borrowed() {
    let cert_receipt = bamabi_trust::VerifiedModSignature {
        signer_fingerprint: "SHA256:signerfp".to_owned(),
        ca_fingerprint: Some("SHA256:cafp".to_owned()),
        key_id: Some("release-42".to_owned()),
        principals: vec!["sample_mod".to_owned(), "host".to_owned()],
        serial: Some(42),
        namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
        binary_sha256: [0xAB; 32],
    };

    let from_borrowed_trust = VerifiedModSignature::from(&cert_receipt);
    let from_owned_trust = VerifiedModSignature::from(cert_receipt.clone());
    assert_eq!(from_borrowed_trust, from_owned_trust);

    let back_from_borrowed = bamabi_trust::VerifiedModSignature::from(&from_borrowed_trust);
    let back_from_owned = bamabi_trust::VerifiedModSignature::from(from_owned_trust);
    assert_eq!(back_from_borrowed, cert_receipt);
    assert_eq!(back_from_owned, cert_receipt);

    let raw_key_receipt = VerifiedModSignature {
        signer_fingerprint: "SHA256:rawkeyfp".to_owned(),
        ca_fingerprint: None,
        key_id: None,
        principals: Vec::new(),
        serial: None,
        namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
        binary_sha256: [0xCD; 32],
    };
    let trust_from_borrowed = bamabi_trust::VerifiedModSignature::from(&raw_key_receipt);
    let trust_from_owned = bamabi_trust::VerifiedModSignature::from(raw_key_receipt.clone());
    assert_eq!(trust_from_borrowed, trust_from_owned);
    assert_eq!(
        VerifiedModSignature::from(&trust_from_borrowed),
        raw_key_receipt
    );
    assert_eq!(
        VerifiedModSignature::from(trust_from_owned),
        raw_key_receipt
    );
}

#[test]
fn wrapper_matches_inner_bamabi_trust_across_all_scenarios() {
    let ca_key = gen_ed25519_key();
    let valid_leaf = gen_ed25519_key();
    let revoked_serial_leaf = gen_ed25519_key();
    let revoked_key_leaf = gen_ed25519_key();
    let pinned_key = gen_ed25519_key();
    let krl_signer = gen_ed25519_key();

    let ca_pub = ca_key.public_key().to_openssh().unwrap();
    let pinned_pub = pinned_key.public_key().to_openssh().unwrap();
    let krl_signer_pub = krl_signer.public_key().to_openssh().unwrap();
    let revoked_leaf_pub = revoked_key_leaf.public_key().to_openssh().unwrap();

    // Build a signed KRL verified via a KRL signer added with `add_krl_signer_openssh`.
    let krl_bytes = KrlBuilder::new()
        .version(5)
        .revoke_cert_serial(Some(ca_key.public_key()), 100)
        .revoke_key_openssh(&revoked_leaf_pub)
        .unwrap()
        .to_bytes()
        .unwrap();
    let krl_sig =
        sign_mod_bytes_with_key(&krl_signer, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_bytes).unwrap();

    let wrapped = ModTrustPolicy::from_ca_openssh(&ca_pub)
        .unwrap()
        .add_public_key_openssh(&pinned_pub)
        .unwrap()
        .add_krl_signer_openssh(&krl_signer_pub)
        .unwrap()
        .with_krl_signer_principal("custom-krl")
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500))
        .with_manifest_name_principal_binding(true)
        .add_signed_krl_bytes(&krl_bytes, krl_sig.as_bytes())
        .unwrap();

    let inner = bamabi_trust::ModTrustPolicy::from_ca_openssh(&ca_pub)
        .unwrap()
        .add_public_key_openssh(&pinned_pub)
        .unwrap()
        .add_krl_signer_openssh(&krl_signer_pub)
        .unwrap()
        .with_krl_signer_principal("custom-krl")
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500))
        .with_manifest_name_principal_binding(true)
        .add_signed_krl_bytes(&krl_bytes, krl_sig.as_bytes())
        .unwrap();

    assert_eq!(wrapped.applied_krl_version(), inner.applied_krl_version());

    let mod_bytes = b"equivalence-test-mod-binary";

    // 1. Key-signed input (`verify_bytes`).
    let key_sig =
        sign_mod_bytes_with_key(&pinned_key, DEFAULT_SIGNATURE_NAMESPACE, mod_bytes).unwrap();
    let w_key = wrapped.verify_bytes(mod_bytes, key_sig.as_bytes()).unwrap();
    let i_key = inner.verify_bytes(mod_bytes, key_sig.as_bytes()).unwrap();
    assert_eq!(w_key, VerifiedModSignature::from(&i_key));
    assert_eq!(bamabi_trust::VerifiedModSignature::from(&w_key), i_key);
    assert_eq!(
        wrapped.verify_receipt_still_trusted(&w_key).is_ok(),
        inner.verify_receipt_still_trusted(&i_key).is_ok()
    );

    // 2. Cert-signed input (`verify_bytes`, `verify_binary_or_sidecar`, `verify_embedded_bam_sig`,
    //    `verify_receipt_still_trusted`, and `verify_manifest_principal`).
    let valid_cert = issue_user_cert(&ca_key, &valid_leaf, 42, "valid-42", &["sample_mod"]);
    let cert_sig = sign_mod_bytes_with_certificate(
        &valid_leaf,
        &valid_cert,
        DEFAULT_SIGNATURE_NAMESPACE,
        mod_bytes,
    )
    .unwrap();
    let w_cert = wrapped
        .verify_bytes(mod_bytes, cert_sig.as_bytes())
        .unwrap();
    let i_cert = inner.verify_bytes(mod_bytes, cert_sig.as_bytes()).unwrap();
    assert_eq!(w_cert, VerifiedModSignature::from(i_cert.clone()));
    assert_eq!(
        wrapped
            .verify_binary_or_sidecar(mod_bytes, Some(cert_sig.as_bytes()))
            .unwrap(),
        VerifiedModSignature::from(
            &inner
                .verify_binary_or_sidecar(mod_bytes, Some(cert_sig.as_bytes()))
                .unwrap()
        )
    );
    wrapped.verify_receipt_still_trusted(&w_cert).unwrap();
    inner.verify_receipt_still_trusted(&i_cert).unwrap();
    wrapped
        .verify_manifest_principal(&w_cert, "sample_mod")
        .unwrap();
    inner
        .verify_manifest_principal(&i_cert, "sample_mod")
        .unwrap();
    let w_pm_err = wrapped
        .verify_manifest_principal(&w_cert, "other_mod")
        .unwrap_err();
    let i_pm_err = inner
        .verify_manifest_principal(&i_cert, "other_mod")
        .unwrap_err();
    assert_same_verification_error(&w_pm_err, &i_pm_err, "manifest principal mismatch");

    // Embedded `.bam_sig` ELF64 parity (`verify_embedded_bam_sig` & `verify_binary_or_sidecar(None)`).
    let (mut elf_img, sig_off) = build_synthetic_elf64_with_bam_sig(1536);
    let emb_sig = sign_mod_bytes_with_certificate(
        &valid_leaf,
        &valid_cert,
        DEFAULT_SIGNATURE_NAMESPACE,
        &elf_img,
    )
    .unwrap();
    elf_img[sig_off..sig_off + emb_sig.len()].copy_from_slice(emb_sig.as_bytes());
    let w_emb = wrapped.verify_embedded_bam_sig(&elf_img).unwrap();
    let i_emb = inner.verify_embedded_bam_sig(&elf_img).unwrap();
    assert_eq!(w_emb, VerifiedModSignature::from(&i_emb));
    assert_eq!(
        wrapped.verify_binary_or_sidecar(&elf_img, None).unwrap(),
        VerifiedModSignature::from(&inner.verify_binary_or_sidecar(&elf_img, None).unwrap())
    );

    // 3. Revoked serial (serial 100 from signed KRL).
    let revoked_serial_cert = issue_user_cert(
        &ca_key,
        &revoked_serial_leaf,
        100,
        "revoked-serial",
        &["sample_mod"],
    );
    let revoked_serial_sig = sign_mod_bytes_with_certificate(
        &revoked_serial_leaf,
        &revoked_serial_cert,
        DEFAULT_SIGNATURE_NAMESPACE,
        mod_bytes,
    )
    .unwrap();
    let w_rev_ser = wrapped
        .verify_bytes(mod_bytes, revoked_serial_sig.as_bytes())
        .unwrap_err();
    let i_rev_ser = inner
        .verify_bytes(mod_bytes, revoked_serial_sig.as_bytes())
        .unwrap_err();
    assert!(matches!(
        w_rev_ser,
        SignatureVerificationError::CertificateSerialRevoked { serial: 100 }
    ));
    assert_same_verification_error(&w_rev_ser, &i_rev_ser, "revoked serial");

    // 4. Revoked key (leaf key revoked in signed KRL).
    let revoked_key_cert = issue_user_cert(
        &ca_key,
        &revoked_key_leaf,
        101,
        "revoked-key",
        &["sample_mod"],
    );
    let revoked_key_sig = sign_mod_bytes_with_certificate(
        &revoked_key_leaf,
        &revoked_key_cert,
        DEFAULT_SIGNATURE_NAMESPACE,
        mod_bytes,
    )
    .unwrap();
    let w_rev_key = wrapped
        .verify_bytes(mod_bytes, revoked_key_sig.as_bytes())
        .unwrap_err();
    let i_rev_key = inner
        .verify_bytes(mod_bytes, revoked_key_sig.as_bytes())
        .unwrap_err();
    assert!(matches!(
        w_rev_key,
        SignatureVerificationError::SignerRevoked { .. }
    ));
    assert_same_verification_error(&w_rev_key, &i_rev_key, "revoked key");

    // 5. Wrong namespace.
    let wrong_ns_sig =
        sign_mod_bytes_with_key(&pinned_key, "wrong.namespace.v1", mod_bytes).unwrap();
    let w_ns = wrapped
        .verify_bytes(mod_bytes, wrong_ns_sig.as_bytes())
        .unwrap_err();
    let i_ns = inner
        .verify_bytes(mod_bytes, wrong_ns_sig.as_bytes())
        .unwrap_err();
    assert!(matches!(
        w_ns,
        SignatureVerificationError::NamespaceMismatch { .. }
    ));
    assert_same_verification_error(&w_ns, &i_ns, "wrong namespace");

    // 6. BamPack verification parity under the same signed-KRL-configured policy.
    let pack_bytes = BamPackBuilder::new()
        .add_entry("mod.toml", b"name = \"sample_mod\"\n".to_vec())
        .unwrap()
        .build_signed_with_certificate(&valid_leaf, &valid_cert, DEFAULT_SIGNATURE_NAMESPACE)
        .unwrap();
    let w_pack = wrapped.verify_bampack_bytes(&pack_bytes).unwrap();
    let i_pack = inner.verify_bampack_bytes(&pack_bytes).unwrap();
    assert_eq!(w_pack, i_pack);
}

#[test]
fn newly_reexported_trust_items_and_deref_getters_accessible() {
    let ca_key = gen_ed25519_key();
    let ca_pub: &PublicKey = ca_key.public_key();
    let ca_fp: Fingerprint = ca_pub.fingerprint(HashAlg::Sha256);
    assert_eq!(ca_pub.algorithm(), Algorithm::Ed25519);
    let _le = LineEnding::LF;

    let policy = ModTrustPolicy::from_ca_openssh(&ca_pub.to_openssh().unwrap())
        .unwrap()
        .with_required_principal("sample_mod");

    // Exercise all 16 Deref-reachable getters documented on ModTrustPolicy.
    assert_eq!(policy.krl_signer_principal(), DEFAULT_KRL_SIGNER_PRINCIPAL);
    assert_eq!(policy.trusted_ca_count(), 1);
    assert_eq!(policy.trusted_public_key_count(), 0);
    assert_eq!(policy.trusted_krl_signer_count(), 0);
    assert_eq!(policy.min_krl_version(), 0);
    assert_eq!(policy.applied_krl_version(), None);
    assert!(!policy.is_cert_serial_revoked(&ca_fp, 1));
    assert!(!policy.is_cert_key_id_revoked(&ca_fp, "kid"));
    assert_eq!(policy.enforcement(), SignatureEnforcement::Required);
    assert!(policy.is_enforced());
    assert_eq!(policy.namespace(), DEFAULT_SIGNATURE_NAMESPACE);
    assert_eq!(policy.required_principal(), Some("sample_mod"));
    assert_eq!(
        policy.validity_policy(),
        CertificateValidityPolicy::SystemClock
    );
    assert_eq!(policy.max_binary_bytes(), DEFAULT_MAX_MOD_BINARY_BYTES);
    assert!(!policy.binds_manifest_name_to_principal());
    assert!(policy.has_trust_anchors());

    // Exercise newly re-exported bampack / embedded_sig / KrlBuilder items.
    assert_eq!(
        DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES,
        DEFAULT_MAX_MOD_BINARY_BYTES
    );
    assert_eq!(MAX_BAMPACK_PATH_DEPTH, 32);
    assert_eq!(MAX_BAMPACK_PATH_LEN, 1024);
    assert!(validate_bampack_entry_path("bin/libmod.so").is_ok());
    assert!(validate_bampack_entry_path("../escape").is_err());

    let krl_bytes = KrlBuilder::new()
        .revoke_cert_serial(Some(ca_pub), 77)
        .to_bytes()
        .unwrap();
    let policy = policy.add_krl_bytes(&krl_bytes).unwrap();
    assert!(policy.is_cert_serial_revoked(&ca_fp, 77));

    let pack_bytes = bampack::BamPackBuilder::new()
        .add_entry("a.txt", b"hello".to_vec())
        .unwrap()
        .build_signed_with_key(&ca_key, DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();
    let (parsed, has_sig) =
        parse_bampack_entries_unverified(&pack_bytes, DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES)
            .unwrap();
    assert!(has_sig);
    assert_eq!(parsed.len(), 1);
    assert_eq!(embedded_sig::BAM_SIG_SECTION_NAME, ".bam_sig");
    assert_eq!(ssh_key::Algorithm::Ed25519, Algorithm::Ed25519);
    let _extract_err_variant: fn(usize, usize) -> BamPackExtractError =
        |count, limit| BamPackExtractError::TooManyEntries { count, limit };
}

#[cfg(all(feature = "signature-verification", feature = "keygen"))]
#[test]
fn keygen_generates_writes_and_issues_verifiable_openssh_cert() {
    let tmp = TempDirGuard::new("keygen_parity");
    let ca = generate_ed25519_keypair(Some("ca-root")).expect("generate CA keypair");
    assert_eq!(ca.comment(), "ca-root");
    let signer = keygen::generate_ed25519_keypair(None).expect("generate signer keypair");
    assert!(matches!(
        generate_ed25519_keypair(Some("bad\ncomment")),
        Err(ModSigningError::Encoding { .. })
    ));

    let out_prefix = tmp.path().join("keys").join("mod_signer");
    let written: GeneratedKeypair =
        write_openssh_keypair(&out_prefix, Some("disk-signer")).expect("write keypair");
    assert_eq!(written.private_key, out_prefix);
    assert_eq!(written.public_key, tmp.path().join("keys/mod_signer.pub"));
    assert!(written.private_key.is_file());
    assert!(written.public_key.is_file());

    let loaded_priv = PrivateKey::read_openssh_file(&written.private_key).expect("read priv key");
    let loaded_pub = PublicKey::read_openssh_file(&written.public_key).expect("read pub key");
    assert_eq!(loaded_priv.public_key(), &loaded_pub);
    assert_eq!(
        loaded_pub.fingerprint(HashAlg::Sha256).to_string(),
        written.fingerprint
    );

    let overwrite_err =
        write_openssh_keypair(&out_prefix, None).expect_err("refuse to overwrite existing keypair");
    assert!(matches!(
        overwrite_err,
        ModTrustConfigError::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists
    ));

    let issued: IssuedCertificate = issue_ed25519_certificate_openssh(
        &ca,
        signer.public_key(),
        707,
        "m4-keygen-cert",
        &["sample_mod".to_owned()],
        0,
        u64::MAX,
    )
    .expect("issue OpenSSH user certificate");
    assert!(
        issued
            .certificate_openssh
            .starts_with("ssh-ed25519-cert-v01@openssh.com ")
    );
    assert_eq!(
        issued.subject_fingerprint,
        signer.public_key().fingerprint(HashAlg::Sha256).to_string()
    );
    assert_eq!(
        issued.ca_fingerprint,
        ca.public_key().fingerprint(HashAlg::Sha256).to_string()
    );

    let payload = b"keygen-signed-mod-payload";
    let pem = sign_mod_bytes_with_openssh_certificate(
        &signer,
        &issued.certificate_openssh,
        DEFAULT_SIGNATURE_NAMESPACE,
        payload,
    )
    .expect("sign payload with issued cert");

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(1_700_000_000))
        .with_required_principal("sample_mod");
    let verified = policy
        .verify_bytes(payload, pem.as_bytes())
        .expect("verify payload signed with keygen-issued cert");
    assert_eq!(verified.serial, Some(707));
    assert_eq!(verified.key_id.as_deref(), Some("m4-keygen-cert"));
    assert_eq!(verified.principals, vec!["sample_mod"]);
    assert_eq!(
        verified.ca_fingerprint.as_deref(),
        Some(issued.ca_fingerprint.as_str())
    );
    assert_eq!(verified.signer_fingerprint, issued.subject_fingerprint);
}
