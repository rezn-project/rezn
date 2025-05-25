package reznstore

import (
	"log"

	"github.com/dgraph-io/badger/v4"
)

type Store struct {
	db *badger.DB
}

func New(path string) *Store {
	opts := badger.DefaultOptions(path)
	opts.Logger = nil // shut up, badger
	db, err := badger.Open(opts)
	if err != nil {
		log.Fatalf("Failed to open badger store: %v", err)
	}
	return &Store{db: db}
}

func (s *Store) Write(key string, data []byte) error {
	return s.db.Update(func(txn *badger.Txn) error {
		return txn.Set([]byte(key), data)
	})
}

func (s *Store) Read(key string) ([]byte, error) {
	var valCopy []byte
	err := s.db.View(func(txn *badger.Txn) error {
		item, err := txn.Get([]byte(key))
		if err != nil {
			return err
		}
		valCopy, err = item.ValueCopy(nil)
		return err
	})
	return valCopy, err
}

func (s *Store) Close() {
	s.db.Close()
}
