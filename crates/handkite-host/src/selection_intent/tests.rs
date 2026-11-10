use super::*;

#[cfg(not(target_os = "macos"))]
#[test]
fn pending_reobservation_fails_closed_on_unsupported_platforms() {
    let mut value = PendingInspection {
        _unsupported: std::marker::PhantomData,
    };
    assert_eq!(
        value.reobserve(),
        Err("pending selection inspection requires macOS")
    );
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn commit_fails_closed_on_unsupported_platforms() {
    let prepared = PreparedSelection {
        _unsupported: std::marker::PhantomData,
    };
    let failure = commit(prepared).await.err().unwrap();
    assert_eq!(failure.reason(), "selection commit requires macOS");
    assert_eq!(failure.phase(), CommitPhase::BeforeCommit);
    assert!(failure.residue_candidates().is_empty());
    let mut committed = CommittedSelection {
        _unsupported: std::marker::PhantomData,
    };
    assert_eq!(
        committed.revalidate(),
        Err("selection commit requires macOS")
    );
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn retirement_fails_closed_on_unsupported_platforms() {
    let committed = CommittedSelection {
        _unsupported: std::marker::PhantomData,
    };
    let failure = retire(committed).await.err().unwrap();
    assert_eq!(
        failure.reason(),
        "selection journal retirement requires macOS"
    );
    assert_eq!(failure.phase(), RetirementPhase::Before);
    assert!(failure.residue_candidates().is_empty());
    let mut retired = RetiredSelection {
        _unsupported: std::marker::PhantomData,
    };
    assert_eq!(
        retired.revalidate(),
        Err("selection journal retirement requires macOS")
    );
}

fn wire(operation: WireOperation) -> WireIntent {
    WireIntent {
        version: 1,
        transaction: "12".repeat(16),
        previous: if matches!(operation, WireOperation::InitialInstall) {
            None
        } else {
            Some(WireLink {
                manifest: "ab".repeat(32),
                device: 1,
                inode: 2,
            })
        },
        operation,
        destination: WireLink {
            manifest: "cd".repeat(32),
            device: 1,
            inode: 3,
        },
    }
}

fn bytes(value: &WireIntent) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

#[test]
fn exact_v1_encoding_roundtrips_each_operation() {
    for operation in [
        WireOperation::InitialInstall,
        WireOperation::Upgrade,
        WireOperation::Rollback,
    ] {
        let bytes = bytes(&wire(operation));
        assert!(bytes.len() < MAX_INTENT_BYTES);
        let intent = Intent::parse(&bytes).unwrap();
        assert_eq!(intent.canonical_bytes().unwrap(), bytes);
        assert_eq!(intent.transaction, [0x12; 16]);
        assert_eq!(intent.destination.manifest, [0xcd; 32]);
    }
    assert_eq!(
        String::from_utf8(bytes(&wire(WireOperation::InitialInstall))).unwrap(),
        format!(
            "{{\"version\":1,\"transaction\":\"{}\",\"operation\":\"initial-install\",\"previous\":null,\"destination\":{{\"manifest\":\"{}\",\"device\":1,\"inode\":3}}}}",
            "12".repeat(16),
            "cd".repeat(32)
        )
    );
}

#[test]
fn rejects_unknown_duplicate_noncanonical_truncated_and_oversized_records() {
    let canonical = String::from_utf8(bytes(&wire(WireOperation::Upgrade))).unwrap();
    for invalid in [
        String::new(),
        canonical[..canonical.len() - 1].into(),
        format!("{canonical}\n"),
        canonical.replace("\"version\":1", "\"version\":2"),
        canonical.replace("\"version\":1", "\"version\":1,\"version\":1"),
        canonical.replace("\"version\":1", "\"version\":1,\"extra\":null"),
        canonical.replace("\"device\":1", "\"device\":1,\"extra\":null"),
        canonical.replace("\"device\":1", "\"device\":1.0"),
        canonical.replace("\"device\":1", "\"device\":2147483648"),
        canonical.replace("\"inode\":3", "\"inode\":-1"),
        canonical.replace("\"inode\":3", "\"inode\":18446744073709551616"),
        canonical.replace("upgrade", "resume"),
        canonical.replace("\"version\"", "\"ver\\u0073ion\""),
        " ".repeat(MAX_INTENT_BYTES + 1),
    ] {
        assert_eq!(Intent::parse(invalid.as_bytes()), Err(INVALID));
    }
    assert_eq!(Intent::parse(&[0xff; 256]), Err(INVALID));
}

#[test]
fn rejects_inconsistent_operations_targets_and_link_identities() {
    for mutation in 0..10 {
        let mut value = wire(WireOperation::Upgrade);
        match mutation {
            0 => value.previous = None,
            1 => value.operation = WireOperation::InitialInstall,
            2 => value.destination.manifest = "ab".repeat(32),
            3 => value.destination.inode = 2,
            4 => value.destination.device = 2,
            5 => value.destination.inode = 0,
            6 => value.previous.as_mut().unwrap().inode = 0,
            7 => value.destination.manifest = "CD".repeat(32),
            8 => value.destination.manifest = format!("./{}", "cd".repeat(32)),
            _ => value.transaction = "00".repeat(16),
        }
        assert_eq!(Intent::parse(&bytes(&value)), Err(INVALID));
    }
}

#[test]
fn transaction_ids_have_one_exact_nonzero_128_bit_spelling() {
    for invalid in [
        "A".repeat(32),
        "f".repeat(31),
        "f".repeat(33),
        " ".repeat(32),
        "\0".repeat(32),
    ] {
        let mut value = wire(WireOperation::InitialInstall);
        value.transaction = invalid;
        assert_eq!(Intent::parse(&bytes(&value)), Err(INVALID));
    }
    let mut value = wire(WireOperation::InitialInstall);
    value.transaction = "ff".repeat(16);
    assert_eq!(
        Intent::parse(&bytes(&value)).unwrap().transaction,
        [255; 16]
    );
}

#[test]
fn arbitrary_single_byte_mutations_never_normalize_an_accepted_record() {
    let bytes = bytes(&wire(WireOperation::Upgrade));
    for index in 0..bytes.len() {
        for changed in [0, b' ', b'/', b'0', b'1', b'A', b'f', b'\n', 0xff] {
            let mut mutated = bytes.clone();
            mutated[index] = changed;
            if let Ok(parsed) = Intent::parse(&mutated) {
                assert_eq!(parsed.canonical_bytes().unwrap(), mutated);
            }
        }
    }
}
