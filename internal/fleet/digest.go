package fleet

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"io/fs"
)

// Digest hashes fleet.yaml and everything under repos/. It is stamped into the lock so that
// a fixture edited without re-running apply is caught before anything is reset against a
// stale baseline.
func Digest(fsys fs.FS) (string, error) {
	h := sha256.New()
	add := func(p string, exec bool) error {
		f, err := fsys.Open(p)
		if err != nil {
			return err
		}
		defer f.Close()
		// Path, the executable bit and a length-delimited body: everything that reaches
		// the git tree, and nothing a checkout can change on its own.
		info, err := f.Stat()
		if err != nil {
			return err
		}
		fmt.Fprintf(h, "%s\x00%t\x00%d\x00", p, exec, info.Size())
		_, err = io.Copy(h, f)
		return err
	}

	if err := add(SpecFile, false); err != nil {
		return "", err
	}
	// WalkDir visits in lexical order, so the digest is stable.
	err := fs.WalkDir(fsys, ReposDir, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() || IsOSJunk(d.Name()) {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		return add(p, info.Mode()&0o111 != 0)
	})
	if err != nil {
		return "", err
	}
	return "sha256:" + hex.EncodeToString(h.Sum(nil)), nil
}

// IsOSJunk reports files that operating systems scatter through directories and that must
// never reach a fixture commit, since their appearance would change a repository's SHA.
func IsOSJunk(name string) bool {
	switch name {
	case ".DS_Store", "Thumbs.db", "desktop.ini":
		return true
	}
	return false
}
