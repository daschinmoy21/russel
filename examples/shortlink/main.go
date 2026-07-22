// shortlink: minimal in-memory URL shortener for Russel demos.
// POST /  body=https://example.com  → {"id":"abc","url":"..."}
// GET /{id} → 302 redirect
// GET /health → ok
package main

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
)

func main() {
	port := 3000
	if v := os.Getenv("PORT"); v != "" {
		if p, err := strconv.Atoi(v); err == nil && p > 0 && p < 65536 {
			port = p
		}
	}

	store := &sync.Map{}

	mux := http.NewServeMux()
	mux.HandleFunc("/health", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/plain")
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodPost && r.URL.Path == "/":
			body, err := io.ReadAll(io.LimitReader(r.Body, 4096))
			if err != nil {
				http.Error(w, "read body", http.StatusBadRequest)
				return
			}
			url := strings.TrimSpace(string(body))
			if url == "" || !strings.HasPrefix(url, "http") {
				http.Error(w, "body must be an http(s) URL", http.StatusBadRequest)
				return
			}
			id := randomID()
			store.Store(id, url)
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(map[string]string{"id": id, "url": url})
		case r.Method == http.MethodGet && r.URL.Path != "/":
			id := strings.TrimPrefix(r.URL.Path, "/")
			if v, ok := store.Load(id); ok {
				http.Redirect(w, r, v.(string), http.StatusFound)
				return
			}
			http.NotFound(w, r)
		case r.Method == http.MethodGet && r.URL.Path == "/":
			w.Header().Set("Content-Type", "text/plain")
			fmt.Fprintln(w, "shortlink: POST / with URL body; GET /{id} redirects")
		default:
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		}
	})

	addr := fmt.Sprintf(":%d", port)
	log.Printf("shortlink listening on %s", addr)
	log.Fatal(http.ListenAndServe(addr, mux))
}

func randomID() string {
	var b [4]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}
