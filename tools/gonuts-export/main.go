package main

import (
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	bolt "go.etcd.io/bbolt"
)

// Bucket names — must match gonuts wallet/storage/bolt.go
const (
	KEYSETS_BUCKET = "keysets"
	PROOFS_BUCKET  = "proofs"
	SEED_BUCKET    = "seed"
)

// Proof mirrors the gonuts cashu.Proof JSON structure.
// We re-declare it here to avoid importing the gonuts package
// (which would pull in secp256k1, hdkeychain, etc.).
type Proof struct {
	Amount  uint64  `json:"amount"`
	Id      string  `json:"id"`
	Secret  string  `json:"secret"`
	C       string  `json:"C"`
	Witness string  `json:"witness,omitempty"`
	DLEQ    *DLEQ   `json:"dleq,omitempty"`
}

type DLEQ struct {
	E string `json:"e"`
	S string `json:"s"`
	R string `json:"r,omitempty"`
}

// WalletKeyset mirrors gonuts crypto.WalletKeyset JSON structure.
// The marshalled form uses []byte for public keys, not the struct form.
type WalletKeyset struct {
	Id          string             `json:"Id"`
	MintURL     string             `json:"MintURL"`
	Unit        string             `json:"Unit"`
	Active      bool               `json:"Active"`
	PublicKeys  map[uint64][]byte   `json:"PublicKeys"`
	Counter     uint32             `json:"Counter"`
	InputFeePpk uint               `json:"InputFeePpk"`
}

// KeysetsMap maps mint URL → list of keysets.
type KeysetsMap map[string][]WalletKeyset

// TokenV3 is the Cashu V3 token format (NUT-00).
type TokenV3 struct {
	Token []TokenV3Proof `json:"token"`
	Unit  string         `json:"unit"`
	Memo  string         `json:"memo,omitempty"`
}

type TokenV3Proof struct {
	Mint   string  `json:"mint"`
	Proofs []Proof `json:"proofs"`
}

// KeysetCounterEntry for keyset_counters.json
type KeysetCounterEntry struct {
	KeysetID string `json:"keyset_id"`
	MintURL  string `json:"mint_url"`
	Counter  uint32 `json:"counter"`
}

// KeysetHealth for migration-report.json
type KeysetHealth struct {
	KeysetID       string `json:"keyset_id"`
	MintURL        string `json:"mint_url"`
	Counter        uint32 `json:"counter"`
	ProofCount     int    `json:"proof_count"`
	TotalAmount    uint64 `json:"total_amount"`
	Status         string `json:"status"`
}

type MigrationReport struct {
	ExportedAt      string         `json:"exported_at"`
	Keysets         []KeysetHealth `json:"keysets"`
	TotalProofs     int            `json:"total_proofs"`
	TotalAmount     uint64         `json:"total_amount"`
	HasSeed         bool           `json:"has_seed"`
}

