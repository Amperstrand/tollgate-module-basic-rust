//! Regression tests for the wallet seed policy (issue #30): public API
//! only, so the §9 red-proof can run them against pre-fix `src/`.

use std::path::PathBuf;

use tollgate_module_basic_rust::wallet::TollWallet;

#[tokio::test]
async fn corrupt_seed_file_fails_instead_of_regenerating() {
    // Old behavior: wrong-size seed was silently regenerated — a fresh
    // seed over an existing wallet orphans every deterministic derivation
    // (AGENTS.md: never silently recreate wallet state).
    let dir = tempfile::tempdir().unwrap();
    let seed_path: PathBuf = dir.path().join("wallet_seed.bin");
    std::fs::write(&seed_path, b"too short").unwrap();

    let err = TollWallet::load_or_create_seed(&seed_path)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("wrong size"), "{err}");
}

#[tokio::test]
async fn seed_mnemonic_mismatch_fails_loudly() {
    // Old behavior: the mnemonic file did not exist and was not consulted;
    // a replaced seed file silently forked the wallet identity.
    let dir = tempfile::tempdir().unwrap();
    let seed_path = dir.path().join("wallet_seed.bin");
    let mnemonic_path = dir.path().join("wallet_mnemonic.txt");

    std::fs::write(&seed_path, [9u8; 64]).unwrap();
    let other = bip39::Mnemonic::generate(24).unwrap();
    std::fs::write(&mnemonic_path, other.to_string()).unwrap();

    let err = TollWallet::load_or_create_seed(&seed_path)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("mismatch"), "{err}");
}

#[tokio::test]
async fn first_boot_writes_mnemonic_that_rederives_the_seed() {
    let dir = tempfile::tempdir().unwrap();
    let seed_path = dir.path().join("wallet_seed.bin");
    let mnemonic_path = dir.path().join("wallet_mnemonic.txt");

    let seed = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
    let phrase = std::fs::read_to_string(&mnemonic_path).unwrap();
    assert_eq!(phrase.split_whitespace().count(), 24);

    let mnemonic: bip39::Mnemonic = phrase.trim().parse().unwrap();
    assert_eq!(mnemonic.to_seed(""), seed);
}
