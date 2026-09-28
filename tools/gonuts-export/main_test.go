package main

import (
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	bolt "go.etcd.io/bbolt"
)

const (
	testMintURL = "https://mint.example"
	keysetA     = "keyset-aaaa"
	keysetB     = "keyset-bbbb"
)

// makeBoltDB builds a gonuts-shaped bbolt wallet with two funded keysets,
// so a successful export emits two token lines.
func makeBoltDB(t *testing.T, path string) {
	t.Helper()
	db, err := bolt.Open(path, 0600, nil)
	if err != nil {
		t.Fatalf("opening bbolt: %v", err)
	}
	defer db.Close()

	ks := WalletKeyset{
		Id:         keysetA,
		MintURL:    testMintURL,
		Unit:       "sat",
		Active:     true,
		PublicKeys: map[uint64][]byte{},
	}
	ksB := ks
	ksB.Id = keysetB

	ksJSON, _ := json.Marshal(ks)
	ksJSONB, _ := json.Marshal(ksB)

	proofA := Proof{Amount: 4, Id: keysetA, Secret: "s1", C: "c1"}
	proofB := Proof{Amount: 8, Id: keysetB, Secret: "s2", C: "c2"}
	proofAJSON, _ := json.Marshal(proofA)
	proofBJSON, _ := json.Marshal(proofB)

	err = db.Update(func(tx *bolt.Tx) error {
		keysetsb, err := tx.CreateBucketIfNotExists([]byte(KEYSETS_BUCKET))
		if err != nil {
			return err
		}
		mintb, err := keysetsb.CreateBucketIfNotExists([]byte(testMintURL))
		if err != nil {
			return err
		}
		if err := mintb.Put([]byte(keysetA), ksJSON); err != nil {
			return err
		}
		if err := mintb.Put([]byte(keysetB), ksJSONB); err != nil {
			return err
		}
		proofsb, err := tx.CreateBucketIfNotExists([]byte(PROOFS_BUCKET))
		if err != nil {
			return err
		}
		if err := proofsb.Put([]byte("p1"), proofAJSON); err != nil {
			return err
		}
		return proofsb.Put([]byte("p2"), proofBJSON)
	})
	if err != nil {
		t.Fatalf("building bbolt: %v", err)
	}
}

func lineCount(t *testing.T, path string) int {
	t.Helper()
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("reading %s: %v", path, err)
	}
	return len(strings.Split(strings.TrimSpace(string(body)), "\n"))
}

func TestExportPublishesAllArtifactsWithoutTempRemains(t *testing.T) {
	dir := t.TempDir()
	dbPath := filepath.Join(dir, "wallet.db")
	outDir := filepath.Join(dir, "out")
	makeBoltDB(t, dbPath)

	if err := run(dbPath, outDir); err != nil {
		t.Fatalf("run: %v", err)
	}

	tokens := filepath.Join(outDir, "tokens.jsonl")
	if got := lineCount(t, tokens); got != 2 {
		t.Fatalf("tokens.jsonl: want 2 lines, got %d", got)
	}
	for _, name := range []string{"keyset_counters.json", "migration-report.json"} {
		if _, err := os.Stat(filepath.Join(outDir, name)); err != nil {
			t.Fatalf("missing artifact %s: %v", name, err)
		}
	}
	entries, _ := os.ReadDir(outDir)
	for _, e := range entries {
		if strings.HasSuffix(e.Name(), ".tmp") {
			t.Fatalf("temp artifact leaked into output: %s", e.Name())
		}
	}
}

// TestCrashMidExportLeavesPreviousArtifactIntact is the PR #21 Codex P1
// reproducer: the pre-fix exporter truncated tokens.jsonl in place before
// finishing, so a crash mid-export left a partial file that the Rust
// importer then swallowed (and could finalize the migration with tokens
// silently unmigrated). The fix writes to tokens.jsonl.tmp and renames
// only on success, so the previous complete artifact survives a crash.
func TestCrashMidExportLeavesPreviousArtifactIntact(t *testing.T) {
	if os.Getenv("GONUTS_EXPORT_CRASH_SUBPROC") == "1" {
		dbPath := os.Getenv("GONUTS_EXPORT_CRASH_DB")
		outDir := os.Getenv("GONUTS_EXPORT_CRASH_OUT")
		if err := runWithCrashAfter(dbPath, outDir, 1); err != nil {
			t.Fatalf("crash run returned error: %v", err)
		}
		return
	}

	dir := t.TempDir()
	dbPath := filepath.Join(dir, "wallet.db")
	outDir := filepath.Join(dir, "out")
	makeBoltDB(t, dbPath)

	if err := run(dbPath, outDir); err != nil {
		t.Fatalf("initial run: %v", err)
	}
	tokens := filepath.Join(outDir, "tokens.jsonl")
	before, err := os.ReadFile(tokens)
	if err != nil {
		t.Fatalf("reading initial artifact: %v", err)
	}

	cmd := exec.Command(os.Args[0], "-test.run=TestCrashMidExportLeavesPreviousArtifactIntact")
	cmd.Env = append(os.Environ(),
		"GONUTS_EXPORT_CRASH_SUBPROC=1",
		"GONUTS_EXPORT_CRASH_DB="+dbPath,
		"GONUTS_EXPORT_CRASH_OUT="+outDir,
	)
	exitErr := cmd.Run()
	ee, ok := exitErr.(*exec.ExitError)
	if !ok || ee.ExitCode() != 9 {
		t.Fatalf("subprocess should die with exit 9 mid-export, got %v", exitErr)
	}

	after, err := os.ReadFile(tokens)
	if err != nil {
		t.Fatalf("reading artifact after crash: %v", err)
	}
	if string(before) != string(after) {
		t.Fatalf("tokens.jsonl changed across a crashed export:\nbefore:\n%s\nafter:\n%s", before, after)
	}
	if got := lineCount(t, tokens); got != 2 {
		t.Fatalf("previous complete artifact must stay at 2 lines, got %d", got)
	}
}