func main() {
	boltPath := flag.String("bolt", "", "path to gonuts wallet.db (required)")
	outDir := flag.String("out", ".", "output directory for tokens.jsonl, keyset_counters.json, migration-report.json")
	flag.Parse()

	// Positional form: `gonuts-export <wallet.db> <tokens.jsonl>` — the
	// invocation the Rust first-boot migration uses (main.rs) and the one
	// MIGRATION.md documents. The second argument is the tokens FILE path;
	// the other artifacts land next to it. Accepted alongside the flag
	// form so both callers keep working.
	if flag.NArg() >= 2 {
		boltPath = &flag.Args()[0]
		tokensOut := flag.Args()[1]
		dir := filepath.Dir(tokensOut)
		outDir = &dir
		tokensName := filepath.Base(tokensOut)
		positionalTokensName = &tokensName
	}

	if *boltPath == "" {
		fmt.Fprintln(os.Stderr, "error: --bolt is required")
		flag.Usage()
		os.Exit(1)
	}

	if err := run(*boltPath, *outDir); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

// When invoked positionally, the tokens artifact filename from the caller
// (defaults to tokens.jsonl under the flag form).
var positionalTokensName *string

func run(boltPath, outDir string) error {
	return runWithCrashAfter(boltPath, outDir, 0)
}

// runWithCrashAfter mirrors run() but terminates the process with exit
// status 9 once crashAfter token lines have been written to the temp
// artifact. It exists solely as a seam for the crash-window regression
// test: it simulates kill -9 / power loss mid-export so the test can
// assert that a previously exported tokens.jsonl is never left truncated.
func runWithCrashAfter(boltPath, outDir string, crashAfter int) error {
	// Open bbolt read-only
	db, err := bolt.Open(boltPath, 0600, &bolt.Options{ReadOnly: true})
	if err != nil {
		return fmt.Errorf("opening bbolt: %w", err)
	}
	defer db.Close()

	// Load keysets
	keysets, err := loadKeysets(db)
	if err != nil {
		return fmt.Errorf("loading keysets: %w", err)
	}

	// Load proofs
	proofsBykeyset, err := loadProofs(db)
	if err != nil {
		return fmt.Errorf("loading proofs: %w", err)
	}

	// Check for seed
	hasSeed := false
	db.View(func(tx *bolt.Tx) error {
		seedb := tx.Bucket([]byte(SEED_BUCKET))
		if seedb != nil {
			seed := seedb.Get([]byte(SEED_BUCKET))
			hasSeed = len(seed) > 0
		}
		return nil
	})

	// Build output
	if err := os.MkdirAll(outDir, 0755); err != nil {
		return fmt.Errorf("creating output dir: %w", err)
	}

	// All artifacts are built under temp names in the SAME directory (same
	// filesystem, so each rename is atomic). tokens.jsonl is renamed LAST:
	// its existence at the final path is the success stamp the Rust
	// importer gates on — a crash or nonzero exit can never expose a
	// truncated token file to the migration (Codex P1 on PR #21).
	tokensName := "tokens.jsonl"
	if positionalTokensName != nil {
		tokensName = *positionalTokensName
	}
	tokensPath := filepath.Join(outDir, tokensName)
	tokensTmp := tokensPath + ".tmp"
	tokensFile, err := os.Create(tokensTmp)
	if err != nil {
		return fmt.Errorf("creating tokens.jsonl.tmp: %w", err)
	}

	totalProofs := 0
	totalAmount := uint64(0)
	var healthEntries []KeysetHealth
	linesWritten := 0

	for mintURL, ksList := range keysets {
		for _, ks := range ksList {
			proofs := proofsBykeyset[ks.Id]
			if len(proofs) == 0 {
				healthEntries = append(healthEntries, KeysetHealth{
					KeysetID:   ks.Id,
					MintURL:    mintURL,
					Counter:    ks.Counter,
					ProofCount: 0,
					Status:     "empty",
				})
				continue
			}

			// Sort proofs by amount for deterministic output
			sort.Slice(proofs, func(i, j int) bool {
				return proofs[i].Amount < proofs[j].Amount
			})

			// Create V3 token
			token := TokenV3{
				Token: []TokenV3Proof{{
					Mint:   mintURL,
					Proofs: proofs,
				}},
				Unit: "sat",
			}

			tokenJSON, err := json.Marshal(token)
			if err != nil {
				tokensFile.Close()
				os.Remove(tokensTmp)
				return fmt.Errorf("marshalling token for keyset %s: %w", ks.Id, err)
			}

			// One canonical token string per line (cashuA + base64url of
			// the V3 JSON): the only encoding CDK/cashu `Token::from_str`
			// accepts, i.e. what the Rust importer and the `migrate` CLI
			// parse. Raw JSON lines were unparseable end-to-end.
			line := "cashuA" + base64.RawURLEncoding.EncodeToString(tokenJSON)
			if _, err := tokensFile.WriteString(line + "\n"); err != nil {
				tokensFile.Close()
				os.Remove(tokensTmp)
				return fmt.Errorf("writing token for keyset %s: %w", ks.Id, err)
			}
			linesWritten++
			if crashAfter > 0 && linesWritten >= crashAfter {
				// Simulated power loss (test seam only; see runWithCrashAfter).
				os.Exit(9)
			}

			// Calculate health
			ksAmount := uint64(0)
			for _, p := range proofs {
				ksAmount += p.Amount
			}

			healthEntries = append(healthEntries, KeysetHealth{
				KeysetID:    ks.Id,
				MintURL:     mintURL,
				Counter:     ks.Counter,
				ProofCount:  len(proofs),
				TotalAmount: ksAmount,
				Status:      "healthy",
			})

			totalProofs += len(proofs)
			totalAmount += ksAmount
		}
	}

	if err := tokensFile.Sync(); err != nil {
		tokensFile.Close()
		os.Remove(tokensTmp)
		return fmt.Errorf("fsyncing tokens.jsonl.tmp: %w", err)
	}
	if err := tokensFile.Close(); err != nil {
		os.Remove(tokensTmp)
		return fmt.Errorf("closing tokens.jsonl.tmp: %w", err)
	}

	// Emit keyset_counters.json
	countersPath := filepath.Join(outDir, "keyset_counters.json")
	var counters []KeysetCounterEntry
	for _, h := range healthEntries {
		counters = append(counters, KeysetCounterEntry{
			KeysetID: h.KeysetID,
			MintURL:  h.MintURL,
			Counter:  h.Counter,
		})
	}
	countersJSON, err := json.MarshalIndent(counters, "", "  ")
	if err != nil {
		os.Remove(tokensTmp)
		return fmt.Errorf("marshalling counters: %w", err)
	}
	if err := atomicWriteFile(countersPath, countersJSON); err != nil {
		os.Remove(tokensTmp)
		return fmt.Errorf("writing keyset_counters.json: %w", err)
	}

	// Emit migration-report.json
	report := MigrationReport{
		ExportedAt:  "", // caller can set
		Keysets:     healthEntries,
		TotalProofs: totalProofs,
		TotalAmount: totalAmount,
		HasSeed:     hasSeed,
	}
	reportJSON, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		os.Remove(tokensTmp)
		return fmt.Errorf("marshalling report: %w", err)
	}
	reportPath := filepath.Join(outDir, "migration-report.json")
	if err := atomicWriteFile(reportPath, reportJSON); err != nil {
		os.Remove(tokensTmp)
		return fmt.Errorf("writing migration-report.json: %w", err)
	}

	// Commit point: only now does the complete token set become visible.
	if err := os.Rename(tokensTmp, tokensPath); err != nil {
		os.Remove(tokensTmp)
		return fmt.Errorf("publishing tokens.jsonl: %w", err)
	}
	// Best-effort durability of the directory entries; the artifacts are
	// already complete and visible, so a failure here is only a warning.
	if err := syncDir(outDir); err != nil {
		fmt.Fprintf(os.Stderr, "warning: fsync of %s failed: %v\n", outDir, err)
	}

	fmt.Printf("Export complete: %d proofs, %d sats across %d keysets\n",
		totalProofs, totalAmount, len(healthEntries))
	fmt.Printf("  tokens.jsonl → %s\n", tokensPath)
	fmt.Printf("  keyset_counters.json → %s\n", countersPath)
	fmt.Printf("  migration-report.json → %s\n", reportPath)

	return nil
}

// atomicWriteFile writes data via a temp file in the same directory and
// renames it into place only after a successful fsync, so a crash leaves
// either the complete previous content or the complete new content.
func atomicWriteFile(path string, data []byte) error {
	tmp := path + ".tmp"
	f, err := os.Create(tmp)
	if err != nil {
		return err
	}
	if _, err := f.Write(data); err != nil {
		f.Close()
		os.Remove(tmp)
		return err
	}
	if err := f.Sync(); err != nil {
		f.Close()
		os.Remove(tmp)
		return err
	}
	if err := f.Close(); err != nil {
		os.Remove(tmp)
		return err
	}
	return os.Rename(tmp, path)
}

func syncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer d.Close()
	return d.Sync()
}

// loadKeysets reads the nested keysets bucket structure.
func loadKeysets(db *bolt.DB) (KeysetsMap, error) {
	keysets := make(KeysetsMap)

	err := db.View(func(tx *bolt.Tx) error {
		keysetsb := tx.Bucket([]byte(KEYSETS_BUCKET))
		if keysetsb == nil {
			return nil // no keysets bucket
		}

		return keysetsb.ForEach(func(mintURL, _ []byte) error {
			mintBucket := keysetsb.Bucket(mintURL)
			if mintBucket == nil {
				return nil
			}

			var mintKeysets []WalletKeyset
			c := mintBucket.Cursor()
			for k, v := c.First(); k != nil; k, v = c.Next() {
				var ks WalletKeyset
				if err := json.Unmarshal(v, &ks); err != nil {
					return fmt.Errorf("unmarshalling keyset %s: %w", string(k), err)
				}
				mintKeysets = append(mintKeysets, ks)
			}
			keysets[string(mintURL)] = mintKeysets
			return nil
		})
	})

	return keysets, err
}

// loadProofs reads the proofs bucket and groups by keyset ID.
func loadProofs(db *bolt.DB) (map[string][]Proof, error) {
	byKeyset := make(map[string][]Proof)

	err := db.View(func(tx *bolt.Tx) error {
		proofsb := tx.Bucket([]byte(PROOFS_BUCKET))
		if proofsb == nil {
			return nil // no proofs bucket
		}

		c := proofsb.Cursor()
		for k, v := c.First(); k != nil; k, v = c.Next() {
			var p Proof
			if err := json.Unmarshal(v, &p); err != nil {
				// Skip malformed proofs (matches gonuts behavior)
				continue
			}
			byKeyset[p.Id] = append(byKeyset[p.Id], p)
		}
		return nil
	})

	return byKeyset, err
}

// helper: hex-decode a string, returning empty slice on error.
func mustHex(s string) []byte {
	b, err := hex.DecodeString(s)
	if err != nil {
		return nil
	}
	return b
}